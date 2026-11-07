//! Tests for the real Immich client, against a pretend Immich (wiremock)
//! running on a random local port. No real photos are involved.

use chrono::{TimeZone, Utc};
use photo_clearout::immich::{ImageSize, ImmichClient, PhotoSource, SourceError};
use serde_json::json;
use wiremock::matchers::{body_json, body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "test-api-key";
const ID: &str = "11111111-2222-3333-4444-555555555555";

fn client(server: &MockServer) -> ImmichClient {
    ImmichClient::new(&server.uri(), KEY).unwrap()
}

fn search_reply(items: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "albums": { "items": [], "total": 0, "count": 0, "facets": [] },
        "assets": { "items": items, "total": 0, "count": 0, "facets": [], "nextPage": null }
    }))
}

#[tokio::test]
async fn search_sends_key_and_asks_oldest_first() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/search/metadata"))
        .and(header("x-api-key", KEY))
        .and(body_partial_json(json!({
            "order": "asc",
            "withExif": true,
            "size": 10,
            "takenAfter": "2009-05-01T12:00:00.000Z"
        })))
        .respond_with(search_reply(json!([])))
        .expect(1)
        .mount(&server)
        .await;

    let after = Utc.with_ymd_and_hms(2009, 5, 1, 12, 0, 0).unwrap();
    let found = client(&server).assets_taken_after(Some(after), 10).await.unwrap();
    assert!(found.assets.is_empty());
}

#[tokio::test]
async fn search_maps_fields_and_skips_trashed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/search/metadata"))
        .respond_with(search_reply(json!([
            {
                "id": ID,
                "fileCreatedAt": "2009-05-01T12:00:00.000Z",
                "localDateTime": "2009-05-01T14:00:00.000Z",
                "originalFileName": "IMG_0001.HEIC",
                "type": "IMAGE",
                "isTrashed": false,
                "exifInfo": { "city": "Paris", "state": "Île-de-France", "country": "France" },
                "someFieldWeDontUse": 42
            },
            {
                "id": "99999999-2222-3333-4444-555555555555",
                "fileCreatedAt": "2009-05-02T12:00:00.000Z",
                "originalFileName": "MOV_0002.MOV",
                "type": "VIDEO",
                "isTrashed": false,
                "exifInfo": { "city": null, "state": null, "country": null }
            },
            {
                "id": "88888888-2222-3333-4444-555555555555",
                "fileCreatedAt": "2009-05-03T12:00:00.000Z",
                "originalFileName": "trashed.jpg",
                "type": "IMAGE",
                "isTrashed": true
            }
        ])))
        .mount(&server)
        .await;

    let found = client(&server).assets_taken_after(None, 10).await.unwrap();
    assert_eq!(found.returned, 3, "counts what Immich sent, so paging knows if there may be more");
    let assets = found.assets;
    assert_eq!(assets.len(), 2, "trashed photo must be left out");

    assert_eq!(assets[0].id, ID);
    assert_eq!(assets[0].file_name, "IMG_0001.HEIC");
    assert_eq!(assets[0].place.as_deref(), Some("Paris, France"));
    assert_eq!(assets[0].local_date_time.as_deref(), Some("2009-05-01T14:00:00.000Z"));
    assert!(!assets[0].is_video);

    assert!(assets[1].is_video);
    assert_eq!(assets[1].place, None);
}

#[tokio::test]
async fn first_search_has_no_date_filter() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/search/metadata"))
        .respond_with(search_reply(json!([])))
        .mount(&server)
        .await;

    client(&server).assets_taken_after(None, 5).await.unwrap();

    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(body.get("takenAfter").is_none());
}

#[tokio::test]
async fn wrong_key_is_reported_as_refused() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let err = client(&server).assets_taken_after(None, 1).await.unwrap_err();
    assert!(matches!(err, SourceError::Refused(401)));
}

#[tokio::test]
async fn server_error_is_reported_with_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;

    let err = client(&server).assets_taken_after(None, 1).await.unwrap_err();
    assert!(matches!(err, SourceError::Http { status: 500, .. }));
}

#[tokio::test]
async fn unreachable_immich_is_reported_clearly() {
    // Nothing listens on port 9 (the "discard" port) locally.
    let c = ImmichClient::new("http://127.0.0.1:9", KEY).unwrap();
    let err = c.assets_taken_after(None, 1).await.unwrap_err();
    assert!(matches!(err, SourceError::Unreachable(_)));
}

#[tokio::test]
async fn image_is_fetched_with_the_right_size() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/assets/{ID}/thumbnail")))
        .and(query_param("size", "preview"))
        .and(header("x-api-key", KEY))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(vec![1u8, 2, 3], "image/webp"),
        )
        .mount(&server)
        .await;

    let img = client(&server).image(ID, ImageSize::Preview).await.unwrap();
    assert_eq!(img.content_type, "image/webp");
    assert_eq!(img.bytes, vec![1, 2, 3]);
}

#[tokio::test]
async fn non_image_response_is_refused() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("<html>", "text/html"))
        .mount(&server)
        .await;

    let err = client(&server).image(ID, ImageSize::Thumbnail).await.unwrap_err();
    assert!(matches!(err, SourceError::BadResponse(_)));
}

#[tokio::test]
async fn trash_moves_to_trash_and_never_force_deletes() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/api/assets"))
        .and(header("x-api-key", KEY))
        .and(body_json(json!({ "ids": [ID], "force": false })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    client(&server).trash(&[ID.to_string()]).await.unwrap();
}

#[tokio::test]
async fn base_url_with_a_path_is_kept() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/immich/api/search/metadata"))
        .respond_with(search_reply(json!([])))
        .expect(1)
        .mount(&server)
        .await;

    let c = ImmichClient::new(&format!("{}/immich", server.uri()), KEY).unwrap();
    c.assets_taken_after(None, 1).await.unwrap();
}

#[tokio::test]
async fn restore_asks_immich_to_restore_from_trash() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/trash/restore/assets"))
        .and(header("x-api-key", KEY))
        .and(body_json(json!({ "ids": [ID] })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "count": 1 })))
        .expect(1)
        .mount(&server)
        .await;

    client(&server).restore(&[ID.to_string()]).await.unwrap();
}

#[tokio::test]
async fn is_trashed_reads_the_asset() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/assets/{ID}")))
        .and(header("x-api-key", KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": ID, "isTrashed": true })))
        .mount(&server)
        .await;

    assert!(client(&server).is_trashed(ID).await.unwrap());
}

#[tokio::test]
async fn redirects_are_never_followed() {
    // A redirect elsewhere would carry the API key (a custom header) along.
    let elsewhere = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(search_reply(json!([])))
        .expect(0)
        .mount(&elsewhere)
        .await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/search/metadata"))
        .respond_with(
            ResponseTemplate::new(307).insert_header("location", format!("{}/api/search/metadata", elsewhere.uri())),
        )
        .mount(&server)
        .await;

    let err = client(&server).assets_taken_after(None, 1).await.unwrap_err();
    assert!(matches!(err, SourceError::BadResponse(_)), "{err}");
}

#[tokio::test]
async fn svg_and_other_non_photo_images_are_refused() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("<svg/>", "image/svg+xml"))
        .mount(&server)
        .await;

    let err = client(&server).image(ID, ImageSize::Thumbnail).await.unwrap_err();
    assert!(matches!(err, SourceError::BadResponse(_)));
}

#[tokio::test]
async fn image_type_with_parameters_is_accepted() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(vec![1u8], "image/jpeg; charset=binary"))
        .mount(&server)
        .await;

    let img = client(&server).image(ID, ImageSize::Thumbnail).await.unwrap();
    assert_eq!(img.content_type, "image/jpeg");
}

#[tokio::test]
async fn oversized_image_is_refused() {
    let server = MockServer::start().await;
    let big = vec![0u8; photo_clearout::immich::MAX_IMAGE_BYTES + 1];
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(big, "image/jpeg"))
        .mount(&server)
        .await;

    let err = client(&server).image(ID, ImageSize::Preview).await.unwrap_err();
    assert!(matches!(err, SourceError::BadResponse(_)));
}

#[tokio::test]
async fn invalid_ids_never_reach_immich() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let c = client(&server);
    let bad = "../users/me".to_string();
    assert!(matches!(c.is_trashed(&bad).await, Err(SourceError::InvalidId(_))));
    assert!(matches!(c.image(&bad, ImageSize::Preview).await, Err(SourceError::InvalidId(_))));
    assert!(matches!(c.trash(std::slice::from_ref(&bad)).await, Err(SourceError::InvalidId(_))));
    assert!(matches!(c.restore(&[bad]).await, Err(SourceError::InvalidId(_))));
}

#[tokio::test]
async fn permission_check_sends_empty_lists_only() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/api/assets"))
        .and(header("x-api-key", KEY))
        .and(body_json(json!({ "ids": [], "force": false })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/trash/restore/assets"))
        .and(body_json(json!({ "ids": [] })))
        // Immich may reject an empty list; that still means the key got
        // past the permission check.
        .respond_with(ResponseTemplate::new(400))
        .expect(1)
        .mount(&server)
        .await;

    client(&server).check_can_trash_and_restore().await.unwrap();
}

#[tokio::test]
async fn permission_check_reports_a_missing_permission() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/api/assets"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/trash/restore/assets"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;

    let err = client(&server).check_can_trash_and_restore().await.unwrap_err();
    assert!(matches!(err, SourceError::MissingPermission(_, 403)), "{err}");
}

#[tokio::test]
async fn a_photo_immich_no_longer_has_counts_as_not_found() {
    // Immich answers 400 ("not found or no access"), not 404.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/assets/{ID}")))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({ "message": "Not found or no asset.read access" })))
        .mount(&server)
        .await;

    assert!(matches!(client(&server).is_trashed(ID).await, Err(SourceError::NotFound)));
}
