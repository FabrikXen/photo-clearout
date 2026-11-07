//! Tests for the local web server, using an in-memory fake photo library.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use chrono::{DateTime, TimeZone, Utc};
use http_body_util::BodyExt;
use photo_clearout::immich::{Asset, Found, Image, ImageSize, PhotoSource, SourceError};
use photo_clearout::paging::Cursor;
use photo_clearout::progress::{Marked, Trashed};
use photo_clearout::web::{self, AppState, TOKEN_HEADER};
use serde_json::{json, Value};
use tower::ServiceExt;

const TOKEN: &str = "test-token";
const PORT: u16 = 3000;
const HOST: &str = "127.0.0.1:3000";

fn id(n: u32) -> String {
    format!("00000000-0000-0000-0000-{n:012}")
}

/// A pretend Immich: a list of photos in memory, one per day from 2009.
struct FakeLibrary {
    assets: Mutex<Vec<Asset>>,
    trashed: Mutex<Vec<String>>,
    /// How many trash requests reached "Immich".
    trash_calls: Mutex<usize>,
    /// Make the Nth trash request (0-based) fail, to test partial failure.
    fail_trash_call: Mutex<Option<usize>>,
    /// Make every trash request take this long, to test the browser closing mid-way.
    trash_delay: Mutex<Option<std::time::Duration>>,
    /// Make the Nth trash request (0-based) trash its photos but then time
    /// out, as if Immich did the work and the reply was lost.
    timeout_after_trash_call: Mutex<Option<usize>>,
    /// Make `is_trashed` fail, as if Immich can't be reached.
    is_trashed_fails: Mutex<bool>,
    /// Make trash requests panic, to test that shutdown still works.
    trash_panics: Mutex<bool>,
}

impl FakeLibrary {
    fn with_photos(n: u32) -> Arc<Self> {
        let assets = (0..n)
            .map(|i| Asset {
                id: id(i),
                taken_at: Utc.with_ymd_and_hms(2009, 1, 1, 12, 0, 0).unwrap()
                    + chrono::Duration::days(i as i64),
                local_date_time: None,
                file_name: format!("IMG_{i:04}.jpg"),
                is_video: false,
                place: None,
            })
            .collect();
        Arc::new(Self {
            assets: Mutex::new(assets),
            trashed: Mutex::new(vec![]),
            trash_calls: Mutex::new(0),
            fail_trash_call: Mutex::new(None),
            trash_delay: Mutex::new(None),
            timeout_after_trash_call: Mutex::new(None),
            is_trashed_fails: Mutex::new(false),
            trash_panics: Mutex::new(false),
        })
    }

    fn all_exist(&self, ids: &[String]) -> bool {
        let assets = self.assets.lock().unwrap();
        ids.iter().all(|id| assets.iter().any(|a| &a.id == id))
    }

    /// Delete a photo for good, as if done in Immich itself (or its trash
    /// was emptied).
    fn purge(&self, id: &str) {
        self.assets.lock().unwrap().retain(|a| a.id != id);
        self.trashed.lock().unwrap().retain(|t| t != id);
    }

    /// A library where `n` photos share the exact same timestamp (a burst).
    fn with_burst(n: u32) -> Arc<Self> {
        let lib = Self::with_photos(n);
        let t = Utc.with_ymd_and_hms(2009, 1, 1, 12, 0, 0).unwrap();
        for a in lib.assets.lock().unwrap().iter_mut() {
            a.taken_at = t;
        }
        lib
    }
}

#[async_trait]
impl PhotoSource for FakeLibrary {
    async fn assets_taken_after(
        &self,
        after: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Found, SourceError> {
        let trashed = self.trashed.lock().unwrap().clone();
        let assets: Vec<Asset> = self
            .assets
            .lock()
            .unwrap()
            .iter()
            .filter(|a| after.is_none_or(|t| a.taken_at >= t))
            .filter(|a| !trashed.contains(&a.id))
            .take(limit)
            .cloned()
            .collect();
        Ok(Found { returned: assets.len(), assets })
    }

    async fn image(&self, id: &str, _size: ImageSize) -> Result<Image, SourceError> {
        if self.assets.lock().unwrap().iter().any(|a| a.id == id) {
            // A tiny made-up "image": the test only checks it's passed through.
            Ok(Image { content_type: "image/webp".into(), bytes: vec![0xAB, 0xCD] })
        } else {
            Err(SourceError::NotFound)
        }
    }

    async fn trash(&self, ids: &[String]) -> Result<(), SourceError> {
        let call = {
            let mut calls = self.trash_calls.lock().unwrap();
            *calls += 1;
            *calls - 1
        };
        if *self.trash_panics.lock().unwrap() {
            panic!("pretend bug in a trash job");
        }
        if *self.fail_trash_call.lock().unwrap() == Some(call) {
            return Err(SourceError::Http { status: 500, body: "boom".into() });
        }
        let delay = *self.trash_delay.lock().unwrap();
        if let Some(d) = delay {
            tokio::time::sleep(d).await;
        }
        // Like real Immich: one photo that doesn't exist refuses the lot.
        if !self.all_exist(ids) {
            return Err(SourceError::Http { status: 400, body: "Not found or no asset.delete access".into() });
        }
        self.trashed.lock().unwrap().extend(ids.iter().cloned());
        if *self.timeout_after_trash_call.lock().unwrap() == Some(call) {
            return Err(SourceError::Unreachable("operation timed out".into()));
        }
        Ok(())
    }

    async fn restore(&self, ids: &[String]) -> Result<(), SourceError> {
        if !self.all_exist(ids) {
            return Err(SourceError::Http { status: 400, body: "Not found or no asset.delete access".into() });
        }
        self.trashed.lock().unwrap().retain(|t| !ids.contains(t));
        Ok(())
    }

    async fn is_trashed(&self, id: &str) -> Result<bool, SourceError> {
        if *self.is_trashed_fails.lock().unwrap() {
            return Err(SourceError::Unreachable("connection refused".into()));
        }
        if !self.all_exist(std::slice::from_ref(&id.to_string())) {
            return Err(SourceError::NotFound);
        }
        Ok(self.trashed.lock().unwrap().iter().any(|t| t == id))
    }
}

struct Harness {
    app: Router,
    state: AppState,
    library: Arc<FakeLibrary>,
    progress_path: std::path::PathBuf,
    settings_path: std::path::PathBuf,
    marks_path: std::path::PathBuf,
    trashed_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn harness(photos: u32) -> Harness {
    harness_with(FakeLibrary::with_photos(photos))
}

fn harness_with(library: Arc<FakeLibrary>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(library.clone(), dir.path(), TOKEN.into(), PORT);
    Harness {
        app: web::router(state.clone()),
        state,
        library,
        progress_path: dir.path().join("progress.json"),
        settings_path: dir.path().join("settings.json"),
        marks_path: dir.path().join("marked.json"),
        trashed_path: dir.path().join("trashed.json"),
        _dir: dir,
    }
}

fn post(path: &str, body: Value) -> Request<Body> {
    Request::post(path)
        .header("host", HOST)
        .header("content-type", "application/json")
        .header(TOKEN_HEADER, TOKEN)
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(path: &str) -> Request<Body> {
    Request::get(path).header("host", HOST).body(Body::empty()).unwrap()
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, bytes)
}

async fn send_json(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let (status, bytes) = send(app, req).await;
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn page_ids(body: &Value) -> Vec<String> {
    body["items"].as_array().unwrap().iter().map(|a| a["id"].as_str().unwrap().to_string()).collect()
}

// ---- The page itself ----

#[tokio::test]
async fn index_contains_the_token_and_security_headers() {
    let h = harness(0);
    let resp = h.app.clone().oneshot(get("/")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers()["content-security-policy"].to_str().unwrap().contains("default-src 'none'"));
    assert_eq!(resp.headers()["x-frame-options"], "DENY");
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains(&format!("content=\"{TOKEN}\"")));
    assert!(!html.contains("__CLEAROUT_TOKEN__"));
}

// ---- Security checks ----

#[tokio::test]
async fn wrong_host_is_refused() {
    // Defends against DNS rebinding: a website pointing its own name at 127.0.0.1.
    let h = harness(3);
    let req = Request::get("/").header("host", "evil.example:3000").body(Body::empty()).unwrap();
    let (status, _) = send(&h.app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn post_without_token_is_refused() {
    let h = harness(3);
    let req = Request::post("/api/end-session")
        .header("host", HOST)
        .header("content-type", "application/json")
        .body(Body::from(json!({ "ids": [id(0)] }).to_string()))
        .unwrap();
    let (status, _) = send(&h.app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(h.library.trashed.lock().unwrap().is_empty(), "nothing may be trashed");
}

#[tokio::test]
async fn post_with_wrong_token_is_refused() {
    let h = harness(3);
    let req = Request::post("/api/end-session")
        .header("host", HOST)
        .header("content-type", "application/json")
        .header(TOKEN_HEADER, "guess")
        .body(Body::from(json!({ "ids": [id(0)] }).to_string()))
        .unwrap();
    let (status, _) = send(&h.app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(h.library.trashed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn foreign_origin_is_refused_even_with_token() {
    let h = harness(3);
    let mut req = post("/api/end-session", json!({ "ids": [id(0)] }));
    req.headers_mut().insert("origin", "https://evil.example".parse().unwrap());
    let (status, _) = send(&h.app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(h.library.trashed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn own_origin_is_allowed() {
    let h = harness(3);
    let mut req = post("/api/page", json!({ "size": 2 }));
    req.headers_mut().insert("origin", "http://127.0.0.1:3000".parse().unwrap());
    let (status, _) = send(&h.app, req).await;
    assert_eq!(status, StatusCode::OK);
}

// ---- Paging and progress ----

#[tokio::test]
async fn pages_go_oldest_first_and_link_together() {
    let h = harness(5);

    let (status, p1) = send_json(&h.app, post("/api/page", json!({ "size": 2 }))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page_ids(&p1), [id(0), id(1)]);

    let (_, p2) = send_json(&h.app, post("/api/page", json!({ "size": 2, "cursor": p1["next"] }))).await;
    assert_eq!(page_ids(&p2), [id(2), id(3)]);

    let (_, p3) = send_json(&h.app, post("/api/page", json!({ "size": 2, "cursor": p2["next"] }))).await;
    assert_eq!(page_ids(&p3), [id(4)]);
    assert!(p3["next"].is_null(), "last page has no next");
}

#[tokio::test]
async fn progress_is_saved_and_resumed() {
    let h = harness(5);
    let (_, p1) = send_json(&h.app, post("/api/page", json!({ "size": 2 }))).await;
    send_json(&h.app, post("/api/page", json!({ "size": 2, "cursor": p1["next"] }))).await;

    // A fresh session asks with no cursor and should land on page 2.
    let (_, resumed) = send_json(&h.app, post("/api/page", json!({ "size": 2 }))).await;
    assert_eq!(page_ids(&resumed), [id(2), id(3)]);

    let saved: Cursor = serde_json::from_str(&std::fs::read_to_string(&h.progress_path).unwrap()).unwrap();
    assert!(saved.after.is_some());
}

#[tokio::test]
async fn reset_starts_again_from_the_oldest() {
    let h = harness(5);
    let (_, p1) = send_json(&h.app, post("/api/page", json!({ "size": 2 }))).await;
    send_json(&h.app, post("/api/page", json!({ "size": 2, "cursor": p1["next"] }))).await;

    let (status, _) = send_json(&h.app, post("/api/reset", json!({}))).await;
    assert_eq!(status, StatusCode::OK);

    let (_, page) = send_json(&h.app, post("/api/page", json!({ "size": 2 }))).await;
    assert_eq!(page_ids(&page), [id(0), id(1)]);
}

#[tokio::test]
async fn page_size_limits_are_enforced() {
    let h = harness(3);
    for size in [0, web::MAX_PAGE_SIZE + 1] {
        let (status, _) = send_json(&h.app, post("/api/page", json!({ "size": size }))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "size {size}");
    }
}

// ---- Marking and ending the session ----

async fn page_assets(app: &Router, size: usize) -> Vec<Value> {
    let (_, body) = send_json(app, post("/api/page", json!({ "size": size }))).await;
    body["items"].as_array().unwrap().clone()
}

async fn mark(app: &Router, assets: &[Value]) -> (StatusCode, Value) {
    send_json(app, post("/api/marks", json!({ "add": assets }))).await
}

fn marked_ids(body: &Value) -> Vec<String> {
    body["marked"].as_array().unwrap().iter().map(|a| a["id"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn marking_saves_to_the_state_folder_and_deletes_nothing() {
    let h = harness(5);
    let assets = page_assets(&h.app, 3).await;

    let (status, body) = mark(&h.app, &assets[..2]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(marked_ids(&body), [id(0), id(1)]);
    assert!(h.marks_path.exists(), "marks survive a crash");
    assert!(h.library.trashed.lock().unwrap().is_empty(), "marking never deletes");

    // Unmark one, and marking the same photo twice keeps one copy.
    send_json(&h.app, post("/api/marks", json!({ "remove": [id(0)] }))).await;
    let (_, body) = mark(&h.app, &assets[1..2]).await;
    assert_eq!(marked_ids(&body), [id(1)]);

    // An empty request just lists the marks (what the page does on start-up).
    let (_, body) = send_json(&h.app, post("/api/marks", json!({}))).await;
    assert_eq!(marked_ids(&body), [id(1)]);
}

#[tokio::test]
async fn marks_reject_bad_ids() {
    let h = harness(3);
    let (status, _) = send_json(&h.app, post("/api/marks", json!({ "remove": ["../users/me"] }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let mut fake = page_assets(&h.app, 1).await;
    fake[0]["id"] = json!("not-a-uuid");
    let (status, _) = mark(&h.app, &fake).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!h.marks_path.exists());
}

fn trashed_ids(body: &Value) -> Vec<String> {
    body["trashed"].as_array().unwrap().iter().map(|a| a["id"].as_str().unwrap().to_string()).collect()
}

async fn stopped_within(h: &Harness, ms: u64) -> bool {
    tokio::time::timeout(std::time::Duration::from_millis(ms), h.state.finished()).await.is_ok()
}

#[tokio::test]
async fn end_session_trashes_reviewed_photos_and_keeps_running_for_undo() {
    let h = harness(5);
    let assets = page_assets(&h.app, 5).await;
    mark(&h.app, &assets[..3]).await;

    // On the review screen photo 1 was kept, so 0 and 2 are sent to trash.
    let (status, body) =
        send_json(&h.app, post("/api/end-session", json!({ "ids": [id(0), id(2)], "keep": [id(1)] }))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["changed"], 2);
    assert!(marked_ids(&body).is_empty(), "kept photo is unmarked too");
    assert_eq!(trashed_ids(&body), [id(0), id(2)], "listed for undo");

    let mut trashed = h.library.trashed.lock().unwrap().clone();
    trashed.sort();
    assert_eq!(trashed, [id(0), id(2)]);

    assert!(!stopped_within(&h, 100).await, "keeps running until Finish");
}

#[tokio::test]
async fn trash_marked_on_page_trashes_only_those() {
    let h = harness(5);
    let assets = page_assets(&h.app, 5).await;
    mark(&h.app, &assets[..3]).await;

    let (status, body) = send_json(&h.app, post("/api/trash-marked", json!({ "ids": [id(1)] }))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["changed"], 1);
    assert_eq!(marked_ids(&body), [id(0), id(2)], "other marks stay");
    assert_eq!(trashed_ids(&body), [id(1)]);
    assert_eq!(*h.library.trashed.lock().unwrap(), vec![id(1)]);
}

#[tokio::test]
async fn trash_marked_refuses_unmarked_photos() {
    let h = harness(3);
    let (status, _) = send_json(&h.app, post("/api/trash-marked", json!({ "ids": [id(0)] }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(h.library.trashed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn undo_restores_and_removes_from_the_list() {
    let h = harness(5);
    let assets = page_assets(&h.app, 5).await;
    mark(&h.app, &assets[..2]).await;
    send_json(&h.app, post("/api/trash-marked", json!({ "ids": [id(0), id(1)] }))).await;

    let (status, body) = send_json(&h.app, post("/api/restore", json!({ "ids": [id(0)] }))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["changed"], 1);
    assert_eq!(trashed_ids(&body), [id(1)]);
    assert_eq!(*h.library.trashed.lock().unwrap(), vec![id(1)], "photo 0 is back in Immich");

    // The restored photo shows up in the grid again.
    let (_, page) = send_json(&h.app, post("/api/page", json!({ "size": 5, "cursor": null }))).await;
    assert!(page_ids(&page).contains(&id(0)));
}

#[tokio::test]
async fn undo_refuses_photos_not_trashed_this_session() {
    let h = harness(3);
    let (status, _) = send_json(&h.app, post("/api/restore", json!({ "ids": [id(0)] }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn finish_clears_the_undo_list_and_stops() {
    let h = harness(3);
    let assets = page_assets(&h.app, 3).await;
    mark(&h.app, &assets[..1]).await;
    send_json(&h.app, post("/api/trash-marked", json!({ "ids": [id(0)] }))).await;

    let (status, _) = send_json(&h.app, post("/api/finish", json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(stopped_within(&h, 1000).await, "Finish stops the tool");

    let log: Trashed = serde_json::from_str(&std::fs::read_to_string(&h.trashed_path).unwrap()).unwrap();
    assert!(log.assets.is_empty());
    assert_eq!(*h.library.trashed.lock().unwrap(), vec![id(0)], "photo stays in Immich's trash");
}

#[tokio::test]
async fn browser_closing_mid_trash_still_finishes_the_job() {
    let h = harness(3);
    let assets = page_assets(&h.app, 3).await;
    mark(&h.app, &assets[..2]).await;
    *h.library.trash_delay.lock().unwrap() = Some(std::time::Duration::from_millis(200));

    // Start trashing, then "close the browser" by dropping the request.
    let app = h.app.clone();
    let req = post("/api/trash-marked", json!({ "ids": [id(0), id(1)] }));
    let in_flight = tokio::spawn(async move { app.oneshot(req).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    in_flight.abort();

    // The job runs on and the tool waits for it before it may exit.
    tokio::time::timeout(std::time::Duration::from_secs(2), h.state.jobs_done()).await.unwrap();

    let mut trashed = h.library.trashed.lock().unwrap().clone();
    trashed.sort();
    assert_eq!(trashed, [id(0), id(1)]);
    let marks: Marked = serde_json::from_str(&std::fs::read_to_string(&h.marks_path).unwrap()).unwrap();
    assert!(marks.assets.is_empty(), "saved state matches what Immich did");
    let log: Trashed = serde_json::from_str(&std::fs::read_to_string(&h.trashed_path).unwrap()).unwrap();
    assert_eq!(log.assets.len(), 2, "both listed for undo");
}

#[tokio::test]
async fn crash_mid_trash_is_repaired_on_next_start() {
    let h = harness(3);
    let assets = page_assets(&h.app, 3).await;
    mark(&h.app, &assets[..2]).await;

    // Simulate a crash after the write-ahead step: both photos are marked
    // *and* on the undo list, but Immich only got as far as photo 0.
    let in_flight: Vec<_> = assets[..2].iter().map(|a| serde_json::from_value(a.clone()).unwrap()).collect();
    std::fs::write(&h.trashed_path, serde_json::to_string(&Trashed { assets: in_flight }).unwrap()).unwrap();
    h.library.trashed.lock().unwrap().push(id(0));

    let report = h.state.recover().await.unwrap();
    assert_eq!((report.completed, report.not_done), (1, 1));

    let (_, body) = send_json(&h.app, post("/api/marks", json!({}))).await;
    assert_eq!(marked_ids(&body), [id(1)], "not trashed: still marked");
    assert_eq!(trashed_ids(&body), [id(0)], "trashed: on the undo list, unmarked");
}

#[tokio::test]
async fn idle_detection_fires_without_heartbeats() {
    let h = harness(0);
    let fired = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        h.state.idle_for(std::time::Duration::from_secs(1)),
    )
    .await;
    assert!(fired.is_ok(), "no page in touch for the limit means stop");
}

#[tokio::test]
async fn ping_needs_the_token() {
    let h = harness(0);
    let (status, _) = send_json(&h.app, post("/api/ping", json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    let req = Request::post("/api/ping").header("host", HOST).body(Body::empty()).unwrap();
    let (status, _) = send(&h.app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn end_session_refuses_photos_that_were_not_marked() {
    let h = harness(5);
    let assets = page_assets(&h.app, 5).await;
    mark(&h.app, &assets[..1]).await;

    let (status, _) = send_json(&h.app, post("/api/end-session", json!({ "ids": [id(0), id(3)] }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(h.library.trashed.lock().unwrap().is_empty(), "nothing trashed if any id is unmarked");
}

#[tokio::test]
async fn end_session_rejects_bad_ids_and_trashes_nothing() {
    let h = harness(3);
    let (status, _) = send_json(&h.app, post("/api/end-session", json!({ "ids": ["../users/me"] }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(h.library.trashed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ending_with_nothing_marked_trashes_nothing() {
    let h = harness(3);
    let (status, body) = send_json(&h.app, post("/api/end-session", json!({ "ids": [] }))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["changed"], 0);
    assert_eq!(*h.library.trash_calls.lock().unwrap(), 0);
}

#[tokio::test]
async fn large_sessions_are_trashed_in_batches() {
    let n = web::MAX_TRASH_BATCH as u32 + 20;
    let h = harness(n);
    let mut all = vec![];
    let mut cursor = Value::Null;
    loop {
        let (_, body) = send_json(&h.app, post("/api/page", json!({ "size": 200, "cursor": cursor }))).await;
        let items = body["items"].as_array().unwrap().clone();
        if items.is_empty() { break; }
        for chunk in items.chunks(200) {
            mark(&h.app, chunk).await;
        }
        all.extend(items);
        cursor = body["next"].clone();
        if cursor.is_null() { break; }
    }
    let ids: Vec<&str> = all.iter().map(|a| a["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), n as usize);

    let (status, body) = send_json(&h.app, post("/api/end-session", json!({ "ids": ids }))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["changed"], n);
    assert_eq!(*h.library.trash_calls.lock().unwrap(), 2, "500 + 20");
}

#[tokio::test]
async fn partial_failure_reports_exactly_and_keeps_the_rest_marked() {
    let n = web::MAX_TRASH_BATCH as u32 + 5;
    let h = harness(n);
    let mut all = vec![];
    let mut cursor = Value::Null;
    loop {
        let (_, body) = send_json(&h.app, post("/api/page", json!({ "size": 200, "cursor": cursor }))).await;
        let items = body["items"].as_array().unwrap().clone();
        for chunk in items.chunks(200) {
            mark(&h.app, chunk).await;
        }
        all.extend(items);
        cursor = body["next"].clone();
        if cursor.is_null() { break; }
    }
    let ids: Vec<&str> = all.iter().map(|a| a["id"].as_str().unwrap()).collect();

    *h.library.fail_trash_call.lock().unwrap() = Some(1); // second batch fails
    let (status, body) = send_json(&h.app, post("/api/end-session", json!({ "ids": ids }))).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let msg = body["error"].as_str().unwrap();
    assert!(msg.contains("moved 500 of 505"), "{msg}");

    // The 5 not trashed are still marked; the 500 trashed are not, and
    // only those 500 are on the undo list.
    let (_, body) = send_json(&h.app, post("/api/marks", json!({}))).await;
    assert_eq!(marked_ids(&body).len(), 5);
    assert_eq!(trashed_ids(&body).len(), 500);
}

// ---- Images ----

#[tokio::test]
async fn image_is_passed_through_and_never_cached() {
    let h = harness(3);
    let resp = h.app.clone().oneshot(get(&format!("/api/image/{}?size=preview", id(1)))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "image/webp");
    // Copies of photos must not be left in the browser's disk cache.
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.to_vec(), vec![0xAB, 0xCD]);
}

#[tokio::test]
async fn image_with_invalid_id_or_size_is_rejected() {
    let h = harness(3);
    let (status, _) = send(&h.app, get("/api/image/not-a-uuid")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = send(&h.app, get(&format!("/api/image/{}?size=original", id(1)))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn unknown_image_is_404() {
    let h = harness(3);
    let (status, _) = send(&h.app, get(&format!("/api/image/{}", id(999)))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---- Settings (grid size) ----

async fn index_html(app: &Router) -> String {
    let (_, bytes) = send(app, get("/")).await;
    String::from_utf8(bytes).unwrap()
}

#[tokio::test]
async fn grid_size_is_saved_in_the_state_folder_and_put_into_the_page() {
    let h = harness(0);
    assert!(index_html(&h.app).await.contains(r#"name="clearout-cols" content="""#), "no saved size yet");

    let (status, _) = send_json(&h.app, post("/api/settings", json!({ "cols": 7 }))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(h.settings_path.exists(), "saved to the state folder, not the browser");

    assert!(index_html(&h.app).await.contains(r#"name="clearout-cols" content="7""#));
}

#[tokio::test]
async fn invalid_grid_size_is_rejected() {
    let h = harness(0);
    for cols in [0, 13] {
        let (status, _) = send_json(&h.app, post("/api/settings", json!({ "cols": cols }))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "cols {cols}");
    }
    assert!(!h.settings_path.exists());
}

#[tokio::test]
async fn settings_need_the_token() {
    let h = harness(0);
    let req = Request::post("/api/settings")
        .header("host", HOST)
        .header("content-type", "application/json")
        .body(Body::from(json!({ "cols": 3 }).to_string()))
        .unwrap();
    let (status, _) = send(&h.app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---- Never running in the background ----

fn bye(token: &str) -> Request<Body> {
    // Like the browser's beacon: no token header, token in a text body.
    Request::post("/api/bye")
        .header("host", HOST)
        .header("content-type", "text/plain")
        .body(Body::from(json!({ "token": token }).to_string()))
        .unwrap()
}

async fn closed_within(h: &Harness, grace_ms: u64, wait_ms: u64) -> bool {
    tokio::time::timeout(
        std::time::Duration::from_millis(wait_ms),
        h.state.page_closed(std::time::Duration::from_millis(grace_ms)),
    )
    .await
    .is_ok()
}

#[tokio::test]
async fn closing_the_tab_stops_the_tool() {
    let h = harness(0);
    let (status, _) = send_json(&h.app, bye(TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(closed_within(&h, 200, 2000).await);
}

#[tokio::test]
async fn reloading_the_page_does_not_stop_the_tool() {
    let h = harness(0);
    send_json(&h.app, bye(TOKEN)).await;
    // The reloaded page is back straight away.
    send_json(&h.app, post("/api/ping", json!({}))).await;
    assert!(!closed_within(&h, 200, 800).await);
}

#[tokio::test]
async fn goodbye_needs_the_right_token() {
    let h = harness(0);
    let (status, _) = send_json(&h.app, bye("guess")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!closed_within(&h, 100, 600).await, "a stranger can't stop the tool");
}

// ---- Review fixes (2026-11) ----

#[tokio::test]
async fn end_session_leaves_marks_the_review_did_not_show() {
    let h = harness(5);
    let assets = page_assets(&h.app, 5).await;
    mark(&h.app, &assets[..3]).await;

    // The review showed 0 and 1 (trash 0, keep 1); photo 2 was marked
    // somewhere else after the review opened.
    let (status, body) =
        send_json(&h.app, post("/api/end-session", json!({ "ids": [id(0)], "keep": [id(1)] }))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(marked_ids(&body), [id(2)], "an unreviewed mark is kept");
}

#[tokio::test]
async fn marking_during_a_trash_waits_and_is_not_lost() {
    let h = harness(5);
    let assets = page_assets(&h.app, 5).await;
    mark(&h.app, &assets[..1]).await;
    *h.library.trash_delay.lock().unwrap() = Some(std::time::Duration::from_millis(300));

    let app = h.app.clone();
    let ending = tokio::spawn(async move {
        send_json(&app, post("/api/end-session", json!({ "ids": [id(0)], "keep": [] }))).await
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // Marked while the end-session job is still talking to Immich.
    let (status, _) = mark(&h.app, &assets[2..3]).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = ending.await.unwrap();
    assert_eq!(status, StatusCode::OK);

    let (_, body) = send_json(&h.app, post("/api/marks", json!({}))).await;
    assert_eq!(marked_ids(&body), [id(2)], "the new mark survives the end of the session");
}

#[tokio::test]
async fn timeout_after_immich_trashed_keeps_the_undo_list() {
    let h = harness(3);
    let assets = page_assets(&h.app, 3).await;
    mark(&h.app, &assets[..2]).await;
    // Immich trashes both, but the reply never arrives.
    *h.library.timeout_after_trash_call.lock().unwrap() = Some(0);

    let (status, body) = send_json(&h.app, post("/api/trash-marked", json!({ "ids": [id(0), id(1)] }))).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body["error"].as_str().unwrap().contains("moved 2 of 2"), "{body}");

    let (_, body) = send_json(&h.app, post("/api/marks", json!({}))).await;
    assert!(marked_ids(&body).is_empty(), "Immich did trash them, so they're unmarked");
    assert_eq!(trashed_ids(&body), [id(0), id(1)], "and still on the undo list");
}

#[tokio::test]
async fn unclear_trash_that_cannot_be_checked_is_settled_at_next_start() {
    let h = harness(3);
    let assets = page_assets(&h.app, 3).await;
    mark(&h.app, &assets[..1]).await;
    *h.library.timeout_after_trash_call.lock().unwrap() = Some(0);
    *h.library.is_trashed_fails.lock().unwrap() = true;

    let (status, body) = send_json(&h.app, post("/api/trash-marked", json!({ "ids": [id(0)] }))).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body["error"].as_str().unwrap().contains("could not be checked"), "{body}");

    // Left "in flight": marked and on the undo list.
    let (_, body) = send_json(&h.app, post("/api/marks", json!({}))).await;
    assert_eq!(marked_ids(&body), [id(0)]);
    assert_eq!(trashed_ids(&body), [id(0)]);

    // Next start, Immich can be asked again.
    *h.library.is_trashed_fails.lock().unwrap() = false;
    let report = h.state.recover().await.unwrap();
    assert_eq!((report.completed, report.not_done), (1, 0));
}

#[tokio::test]
async fn a_job_that_panics_does_not_block_shutdown() {
    let h = harness(3);
    let assets = page_assets(&h.app, 3).await;
    mark(&h.app, &assets[..1]).await;
    *h.library.trash_panics.lock().unwrap() = true;

    let (status, _) = send_json(&h.app, post("/api/trash-marked", json!({ "ids": [id(0)] }))).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    tokio::time::timeout(std::time::Duration::from_secs(2), h.state.jobs_done())
        .await
        .expect("the job count went back to 0");
}

#[tokio::test]
async fn a_burst_bigger_than_one_fetch_is_paged_through_completely() {
    // 1×1 grid: each fetch asks for 52 photos, but 60 share one timestamp.
    let n = 60;
    let h = harness_with(FakeLibrary::with_burst(n));
    let mut seen = std::collections::HashSet::new();
    let mut cursor = Value::Null;
    for _ in 0..(n + 5) {
        let (status, body) = send_json(&h.app, post("/api/page", json!({ "size": 1, "cursor": cursor }))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        for id in page_ids(&body) {
            assert!(seen.insert(id), "a photo was shown twice");
        }
        cursor = body["next"].clone();
        if cursor.is_null() {
            break;
        }
    }
    assert_eq!(seen.len(), n as usize, "every photo in the burst was shown");
}

#[tokio::test]
async fn images_cannot_be_embedded_by_other_sites() {
    let h = harness(3);
    let req = Request::get(format!("/api/image/{}", id(1)))
        .header("host", HOST)
        .header("sec-fetch-site", "cross-site")
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(&h.app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let req = Request::get(format!("/api/image/{}", id(1)))
        .header("host", HOST)
        .header("sec-fetch-site", "same-origin")
        .body(Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["cross-origin-resource-policy"], "same-origin");
}

#[tokio::test]
async fn a_malformed_request_gets_a_json_error() {
    let h = harness(3);
    let req = Request::post("/api/marks")
        .header("host", HOST)
        .header("content-type", "application/json")
        .header(TOKEN_HEADER, TOKEN)
        .body(Body::from("{ not json"))
        .unwrap();
    let (status, body) = send_json(&h.app, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string(), "the page can show the message: {body}");
}

#[tokio::test]
async fn state_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let h = harness(3);
    let assets = page_assets(&h.app, 3).await;
    mark(&h.app, &assets[..1]).await;
    let mode = std::fs::metadata(&h.marks_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

// ---- Photos that no longer exist in Immich ----

#[tokio::test]
async fn a_photo_deleted_in_immich_does_not_block_trashing_the_rest() {
    let h = harness(5);
    let assets = page_assets(&h.app, 5).await;
    mark(&h.app, &assets[..3]).await;
    h.library.purge(&id(1)); // deleted for good in Immich meanwhile

    let (status, body) =
        send_json(&h.app, post("/api/end-session", json!({ "ids": [id(0), id(1), id(2)], "keep": [] }))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changed"], 2);
    assert_eq!(body["gone"], 1);
    assert!(marked_ids(&body).is_empty(), "the missing photo is unmarked too");
    assert_eq!(trashed_ids(&body), [id(0), id(2)], "only real photos are on the undo list");
}

#[tokio::test]
async fn a_photo_purged_from_immichs_trash_does_not_block_restoring_the_rest() {
    let h = harness(5);
    let assets = page_assets(&h.app, 5).await;
    mark(&h.app, &assets[..3]).await;
    send_json(&h.app, post("/api/trash-marked", json!({ "ids": [id(0), id(1), id(2)] }))).await;
    h.library.purge(&id(1)); // Immich emptied its trash

    let (status, body) = send_json(&h.app, post("/api/restore", json!({ "ids": [id(0), id(1), id(2)] }))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changed"], 2);
    assert_eq!(body["gone"], 1);
    assert!(trashed_ids(&body).is_empty(), "nothing left that could be restored");
    assert!(h.library.trashed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn crash_repair_with_a_photo_gone_from_immich_still_starts() {
    let h = harness(3);
    let assets = page_assets(&h.app, 3).await;
    mark(&h.app, &assets[..1]).await;
    let in_flight: Vec<_> = assets[..1].iter().map(|a| serde_json::from_value(a.clone()).unwrap()).collect();
    std::fs::write(&h.trashed_path, serde_json::to_string(&Trashed { assets: in_flight }).unwrap()).unwrap();
    h.library.purge(&id(0)); // e.g. Immich purged its trash while the tool wasn't used

    let report = h.state.recover().await.unwrap();
    assert_eq!((report.completed, report.not_done, report.unchecked), (1, 0, 0));
}

#[tokio::test]
async fn crash_repair_that_cannot_ask_immich_still_starts() {
    let h = harness(3);
    let assets = page_assets(&h.app, 3).await;
    mark(&h.app, &assets[..1]).await;
    let in_flight: Vec<_> = assets[..1].iter().map(|a| serde_json::from_value(a.clone()).unwrap()).collect();
    std::fs::write(&h.trashed_path, serde_json::to_string(&Trashed { assets: in_flight }).unwrap()).unwrap();
    *h.library.is_trashed_fails.lock().unwrap() = true;

    let report = h.state.recover().await.unwrap();
    assert_eq!(report.unchecked, 1, "left alone, checked again next time");
    let (_, body) = send_json(&h.app, post("/api/marks", json!({}))).await;
    assert_eq!(marked_ids(&body), [id(0)]);
    assert_eq!(trashed_ids(&body), [id(0)]);
}
