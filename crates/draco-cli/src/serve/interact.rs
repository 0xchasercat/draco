//! Tier 2 interact-session registry and Firecrawl-shaped REST handlers.
//!
//! Sessions are in-memory daemon resources: each owns one resumable isolate and
//! expires after inactivity or a hard lifetime. A per-session async mutex
//! serializes turns without holding the registry lock across an await.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use draco_core::{Config, ExecOptions, ExecReport, ExtractionResult, FormatSet, NavReport, Session};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore};

use super::{error_body, parse_formats, to_firecrawl, AppState};

const IDLE_TTL: Duration = Duration::from_secs(60);
const MAX_LIFETIME: Duration = Duration::from_secs(10 * 60);
const REAPER_INTERVAL: Duration = Duration::from_secs(10);

/// A stable read view of one live session.
pub(crate) struct SessionInfo {
    pub(crate) url: String,
}

struct SessionEntry {
    session: Arc<AsyncMutex<Option<Session>>>,
    last_activity: Instant,
    created_at: Instant,
    url: String,
    _permit: OwnedSemaphorePermit,
}

struct StoreInner {
    sessions: Mutex<HashMap<String, SessionEntry>>,
    next_id: AtomicU64,
    permits: Arc<Semaphore>,
}

/// In-memory interact registry shared by REST and daemon-transport MCP calls.
#[derive(Clone)]
pub(crate) struct SessionStore {
    inner: Arc<StoreInner>,
}

pub(crate) struct OpenedSession {
    pub(crate) id: String,
    pub(crate) snapshot_html: Option<String>,
}

#[derive(Debug)]
pub(crate) enum SessionStoreError {
    NotFound,
    Capacity,
    Closed,
    Runtime(String),
}

impl SessionStore {
    pub(crate) fn new(max_sessions: usize) -> Self {
        let store = Self {
            inner: Arc::new(StoreInner {
                sessions: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                permits: Arc::new(Semaphore::new(max_sessions.max(1))),
            }),
        };
        store.spawn_reaper();
        store
    }

    fn lock_sessions(&self) -> MutexGuard<'_, HashMap<String, SessionEntry>> {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn spawn_reaper(&self) {
        let inner = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(REAPER_INTERVAL).await;
                let Some(inner) = inner.upgrade() else {
                    break;
                };
                let store = SessionStore { inner };
                let expired = store.take_expired();
                for entry in expired {
                    let _ = close_entry(entry).await;
                }
            }
        });
    }

    fn take_expired(&self) -> Vec<SessionEntry> {
        let now = Instant::now();
        let mut sessions = self.lock_sessions();
        let ids: Vec<String> = sessions
            .iter()
            .filter(|(_, entry)| {
                now.duration_since(entry.last_activity) > IDLE_TTL
                    || now.duration_since(entry.created_at) > MAX_LIFETIME
            })
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| sessions.remove(&id))
            .collect()
    }

    fn acquire(
        &self,
        id: &str,
    ) -> Result<(Arc<AsyncMutex<Option<Session>>>, String), SessionStoreError> {
        let mut sessions = self.lock_sessions();
        let entry = sessions.get_mut(id).ok_or(SessionStoreError::NotFound)?;
        entry.last_activity = Instant::now();
        Ok((entry.session.clone(), entry.url.clone()))
    }

    fn touch(&self, id: &str) {
        if let Some(entry) = self.lock_sessions().get_mut(id) {
            entry.last_activity = Instant::now();
        }
    }

    pub(crate) fn get(&self, id: &str) -> Option<SessionInfo> {
        let mut sessions = self.lock_sessions();
        let entry = sessions.get_mut(id)?;
        entry.last_activity = Instant::now();
        Some(SessionInfo {
            url: entry.url.clone(),
        })
    }

    pub(crate) async fn open(
        &self,
        url: &str,
        config: &Config,
    ) -> Result<OpenedSession, SessionStoreError> {
        let permit = self
            .inner
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| SessionStoreError::Capacity)?;
        let session = draco_core::open_interact_session(url, config)
            .await
            .map_err(|e| SessionStoreError::Runtime(format!("{e:?}")))?;
        let snapshot_html = session.serialize().await.unwrap_or(None);
        let id = self
            .inner
            .next_id
            .fetch_add(1, Ordering::Relaxed)
            .to_string();
        let now = Instant::now();
        self.lock_sessions().insert(
            id.clone(),
            SessionEntry {
                session: Arc::new(AsyncMutex::new(Some(session))),
                last_activity: now,
                created_at: now,
                url: url.to_string(),
                _permit: permit,
            },
        );
        Ok(OpenedSession { id, snapshot_html })
    }

    pub(crate) async fn exec(
        &self,
        id: &str,
        js: String,
        opts: ExecOptions,
    ) -> Result<ExecReport, SessionStoreError> {
        let (handle, _) = self.acquire(id)?;
        let guard = handle.lock().await;
        let session = guard.as_ref().ok_or(SessionStoreError::Closed)?;
        let report = session
            .exec(js, opts)
            .await
            .map_err(|e| SessionStoreError::Runtime(e.to_string()))?;
        drop(guard);
        self.touch(id);
        Ok(report)
    }

    pub(crate) async fn navigate(
        &self,
        id: &str,
        url: String,
    ) -> Result<NavReport, SessionStoreError> {
        let (handle, _) = self.acquire(id)?;
        let guard = handle.lock().await;
        let report = guard
            .as_ref()
            .ok_or(SessionStoreError::Closed)?
            .navigate(url)
            .await
            .map_err(|e| SessionStoreError::Runtime(e.to_string()))?;
        drop(guard);
        let mut sessions = self.lock_sessions();
        if let Some(entry) = sessions.get_mut(id) {
            entry.last_activity = Instant::now();
            if let Some(url) = report.url.as_ref() {
                entry.url = url.clone();
            }
        }
        Ok(report)
    }

    pub(crate) async fn scrape(
        &self,
        id: &str,
        formats: FormatSet,
        only_main_content: bool,
    ) -> Result<ExtractionResult, SessionStoreError> {
        let info = self.get(id).ok_or(SessionStoreError::NotFound)?;
        let (handle, _) = self.acquire(id)?;
        let guard = handle.lock().await;
        let html = guard
            .as_ref()
            .ok_or(SessionStoreError::Closed)?
            .serialize()
            .await
            .map_err(|e| SessionStoreError::Runtime(e.to_string()))?
            .ok_or_else(|| SessionStoreError::Runtime("session produced no DOM".to_string()))?;
        drop(guard);
        self.touch(id);
        Ok(draco_core::scrape_interact_html(
            &info.url,
            &html,
            formats,
            only_main_content,
        ))
    }

    pub(crate) async fn close(&self, id: &str) -> Result<(), SessionStoreError> {
        let entry = self
            .lock_sessions()
            .remove(id)
            .ok_or(SessionStoreError::NotFound)?;
        close_entry(entry).await
    }

    pub(crate) async fn close_all(&self) {
        let entries: Vec<SessionEntry> = self.lock_sessions().drain().map(|(_, e)| e).collect();
        for entry in entries {
            let _ = close_entry(entry).await;
        }
    }
}

async fn close_entry(entry: SessionEntry) -> Result<(), SessionStoreError> {
    let mut guard = entry.session.lock().await;
    if let Some(session) = guard.take() {
        session
            .close()
            .await
            .map_err(|e| SessionStoreError::Runtime(e.to_string()))?;
    }
    Ok(())
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RenderOptions {
    #[serde(default)]
    proxy: Option<String>,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    capture_window_ms: Option<u64>,
    #[serde(default)]
    tier_max: Option<u8>,
    #[serde(default)]
    ignore_robots: Option<bool>,
    #[serde(default)]
    allow_unsafe_replay: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OpenRequest {
    url: String,
    #[serde(default)]
    render_opts: Option<RenderOptions>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExecRequest {
    js: String,
    #[serde(default)]
    settle: Option<bool>,
    #[serde(default)]
    full: Option<bool>,
    #[serde(default)]
    max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct NavigateRequest {
    url: String,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScrapeRequest {
    #[serde(default)]
    formats: Vec<String>,
    #[serde(default)]
    only_main_content: Option<bool>,
}

pub(crate) async fn open_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<OpenRequest>,
) -> (StatusCode, Json<Value>) {
    if req.url.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(error_body("\"url\" must be a non-empty string")),
        );
    }
    let render = req.render_opts.unwrap_or_default();
    let config = Config {
        formats: FormatSet::markdown_only(),
        proxy: render.proxy.or_else(|| state.defaults.proxy.clone()),
        timeout_ms: render.timeout.unwrap_or(state.defaults.timeout_ms),
        capture_window_ms: render
            .capture_window_ms
            .unwrap_or(state.defaults.capture_window_ms),
        tier_max: render.tier_max.unwrap_or(state.defaults.tier_max),
        respect_robots: render
            .ignore_robots
            .map(|ignore| !ignore)
            .unwrap_or(state.defaults.respect_robots),
        allow_unsafe_replay: render
            .allow_unsafe_replay
            .unwrap_or(state.defaults.allow_unsafe_replay),
        force_render: false,
        ..state.defaults.clone()
    };

    match state.sessions.open(&req.url, &config).await {
        Ok(opened) => {
            let snapshot = opened.snapshot_html.as_deref().map(|html| {
                let result = draco_core::scrape_interact_html(
                    &req.url,
                    html,
                    FormatSet::markdown_only(),
                    true,
                );
                json!({
                    "markdown": result.markdown,
                    "html": html,
                    "metadata": result.metadata,
                })
            });
            (
                StatusCode::OK,
                Json(json!({
                    "success": true,
                    "sessionId": opened.id,
                    "snapshot": snapshot,
                })),
            )
        }
        Err(error) => store_error_response(error),
    }
}

pub(crate) async fn exec_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<ExecRequest>,
) -> (StatusCode, Json<Value>) {
    if req.js.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(error_body("\"js\" must be a non-empty string")),
        );
    }
    let defaults = ExecOptions::default();
    let opts = ExecOptions {
        settle: req.settle.unwrap_or(defaults.settle),
        full: req.full.unwrap_or(defaults.full),
        max_bytes: req.max_bytes.unwrap_or(defaults.max_bytes).max(1),
    };
    match state.sessions.exec(&id, req.js, opts).await {
        Ok(report) => (
            StatusCode::OK,
            Json(json!({
                "success": report.ok,
                "result": report.result,
                "logs": report.logs,
                "error": report.error,
            })),
        ),
        Err(error) => store_error_response(error),
    }
}

pub(crate) async fn navigate_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<NavigateRequest>,
) -> (StatusCode, Json<Value>) {
    if req.url.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(error_body("\"url\" must be a non-empty string")),
        );
    }
    match state.sessions.navigate(&id, req.url).await {
        Ok(report) => (
            StatusCode::OK,
            Json(json!({
                "success": report.ok,
                "url": report.url,
                "error": report.error,
            })),
        ),
        Err(error) => store_error_response(error),
    }
}

pub(crate) async fn scrape_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<ScrapeRequest>,
) -> (StatusCode, Json<Value>) {
    let formats = match parse_formats(&req.formats) {
        Ok(formats) => formats,
        Err(reject) => {
            let status = if reject.unsupported {
                StatusCode::UNPROCESSABLE_ENTITY
            } else {
                StatusCode::BAD_REQUEST
            };
            return (status, Json(error_body(&reject.message)));
        }
    };
    if formats.json || formats.endpoints {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(error_body(
                "interact scrape supports DOM-derived formats only: markdown, html, rawHtml, links",
            )),
        );
    }
    let only_main = req
        .only_main_content
        .unwrap_or(state.defaults.only_main_content);
    match state.sessions.scrape(&id, formats, only_main).await {
        Ok(result) => {
            let (status, body) = to_firecrawl(&result);
            (status, Json(body))
        }
        Err(error) => store_error_response(error),
    }
}

pub(crate) async fn close_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    match state.sessions.close(&id).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "success": true }))),
        Err(error) => store_error_response(error),
    }
}

fn store_error_response(error: SessionStoreError) -> (StatusCode, Json<Value>) {
    match error {
        SessionStoreError::NotFound => (
            StatusCode::NOT_FOUND,
            Json(error_body("interact session not found")),
        ),
        SessionStoreError::Capacity => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(error_body("interact session capacity reached")),
        ),
        SessionStoreError::Closed => (
            StatusCode::GONE,
            Json(error_body("interact session is closed")),
        ),
        SessionStoreError::Runtime(message) => (
            StatusCode::BAD_GATEWAY,
            Json(error_body(&format!("interact session error: {message}"))),
        ),
    }
}
