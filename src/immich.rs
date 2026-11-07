//! Everything that talks to Immich.
//!
//! The rest of the program only sees the `PhotoSource` trait, so tests can
//! swap in a fake Immich that has no real photos behind it.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::header::{HeaderValue, CONTENT_TYPE};
use serde::{Deserialize, Serialize};

use crate::tunnel::Tunnel;

/// Immich allows at most this many results per search request.
pub const MAX_SEARCH_SIZE: usize = 1000;
/// The only image types passed on to the browser.
pub const ALLOWED_IMAGE_TYPES: [&str; 6] =
    ["image/jpeg", "image/webp", "image/png", "image/avif", "image/heic", "image/heif"];
/// Thumbnails and previews are far smaller than this; anything bigger is refused.
pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// Immich asset ids are UUIDs. Checking this stops anything else (like
/// "../users") being slipped into a URL sent to Immich.
#[must_use]
pub fn is_valid_id(id: &str) -> bool {
    id.len() == 36 && uuid::Uuid::try_parse(id).is_ok()
}

/// One search result: the photos, plus how many Immich actually returned.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Found {
    /// Oldest first, trashed ones left out.
    pub assets: Vec<Asset>,
    /// How many Immich returned, *before* trashed ones were left out. If
    /// this reached the limit, there may be more photos after these.
    pub returned: usize,
}

/// One photo or video, with only the fields the tool needs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Asset {
    pub id: String,
    /// When it was taken (UTC). Used for ordering.
    pub taken_at: DateTime<Utc>,
    /// Wall-clock time where it was taken, as Immich reports it. Used for display.
    pub local_date_time: Option<String>,
    pub file_name: String,
    pub is_video: bool,
    /// "City, Country" when Immich knows where it was taken.
    pub place: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ImageSize {
    /// Small (~250px), for the grid.
    Thumbnail,
    /// Larger (~1440px), for the full-size view.
    Preview,
}

impl ImageSize {
    fn as_query(self) -> &'static str {
        match self {
            ImageSize::Thumbnail => "thumbnail",
            ImageSize::Preview => "preview",
        }
    }
}

#[derive(Debug)]
pub struct Image {
    pub content_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("cannot reach Immich ({0}). Are you on home WiFi or Tailscale?")]
    Unreachable(String),
    #[error(
        "Immich refused the request (HTTP {0}): the API key is wrong or is missing a permission"
    )]
    Refused(u16),
    #[error("not found in Immich")]
    NotFound,
    #[error("Immich returned HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("unexpected response from Immich: {0}")]
    BadResponse(String),
    #[error("the Immich API key may not {0} (Immich said HTTP {1}). Give the key that permission: see Setup in the README")]
    MissingPermission(&'static str, u16),
    #[error("not a valid photo id: {0:?}")]
    InvalidId(String),
}

impl SourceError {
    /// True when Immich may or may not have done what was asked: the
    /// connection broke, timed out, or Immich failed on its side.
    #[must_use]
    pub fn outcome_unknown(&self) -> bool {
        match self {
            SourceError::Unreachable(_) | SourceError::BadResponse(_) => true,
            SourceError::Http { status, .. } => *status >= 500,
            _ => false,
        }
    }
}

#[async_trait]
pub trait PhotoSource: Send + Sync {
    /// Up to `limit` assets taken at or after `after` (or from the very
    /// oldest when `after` is None), oldest first. Trashed assets are left out.
    async fn assets_taken_after(
        &self,
        after: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Found, SourceError>;

    async fn image(&self, id: &str, size: ImageSize) -> Result<Image, SourceError>;

    /// Move assets to Immich's trash (restorable there for 30 days).
    async fn trash(&self, ids: &[String]) -> Result<(), SourceError>;

    /// Bring assets back out of Immich's trash (undo).
    async fn restore(&self, ids: &[String]) -> Result<(), SourceError>;

    /// Whether one asset is currently in Immich's trash. Used after a crash
    /// to find out how far an interrupted trash request got.
    async fn is_trashed(&self, id: &str) -> Result<bool, SourceError>;

    /// Checks at start-up, without trashing or restoring anything, that the
    /// API key is allowed to trash and restore.
    async fn check_can_trash_and_restore(&self) -> Result<(), SourceError> {
        Ok(())
    }
}

/// The real Immich, over HTTP.
pub struct ImmichClient {
    base: reqwest::Url,
    key: HeaderValue,
    http: reqwest::Client,
}

impl ImmichClient {
    /// # Errors
    /// If the URL or the API key isn't usable.
    pub fn new(base_url: &str, api_key: &str) -> anyhow::Result<Self> {
        Self::build(base_url, api_key, None)
    }

    /// Like `new`, but every connection is pinned to Tailscale's tunnel: it
    /// goes to the checked address, from this Mac's own Tailscale address.
    /// If Tailscale goes down, that address disappears and requests fail
    /// instead of going out over WiFi.
    ///
    /// # Errors
    /// If the URL or the API key isn't usable.
    pub fn through_tunnel(base_url: &str, api_key: &str, tunnel: &Tunnel) -> anyhow::Result<Self> {
        Self::build(base_url, api_key, Some(tunnel))
    }

    fn build(base_url: &str, api_key: &str, tunnel: Option<&Tunnel>) -> anyhow::Result<Self> {
        let mut base = reqwest::Url::parse(base_url)?;
        if !matches!(base.scheme(), "http" | "https") {
            anyhow::bail!("Immich URL must use http or https");
        }
        // A trailing slash makes `join("api/...")` append instead of replace.
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }

        let mut key = HeaderValue::from_str(api_key)
            .map_err(|_| anyhow::anyhow!("the API key contains characters that are not allowed"))?;
        // Keeps the key out of reqwest's debug output.
        key.set_sensitive(true);

        let mut http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            // Never follow a redirect: the key is a custom header, and reqwest
            // would send it on to whatever server the redirect points to.
            .redirect(reqwest::redirect::Policy::none());
        if let Some(t) = tunnel {
            // The address checked at start-up, not a fresh (spoofable) lookup.
            http = http.resolve(&t.host, t.remote).local_address(t.local);
        }
        let http = http.build()?;

        Ok(Self { base, key, http })
    }

    fn url(&self, path: &str) -> Result<reqwest::Url, SourceError> {
        self.base
            .join(path)
            .map_err(|e| SourceError::BadResponse(format!("could not build the Immich URL: {e}")))
    }

    /// The URL for one asset. The id is checked first, so it can't change
    /// which Immich API is called.
    fn asset_url(&self, id: &str, rest: &str) -> Result<reqwest::Url, SourceError> {
        if !is_valid_id(id) {
            return Err(SourceError::InvalidId(id.to_string()));
        }
        self.url(&format!("api/assets/{id}{rest}"))
    }

    fn check_ids(ids: &[String]) -> Result<(), SourceError> {
        match ids.iter().find(|id| !is_valid_id(id)) {
            Some(bad) => Err(SourceError::InvalidId(bad.clone())),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl PhotoSource for ImmichClient {
    async fn assets_taken_after(
        &self,
        after: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Found, SourceError> {
        let mut body = serde_json::json!({
            "page": 1,
            "size": limit.clamp(1, MAX_SEARCH_SIZE),
            "order": "asc",
            "withExif": true,
            "withDeleted": false,
        });
        if let Some(t) = after {
            body["takenAfter"] = t.to_rfc3339_opts(SecondsFormat::Millis, true).into();
        }

        let resp = self
            .http
            .post(self.url("api/search/metadata")?)
            .header("x-api-key", self.key.clone())
            .json(&body)
            .send()
            .await
            .map_err(network_error)?;
        let resp = check_status(resp).await?;

        let parsed: SearchResponse = resp
            .json()
            .await
            .map_err(|e| SourceError::BadResponse(e.to_string()))?;

        let returned = parsed.assets.items.len();
        let assets = parsed
            .assets
            .items
            .into_iter()
            .filter(|a| !a.is_trashed)
            .map(Asset::from)
            .collect();
        Ok(Found { assets, returned })
    }

    async fn image(&self, id: &str, size: ImageSize) -> Result<Image, SourceError> {
        let mut url = self.asset_url(id, "/thumbnail")?;
        url.query_pairs_mut().append_pair("size", size.as_query());

        let resp = self
            .http
            .get(url)
            .header("x-api-key", self.key.clone())
            .send()
            .await
            .map_err(network_error)?;
        let mut resp = check_status(resp).await?;

        // Only ever pass ordinary photo formats on to the browser (not, say,
        // SVG, which can contain scripts).
        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if !ALLOWED_IMAGE_TYPES.contains(&content_type.as_str()) {
            return Err(SourceError::BadResponse(format!(
                "expected a photo, got {content_type:?}"
            )));
        }

        let too_big = || SourceError::BadResponse(format!("image larger than {MAX_IMAGE_BYTES} bytes"));
        if resp.content_length().is_some_and(|n| n > MAX_IMAGE_BYTES as u64) {
            return Err(too_big());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(network_error)? {
            if bytes.len() + chunk.len() > MAX_IMAGE_BYTES {
                return Err(too_big());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Image {
            content_type,
            bytes,
        })
    }

    async fn trash(&self, ids: &[String]) -> Result<(), SourceError> {
        Self::check_ids(ids)?;
        let resp = self
            .http
            .delete(self.url("api/assets")?)
            .header("x-api-key", self.key.clone())
            // force: false = move to trash, never delete permanently.
            .json(&serde_json::json!({ "ids": ids, "force": false }))
            .send()
            .await
            .map_err(network_error)?;
        check_status(resp).await?;
        Ok(())
    }

    async fn restore(&self, ids: &[String]) -> Result<(), SourceError> {
        Self::check_ids(ids)?;
        let resp = self
            .http
            .post(self.url("api/trash/restore/assets")?)
            .header("x-api-key", self.key.clone())
            .json(&serde_json::json!({ "ids": ids }))
            .send()
            .await
            .map_err(network_error)?;
        check_status(resp).await?;
        Ok(())
    }

    async fn is_trashed(&self, id: &str) -> Result<bool, SourceError> {
        let resp = self
            .http
            .get(self.asset_url(id, "")?)
            .header("x-api-key", self.key.clone())
            .send()
            .await
            .map_err(network_error)?;
        // Immich answers 400 ("not found or no access"), not 404, for a photo
        // that doesn't exist. The id was checked and the key can read photos
        // (checked at start-up), so here it means the photo is gone.
        if resp.status() == reqwest::StatusCode::BAD_REQUEST {
            return Err(SourceError::NotFound);
        }
        let resp = check_status(resp).await?;
        let status: RawStatus = resp
            .json()
            .await
            .map_err(|e| SourceError::BadResponse(e.to_string()))?;
        Ok(status.is_trashed)
    }

    async fn check_can_trash_and_restore(&self) -> Result<(), SourceError> {
        // Both requests carry an empty list, so nothing is trashed or
        // restored. Immich checks the key's permissions before it looks at
        // the list, so a missing permission still shows up as 401/403.
        let checks = [
            (
                self.http.delete(self.url("api/assets")?).json(&serde_json::json!({ "ids": [], "force": false })),
                "move photos to the trash",
            ),
            (
                self.http.post(self.url("api/trash/restore/assets")?).json(&serde_json::json!({ "ids": [] })),
                "restore photos from the trash (needed for undo)",
            ),
        ];
        for (req, what) in checks {
            let resp = req.header("x-api-key", self.key.clone()).send().await.map_err(network_error)?;
            match resp.status().as_u16() {
                code @ (401 | 403) => return Err(SourceError::MissingPermission(what, code)),
                404 => {
                    return Err(SourceError::BadResponse(format!(
                        "Immich has no API to {what}; it may have changed in an update"
                    )))
                }
                // Anything else (done, or "the list is empty") means the key
                // got past the permission check.
                _ => {}
            }
        }
        Ok(())
    }
}

fn network_error(e: reqwest::Error) -> SourceError {
    SourceError::Unreachable(e.without_url().to_string())
}

async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response, SourceError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    match status.as_u16() {
        300..=399 => Err(SourceError::BadResponse(format!(
            "Immich answered with a redirect (HTTP {status}), which the tool never follows so the API key \
             can't be sent elsewhere. Check IMMICH_URL is Immich's own address"
        ))),
        401 | 403 => Err(SourceError::Refused(status.as_u16())),
        404 => Err(SourceError::NotFound),
        code => {
            let body: String = resp.text().await.unwrap_or_default().chars().take(300).collect();
            Err(SourceError::Http { status: code, body })
        }
    }
}

// The shapes Immich sends back. Unknown fields are ignored, and fields that
// might be missing in some Immich versions are optional, so small API changes
// don't break the tool.

#[derive(Deserialize)]
struct SearchResponse {
    assets: SearchAssets,
}

#[derive(Deserialize)]
struct SearchAssets {
    items: Vec<RawAsset>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAsset {
    id: String,
    file_created_at: DateTime<Utc>,
    local_date_time: Option<String>,
    #[serde(default)]
    original_file_name: String,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    is_trashed: bool,
    exif_info: Option<RawExif>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawStatus {
    #[serde(default)]
    is_trashed: bool,
}

#[derive(Deserialize)]
struct RawExif {
    city: Option<String>,
    state: Option<String>,
    country: Option<String>,
}

impl From<RawAsset> for Asset {
    fn from(raw: RawAsset) -> Self {
        let place = raw.exif_info.and_then(|e| {
            let non_empty = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
            let local = non_empty(e.city).or_else(|| non_empty(e.state));
            let parts: Vec<String> = [local, non_empty(e.country)].into_iter().flatten().collect();
            (!parts.is_empty()).then(|| parts.join(", "))
        });
        Asset {
            id: raw.id,
            taken_at: raw.file_created_at,
            local_date_time: raw.local_date_time,
            file_name: raw.original_file_name,
            is_video: raw.kind.eq_ignore_ascii_case("VIDEO"),
            place,
        }
    }
}
