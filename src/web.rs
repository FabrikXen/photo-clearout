//! The local web server: the page, plus a small JSON API the page calls.
//!
//! Security model: the server listens only on 127.0.0.1, but any website open
//! in your browser could still *try* to send requests to it. So every request
//! must carry the right Host (blocks DNS-rebinding tricks), any Origin must be
//! this tool itself, and every state-changing request must carry a secret
//! token that only the tool's own page knows.
//!
//! Safety model: browsing only *marks* photos. Photos are trashed only on
//! request ("trash this page" or "end session"), and only if they're marked.
//! Every trash is logged in `.state/trashed.json` so it can be undone.
//! Trashing and restoring run as background jobs, so closing the browser
//! mid-way can't leave them half done. A crash is repaired on next start
//! (see `AppState::recover`).

use std::collections::HashSet;
use std::path::Path as FsPath;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    extract::{FromRequest, Path, Query, Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub use crate::immich::is_valid_id;
use crate::immich::{Asset, ImageSize, PhotoSource, SourceError};
use crate::paging::{self, Cursor};
use crate::progress::{
    MarkStore, ProgressStore, Settings, SettingsStore, TrashedStore, MAX_COLS, MIN_COLS,
};

pub const MAX_PAGE_SIZE: usize = 200;
/// Immich gets photos to trash or restore in batches of at most this many.
pub const MAX_TRASH_BATCH: usize = 500;
/// A sanity limit on how many photos can be marked at once.
pub const MAX_MARKED: usize = 20_000;
/// Backstop: the tool stops when no page has been in touch for this long
/// (e.g. the browser crashed and couldn't say goodbye). The page beats every
/// 10 s; browsers slow background tabs to about once a minute, so this leaves
/// room for a tab you've only switched away from.
pub const IDLE_LIMIT: Duration = Duration::from_mins(2);
/// After the page says goodbye (tab closed), wait this long before stopping,
/// so a page *reload* (goodbye, then straight back) doesn't stop the tool.
pub const GOODBYE_GRACE: Duration = Duration::from_secs(5);
pub const TOKEN_HEADER: &str = "x-clearout-token";

const INDEX_HTML: &str = include_str!("../static/index.html");
const APP_JS: &str = include_str!("../static/app.js");
const APP_CSS: &str = include_str!("../static/app.css");

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; \
                   connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    source: Arc<dyn PhotoSource>,
    progress: ProgressStore,
    settings: SettingsStore,
    marks: MarkStore,
    trashed: TrashedStore,
    token: String,
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
    /// Held for the whole of every change to marks or the undo list (marking,
    /// trashing, restoring, ending the session, crash repair), so two
    /// requests can never interleave and leave the lists disagreeing with
    /// Immich or with each other.
    ops: tokio::sync::Mutex<()>,
    /// Signalled by "Finish", so `main()` can stop the server.
    shutdown: tokio::sync::Notify,
    /// The clock for the idle and tab-closed checks. Monotonic, so changing
    /// the Mac's clock can't stop the tool.
    started: Instant,
    /// Time (ms on `started`'s clock) of the last request from the page.
    last_seen: AtomicU64,
    /// Counts every request from a page. Comparing counts (not times) tells
    /// whether anything was heard after a goodbye, even within the same ms.
    contacts: AtomicU64,
    /// Time (ms on `started`'s clock) the page last said goodbye, or 0.
    goodbye_at: AtomicU64,
    /// The value of `contacts` when the page said goodbye.
    goodbye_contacts: AtomicU64,
    /// How many trash/restore jobs are running. The tool waits for this to
    /// reach 0 before it exits, so a job is never cut off halfway.
    jobs: tokio::sync::watch::Sender<usize>,
    /// Numbers each /api/page request. Only a request newer than the one
    /// that last saved progress may save, so when two overlap, the page you
    /// asked for last is the one remembered.
    page_requests: AtomicU64,
    progress_saved_by: std::sync::Mutex<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum RecoverError {
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error("could not save state: {0}")]
    State(#[from] std::io::Error),
}

/// What `recover` found after an unfinished trash request.
#[derive(Debug, Default, PartialEq)]
pub struct Recovery {
    /// Were trashed before the crash: unmarked now, kept in the undo list.
    pub completed: usize,
    /// Were not trashed: removed from the undo list, still marked.
    pub not_done: usize,
    /// Immich couldn't say: left as they were, checked again next time.
    pub unchecked: usize,
}

impl AppState {
    /// `state_dir` is where progress, settings, marks and the undo list are saved.
    pub fn new(source: Arc<dyn PhotoSource>, state_dir: &FsPath, token: String, port: u16) -> Self {
        let allowed_hosts = vec![format!("127.0.0.1:{port}"), format!("localhost:{port}")];
        let allowed_origins = allowed_hosts.iter().map(|h| format!("http://{h}")).collect();
        Self {
            inner: Arc::new(Inner {
                source,
                progress: ProgressStore::new(state_dir.join("progress.json")),
                settings: SettingsStore::new(state_dir.join("settings.json")),
                marks: MarkStore::new(state_dir.join("marked.json")),
                trashed: TrashedStore::new(state_dir.join("trashed.json")),
                token,
                allowed_hosts,
                allowed_origins,
                ops: tokio::sync::Mutex::new(()),
                shutdown: tokio::sync::Notify::new(),
                started: Instant::now(),
                last_seen: AtomicU64::new(0),
                contacts: AtomicU64::new(0),
                goodbye_at: AtomicU64::new(0),
                goodbye_contacts: AtomicU64::new(0),
                jobs: tokio::sync::watch::Sender::new(0),
                page_requests: AtomicU64::new(0),
                progress_saved_by: std::sync::Mutex::new(0),
            }),
        }
    }

    /// Completes when the user presses "Finish" on the page.
    pub async fn finished(&self) {
        self.inner.shutdown.notified().await;
    }

    /// Completes once no page has been in touch for `limit`, i.e. the
    /// browser was closed. The tool shouldn't keep running in the background
    /// with your API key in memory.
    pub async fn idle_for(&self, limit: Duration) {
        let limit_ms = millis(limit);
        loop {
            tokio::time::sleep(Duration::from_secs(5).min(limit)).await;
            let seen = self.inner.last_seen.load(Ordering::Relaxed);
            if self.inner.now_ms().saturating_sub(seen) >= limit_ms {
                return;
            }
        }
    }

    /// Completes when the page said goodbye (the tab was closed) and nothing
    /// has been heard from any page for `grace` since. A reload says goodbye
    /// too, but its new page is back within moments, which cancels this.
    pub async fn page_closed(&self, grace: Duration) {
        let grace_ms = millis(grace);
        loop {
            tokio::time::sleep(Duration::from_millis(250).min(grace)).await;
            let inner = &self.inner;
            let bye = inner.goodbye_at.load(Ordering::SeqCst);
            let heard_since = inner.contacts.load(Ordering::SeqCst) != inner.goodbye_contacts.load(Ordering::SeqCst);
            if bye != 0 && !heard_since && inner.now_ms().saturating_sub(bye) >= grace_ms {
                return;
            }
        }
    }

    /// Completes when no trash/restore job is running. Called before exiting.
    pub async fn jobs_done(&self) {
        let mut rx = self.inner.jobs.subscribe();
        let _ = rx.wait_for(|n| *n == 0).await;
    }

    /// Repair the state after a crash in the middle of trashing.
    ///
    /// Before asking Immich to trash a photo, it's written to the undo list
    /// while still marked. So a photo that is *both* marked and in the undo
    /// list was mid-flight when the tool stopped. Ask Immich which way it went.
    ///
    /// # Errors
    /// If Immich can't be asked, or the state files can't be saved.
    pub async fn recover(&self) -> Result<Recovery, RecoverError> {
        let inner = &self.inner;
        let _ops = inner.ops.lock().await;
        let marked = inner.marks.load();
        let marked = marked.ids();
        let in_flight: Vec<String> = inner
            .trashed
            .load()
            .assets
            .into_iter()
            .filter(|a| marked.contains(a.id.as_str()))
            // The state files are ours, but check anyway before an id goes
            // into a URL. A bad one is left alone and reported.
            .filter(|a| {
                let ok = is_valid_id(&a.id);
                if !ok {
                    eprintln!("warning: ignoring an invalid photo id in the undo list: {:?}", a.id);
                }
                ok
            })
            .map(|a| a.id)
            .collect();

        let mut completed: HashSet<String> = HashSet::new();
        let mut not_done: HashSet<String> = HashSet::new();
        let mut unchecked = 0;
        for id in in_flight {
            match inner.source.is_trashed(&id).await {
                // Gone entirely counts as done: there's nothing left to trash.
                Ok(true) | Err(SourceError::NotFound) => {
                    completed.insert(id);
                }
                Ok(false) => {
                    not_done.insert(id);
                }
                // One photo Immich can't answer about mustn't stop the tool
                // starting. It's left as it is and checked again next time.
                Err(e) => {
                    eprintln!("warning: could not check photo {id} with Immich ({e}); will check again next time");
                    unchecked += 1;
                }
            }
        }

        inner.marks.update(|m| m.assets.retain(|a| !completed.contains(&a.id)))?;
        inner.trashed.update(|t| t.assets.retain(|a| !not_done.contains(&a.id)))?;
        Ok(Recovery {
            completed: completed.len(),
            not_done: not_done.len(),
            unchecked,
        })
    }
}

impl Inner {
    /// Milliseconds since the tool started, plus 1 so it's never 0 (0 means
    /// "never" in `goodbye_at`).
    fn now_ms(&self) -> u64 {
        millis(self.started.elapsed()).saturating_add(1)
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// A fresh random secret for each run of the tool.
#[must_use]
pub fn random_token() -> String {
    use std::fmt::Write;
    rand::random::<[u8; 32]>().iter().fold(String::with_capacity(64), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    })
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/app.css", get(app_css))
        .route("/api/page", post(page))
        .route("/api/reset", post(reset))
        .route("/api/settings", post(save_settings))
        .route("/api/marks", post(marks))
        .route("/api/trash-marked", post(trash_marked))
        .route("/api/end-session", post(end_session))
        .route("/api/restore", post(restore))
        .route("/api/finish", post(finish))
        .route("/api/ping", post(ping))
        .route("/api/bye", post(bye))
        .route("/api/image/{id}", get(image))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

async fn guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let inner = &state.inner;
    let (host_ok, origin_ok, token_ok) = {
        let headers = req.headers();
        let text = |name| headers.get(name).and_then(|v: &HeaderValue| v.to_str().ok());

        let host_ok = text(header::HOST).is_some_and(|h| inner.allowed_hosts.iter().any(|a| a == h));
        // Browsers say where a request comes from. Only the tool's own page
        // ("same-origin") or typing the address ("none") may use it, so
        // another website can't even embed its images.
        let site_ok = text(header::HeaderName::from_static("sec-fetch-site"))
            .is_none_or(|s| s == "same-origin" || s == "none");
        // Browsers send Origin on cross-site requests; same-page GETs may omit it.
        let origin_ok = match headers.get(header::ORIGIN) {
            None => true,
            Some(_) => text(header::ORIGIN).is_some_and(|o| inner.allowed_origins.iter().any(|a| a == o)),
        };
        // "Goodbye" is sent by the browser as the tab closes, which can't add
        // headers, so it carries the token in its body instead (checked in bye()).
        let token_ok = req.method() != Method::POST
            || req.uri().path() == "/api/bye"
            || text(header::HeaderName::from_static(TOKEN_HEADER))
                .is_some_and(|t| constant_time_eq(t.as_bytes(), inner.token.as_bytes()));
        (host_ok, origin_ok && site_ok, token_ok)
    };

    if !host_ok {
        return error(StatusCode::FORBIDDEN, "unexpected Host header");
    }
    if !origin_ok {
        return error(StatusCode::FORBIDDEN, "cross-origin request refused");
    }
    if !token_ok {
        return error(StatusCode::FORBIDDEN, "missing or wrong token");
    }

    if req.uri().path() != "/api/bye" {
        inner.last_seen.store(inner.now_ms(), Ordering::Relaxed);
        inner.contacts.fetch_add(1, Ordering::SeqCst);
    }

    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
    resp
}

/// Compares secrets without leaking, through timing, how much matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn check_ids<'a>(ids: impl IntoIterator<Item = &'a str>) -> Result<(), ApiError> {
    match ids.into_iter().find(|id| !is_valid_id(id)) {
        Some(bad) => Err(ApiError::BadRequest(format!("not a valid photo id: {bad:?}"))),
        None => Ok(()),
    }
}

fn sorted_unique(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids.dedup();
    ids
}

// Takes the error by value so it can be passed straight to `map_err`.
#[allow(clippy::needless_pass_by_value)]
fn store_error(e: std::io::Error) -> ApiError {
    ApiError::Internal(format!("could not save state: {e}"))
}

/// Like axum's `Json`, but a body that isn't valid JSON (or has the wrong
/// fields) gets the same `{"error": …}` reply as every other error, so the
/// page can show it.
struct ApiJson<T>(T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for ApiJson<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(ApiJson(value)),
            Err(rejection) => Err(ApiError::BadRequest(rejection.body_text())),
        }
    }
}

// ---- The page ----

async fn index(State(state): State<AppState>) -> Response {
    // Saved grid size, or empty for the page's default. Only ever a number
    // the server itself checked, so it's safe to put into the HTML.
    let cols = state
        .inner
        .settings
        .load()
        .cols
        .filter(|c| (MIN_COLS..=MAX_COLS).contains(c))
        .map(|c| c.to_string())
        .unwrap_or_default();
    let html = INDEX_HTML
        .replace("__CLEAROUT_TOKEN__", &state.inner.token)
        .replace("__CLEAROUT_COLS__", &cols);
    ([(header::CACHE_CONTROL, "no-store")], Html(html)).into_response()
}

async fn app_js() -> Response {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], APP_JS).into_response()
}

async fn app_css() -> Response {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS).into_response()
}

async fn ping() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

#[derive(Deserialize)]
struct ByeRequest {
    token: String,
}

/// The page is closing. Sent with the browser's "beacon", which survives the
/// tab closing but can't set headers, so the token comes in the body.
async fn bye(State(state): State<AppState>, body: String) -> Result<Json<serde_json::Value>, ApiError> {
    let inner = &state.inner;
    let ok = serde_json::from_str::<ByeRequest>(&body)
        .is_ok_and(|b| constant_time_eq(b.token.as_bytes(), inner.token.as_bytes()));
    if !ok {
        return Err(ApiError::Forbidden("missing or wrong token".into()));
    }
    inner.goodbye_contacts.store(inner.contacts.load(Ordering::SeqCst), Ordering::SeqCst);
    inner.goodbye_at.store(inner.now_ms(), Ordering::SeqCst);
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---- Browsing ----

#[derive(Deserialize)]
struct PageRequest {
    size: usize,
    /// None means "wherever I stopped last time".
    cursor: Option<Cursor>,
}

#[derive(Serialize)]
struct PageResponse {
    items: Vec<Asset>,
    /// The cursor this page was loaded from (send it again to reload the page).
    cursor: Cursor,
    /// The cursor for the following page, or null at the newest photo.
    next: Option<Cursor>,
}

async fn page(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<PageRequest>,
) -> Result<Json<PageResponse>, ApiError> {
    if req.size == 0 || req.size > MAX_PAGE_SIZE {
        return Err(ApiError::BadRequest(format!(
            "page size must be between 1 and {MAX_PAGE_SIZE}"
        )));
    }
    let inner = &state.inner;
    let ticket = inner.page_requests.fetch_add(1, Ordering::SeqCst) + 1;
    let cursor = req.cursor.unwrap_or_else(|| inner.progress.load());
    let page = paging::load_page(inner.source.as_ref(), &cursor, req.size).await?;

    // Remember this page as "where I stopped", unless a newer request has
    // already saved its page. A failed save shouldn't stop you working, so
    // it's reported in the terminal instead of the page.
    let mut saved_by = inner.progress_saved_by.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if ticket > *saved_by {
        match inner.progress.save(&cursor) {
            Ok(()) => *saved_by = ticket,
            Err(e) => eprintln!(
                "warning: could not save progress to {}: {e}",
                inner.progress.path().display()
            ),
        }
    }
    drop(saved_by);

    Ok(Json(PageResponse {
        items: page.items,
        cursor,
        next: page.next,
    }))
}

async fn reset(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    state.inner.progress.save(&Cursor::default()).map_err(store_error)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn save_settings(
    State(state): State<AppState>,
    ApiJson(new): ApiJson<Settings>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if let Some(c) = new.cols {
        if !(MIN_COLS..=MAX_COLS).contains(&c) {
            return Err(ApiError::BadRequest(format!(
                "grid size must be between {MIN_COLS} and {MAX_COLS}"
            )));
        }
    }
    state.inner.settings.save(&new).map_err(store_error)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---- Marking, trashing, undo ----

/// What every marking/trashing call returns, so the page always shows the
/// server's real state.
#[derive(Serialize)]
struct SessionResponse {
    marked: Vec<Asset>,
    /// Trashed this session (restorable).
    trashed: Vec<Asset>,
    /// How many photos this call trashed or restored.
    changed: usize,
    /// How many turned out to no longer exist in Immich (e.g. deleted for
    /// good there), so they were taken off the lists.
    gone: usize,
}

/// What a trash or restore job did.
#[derive(Debug, Default, Clone, Copy)]
struct Outcome {
    changed: usize,
    gone: usize,
}

/// Both lists are read under the `ops` lock, so they always match each other.
async fn session(inner: &Inner, outcome: Outcome) -> Json<SessionResponse> {
    let _ops = inner.ops.lock().await;
    Json(SessionResponse {
        marked: inner.marks.load().assets,
        trashed: inner.trashed.load().assets,
        changed: outcome.changed,
        gone: outcome.gone,
    })
}

#[derive(Deserialize)]
struct MarksRequest {
    #[serde(default)]
    add: Vec<Asset>,
    #[serde(default)]
    remove: Vec<String>,
}

/// Mark / unmark photos. Nothing is deleted here: marks are only saved.
/// An empty request just returns the current state.
async fn marks(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<MarksRequest>,
) -> Result<Json<SessionResponse>, ApiError> {
    if req.add.len() > MAX_PAGE_SIZE || req.remove.len() > MAX_PAGE_SIZE {
        return Err(ApiError::BadRequest(format!(
            "at most {MAX_PAGE_SIZE} changes at once"
        )));
    }
    check_ids(req.add.iter().map(|a| a.id.as_str()).chain(req.remove.iter().map(String::as_str)))?;

    let mut too_many = false;
    let remove: HashSet<String> = req.remove.into_iter().collect();
    let ops = state.inner.ops.lock().await;
    state
        .inner
        .marks
        .update(|m| {
            m.assets.retain(|a| !remove.contains(&a.id));
            let mut present: HashSet<String> = m.assets.iter().map(|a| a.id.clone()).collect();
            for asset in req.add {
                if !present.insert(asset.id.clone()) {
                    continue;
                }
                if m.assets.len() >= MAX_MARKED {
                    too_many = true;
                    break;
                }
                m.assets.push(asset);
            }
        })
        .map_err(store_error)?;
    drop(ops);

    if too_many {
        return Err(ApiError::BadRequest(format!(
            "at most {MAX_MARKED} photos can be marked; trash some first"
        )));
    }
    Ok(session(&state.inner, Outcome::default()).await)
}

#[derive(Deserialize)]
struct IdsRequest {
    ids: Vec<String>,
}

/// Trash some marked photos (e.g. the marked ones on the current page).
async fn trash_marked(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<IdsRequest>,
) -> Result<Json<SessionResponse>, ApiError> {
    let ids = sorted_unique(req.ids);
    if ids.is_empty() {
        return Err(ApiError::BadRequest("nothing to trash".into()));
    }
    let n = run_to_completion(&state, move |inner| async move { trash_ids(&inner, ids).await }).await?;
    Ok(session(&state.inner, n).await)
}

#[derive(Deserialize)]
struct EndSessionRequest {
    /// Still chosen on the review screen: trash these.
    ids: Vec<String>,
    /// Kept on the review screen: unmark these.
    #[serde(default)]
    keep: Vec<String>,
}

/// End the session: trash the photos still chosen after the review screen,
/// and unmark the ones you kept. Marks the review screen didn't show (say,
/// added in another tab meanwhile) are left alone. The tool keeps running
/// so you can see the summary and undo; "Finish" stops it.
async fn end_session(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<EndSessionRequest>,
) -> Result<Json<SessionResponse>, ApiError> {
    let ids = sorted_unique(req.ids);
    let keep: HashSet<String> = req.keep.into_iter().collect();
    check_ids(keep.iter().map(String::as_str))?;
    let n = run_to_completion(&state, move |inner| async move {
        let n = trash_ids(&inner, ids).await?;
        inner.marks.update(|m| m.assets.retain(|a| !keep.contains(&a.id))).map_err(store_error)?;
        Ok(n)
    })
    .await?;
    Ok(session(&state.inner, n).await)
}

/// Undo: bring photos trashed this session back out of Immich's trash.
async fn restore(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<IdsRequest>,
) -> Result<Json<SessionResponse>, ApiError> {
    let ids = sorted_unique(req.ids);
    if ids.is_empty() {
        return Err(ApiError::BadRequest("nothing to restore".into()));
    }
    let n = run_to_completion(&state, move |inner| async move { restore_ids(&inner, ids).await }).await?;
    Ok(session(&state.inner, n).await)
}

/// Finish: forget this session's undo list and stop the tool. The photos stay
/// restorable from Immich's own Trash page for 30 days.
async fn finish(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let ops = state.inner.ops.lock().await;
    state.inner.trashed.save(&crate::progress::Trashed::default()).map_err(store_error)?;
    drop(ops);
    // Stop after this reply is sent (graceful shutdown waits for it).
    state.inner.shutdown.notify_one();
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Run a trash/restore job as its own task. If the browser disconnects, the
/// web server drops the request, but a spawned task keeps going to the end,
/// so the saved state always matches what Immich actually did.
async fn run_to_completion<F, Fut>(state: &AppState, job: F) -> Result<Outcome, ApiError>
where
    F: FnOnce(Arc<Inner>) -> Fut,
    Fut: std::future::Future<Output = Result<Outcome, ApiError>> + Send + 'static,
{
    let inner = state.inner.clone();
    // Counted from before the task starts until after it ends, so the tool
    // can't exit in between (see jobs_done). The count goes down when the
    // task is dropped, so even a job that panics can't block shutdown.
    inner.jobs.send_modify(|n| *n += 1);
    let running = RunningJob(inner.clone());
    let job = job(inner.clone());
    tokio::spawn(async move {
        let _running = running;
        let _ops = inner.ops.lock().await;
        job.await
    })
    .await
    .map_err(|e| ApiError::Internal(format!("background job failed: {e}")))?
}

/// One running job in `Inner::jobs`, for as long as this exists.
struct RunningJob(Arc<Inner>);

impl Drop for RunningJob {
    fn drop(&mut self) {
        self.0.jobs.send_modify(|n| *n -= 1);
    }
}

async fn trash_ids(inner: &Inner, ids: Vec<String>) -> Result<Outcome, ApiError> {
    check_ids(ids.iter().map(String::as_str))?;
    if ids.is_empty() {
        return Ok(Outcome::default());
    }

    // Only photos you actually marked can be trashed. A request for anything
    // else is refused as a whole, before anything is touched.
    let marked = inner.marks.load();
    let marked_ids = marked.ids();
    if let Some(unmarked) = ids.iter().find(|id| !marked_ids.contains(id.as_str())) {
        return Err(ApiError::BadRequest(format!(
            "photo {unmarked} is not marked; reload the page and try again"
        )));
    }
    drop(marked_ids);
    let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let assets: Vec<Asset> = marked
        .assets
        .into_iter()
        .filter(|a| wanted.contains(a.id.as_str()))
        .collect();

    // Write-ahead: add to the undo list *before* asking Immich. A photo that
    // is both marked and in the undo list is "in flight"; see recover().
    inner
        .trashed
        .update(|t| {
            let mut listed: HashSet<String> = t.assets.iter().map(|a| a.id.clone()).collect();
            for a in &assets {
                if listed.insert(a.id.clone()) {
                    t.assets.push(a.clone());
                }
            }
        })
        .map_err(store_error)?;

    let mut done: HashSet<String> = HashSet::new();
    let mut gone: HashSet<String> = HashSet::new();
    for batch in ids.chunks(MAX_TRASH_BATCH) {
        let mut attempt = batch.to_vec();
        let mut result = inner.source.trash(&attempt).await;
        if result.as_ref().is_err_and(refused_whole_batch) {
            // Immich refuses the whole batch if any photo in it no longer
            // exists. Take those out (and any already in the trash) and try
            // the rest once more.
            if let Ok(sorted) = sort_batch(inner, &attempt).await {
                if sorted.out_of_trash.len() < attempt.len() {
                    gone.extend(sorted.gone);
                    done.extend(sorted.in_trash);
                    attempt = sorted.out_of_trash;
                    result = if attempt.is_empty() { Ok(()) } else { inner.source.trash(&attempt).await };
                }
            }
        }

        let err = match result {
            Ok(()) => {
                done.extend(attempt);
                // Unmark each batch as soon as it's done, so a crash between
                // batches leaves an exact record. Photos no longer in Immich
                // can't be restored either, so they leave the undo list too.
                inner
                    .marks
                    .update(|m| m.assets.retain(|a| !done.contains(&a.id) && !gone.contains(&a.id)))
                    .map_err(store_error)?;
                inner.trashed.update(|t| t.assets.retain(|a| !gone.contains(&a.id))).map_err(store_error)?;
                continue;
            }
            Err(e) => e,
        };

        // After a timeout or a broken connection, Immich may have trashed the
        // batch anyway. Ask about each photo, as recover() does after a crash.
        // If Immich can't be asked either, the rest stay "in flight" (marked
        // *and* on the undo list) and are settled when the tool next starts.
        let mut unknown: HashSet<String> = HashSet::new();
        if err.outcome_unknown() {
            for id in &attempt {
                if !unknown.is_empty() {
                    unknown.insert(id.clone());
                    continue;
                }
                match inner.source.is_trashed(id).await {
                    Ok(true) => {
                        done.insert(id.clone());
                    }
                    Err(SourceError::NotFound) => {
                        gone.insert(id.clone());
                    }
                    Ok(false) => {}
                    Err(_) => {
                        unknown.insert(id.clone());
                    }
                }
            }
        }

        // Trashed: unmarked, kept on the undo list. Not trashed (or never
        // sent): taken off the undo list, still marked, so nothing is lost.
        // No longer in Immich: off both lists.
        inner
            .marks
            .update(|m| m.assets.retain(|a| !done.contains(&a.id) && !gone.contains(&a.id)))
            .map_err(store_error)?;
        inner
            .trashed
            .update(|t| {
                t.assets.retain(|a| {
                    !wanted.contains(a.id.as_str()) || done.contains(&a.id) || unknown.contains(&a.id)
                });
            })
            .map_err(store_error)?;

        let not_done = ids.len() - done.len() - gone.len() - unknown.len();
        let unchecked = if unknown.is_empty() {
            String::new()
        } else {
            format!(
                " {} could not be checked with Immich; they stay marked and are checked again when the tool next starts.",
                unknown.len()
            )
        };
        let missing = if gone.is_empty() {
            String::new()
        } else {
            format!(" {} were no longer in Immich, so they were unmarked.", gone.len())
        };
        return Err(ApiError::Partial(format!(
            "moved {} of {} photos to the trash, then failed: {err}.{missing}{unchecked} The other {not_done} are still marked; nothing was lost.",
            done.len(),
            ids.len(),
        )));
    }
    if !gone.is_empty() {
        eprintln!("note: {} marked photos were no longer in Immich; they were unmarked", gone.len());
    }
    Ok(Outcome { changed: done.len(), gone: gone.len() })
}

/// Immich answers HTTP 400 for a whole trash or restore request when any
/// photo in it doesn't exist (any more), e.g. one deleted for good in Immich.
fn refused_whole_batch(e: &SourceError) -> bool {
    matches!(e, SourceError::Http { status: 400, .. })
}

/// A batch Immich refused, sorted photo by photo.
struct SortedBatch {
    /// No longer in Immich at all.
    gone: Vec<String>,
    in_trash: Vec<String>,
    out_of_trash: Vec<String>,
}

async fn sort_batch(inner: &Inner, batch: &[String]) -> Result<SortedBatch, SourceError> {
    let mut sorted = SortedBatch { gone: vec![], in_trash: vec![], out_of_trash: vec![] };
    for id in batch {
        match inner.source.is_trashed(id).await {
            Ok(true) => sorted.in_trash.push(id.clone()),
            Ok(false) => sorted.out_of_trash.push(id.clone()),
            Err(SourceError::NotFound) => sorted.gone.push(id.clone()),
            Err(e) => return Err(e),
        }
    }
    Ok(sorted)
}

async fn restore_ids(inner: &Inner, ids: Vec<String>) -> Result<Outcome, ApiError> {
    check_ids(ids.iter().map(String::as_str))?;
    // Only photos this tool trashed this session can be restored through it.
    let trashed = inner.trashed.load();
    let trashed = trashed.ids();
    if let Some(unknown) = ids.iter().find(|id| !trashed.contains(id.as_str())) {
        return Err(ApiError::BadRequest(format!(
            "photo {unknown} was not trashed this session; use Immich's Trash page"
        )));
    }

    let mut outcome = Outcome::default();
    for batch in ids.chunks(MAX_TRASH_BATCH) {
        let mut attempt = batch.to_vec();
        let mut settled: Vec<String> = vec![];
        let mut result = inner.source.restore(&attempt).await;
        if result.as_ref().is_err_and(refused_whole_batch) {
            // As for trashing: take out photos no longer in Immich (purged
            // from its trash, say) and any already restored, then retry.
            if let Ok(sorted) = sort_batch(inner, &attempt).await {
                if sorted.in_trash.len() < attempt.len() {
                    outcome.gone += sorted.gone.len();
                    outcome.changed += sorted.out_of_trash.len();
                    settled.extend(sorted.gone);
                    settled.extend(sorted.out_of_trash);
                    attempt = sorted.in_trash;
                    result = if attempt.is_empty() { Ok(()) } else { inner.source.restore(&attempt).await };
                }
            }
        }
        if let Ok(()) = result {
            outcome.changed += attempt.len();
            settled.extend(attempt);
        }
        let off_the_list: HashSet<&str> = settled.iter().map(String::as_str).collect();
        inner
            .trashed
            .update(|t| t.assets.retain(|a| !off_the_list.contains(a.id.as_str())))
            .map_err(store_error)?;
        if let Err(e) = result {
            return Err(ApiError::Partial(format!(
                "restored {} of {} photos, then failed: {e}. The others are still in the trash and still listed here.",
                outcome.changed,
                ids.len()
            )));
        }
    }
    Ok(outcome)
}

// ---- Images ----

#[derive(Deserialize)]
struct ImageQuery {
    size: Option<String>,
}

async fn image(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ImageQuery>,
) -> Result<Response, ApiError> {
    if !is_valid_id(&id) {
        return Err(ApiError::BadRequest("not a valid photo id".into()));
    }
    let size = match q.size.as_deref() {
        None | Some("thumbnail") => ImageSize::Thumbnail,
        Some("preview") => ImageSize::Preview,
        Some(_) => return Err(ApiError::BadRequest("size must be thumbnail or preview".into())),
    };

    let img = state.inner.source.image(&id, size).await?;
    let content_type = HeaderValue::from_str(&img.content_type)
        .map_err(|_| ApiError::Source(SourceError::BadResponse("bad content type".into())))?;
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            // Never let the browser keep copies of your photos on disk.
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        img.bytes,
    )
        .into_response())
}

// ---- Errors ----

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Forbidden(String),
    #[error(transparent)]
    Source(#[from] SourceError),
    /// Part of a multi-step action worked, part failed.
    #[error("{0}")]
    Partial(String),
    #[error("{0}")]
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::BadRequest(msg) => error(StatusCode::BAD_REQUEST, &msg),
            ApiError::Forbidden(msg) => error(StatusCode::FORBIDDEN, &msg),
            ApiError::Source(SourceError::NotFound) => error(StatusCode::NOT_FOUND, "not found in Immich"),
            ApiError::Source(e @ SourceError::InvalidId(_)) => error(StatusCode::BAD_REQUEST, &e.to_string()),
            ApiError::Source(e) => error(StatusCode::BAD_GATEWAY, &e.to_string()),
            ApiError::Partial(msg) => error(StatusCode::BAD_GATEWAY, &msg),
            ApiError::Internal(msg) => error(StatusCode::INTERNAL_SERVER_ERROR, &msg),
        }
    }
}

fn error(status: StatusCode, msg: &str) -> Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}
