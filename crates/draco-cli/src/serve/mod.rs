//! `draco serve` — a persistent HTTP daemon exposing a Firecrawl-compatible
//! REST API over the extraction ladder.
//!
//! The process stays resident, so clients skip the per-scrape binary spawn and
//! the request path is exactly [`draco_core::extract`] — the same tiered ladder
//! the CLI runs, warm. The surface mirrors Firecrawl's self-hosted API so
//! existing Firecrawl clients can point at Draco unchanged:
//!
//! - `GET /health` → `{ "status": "ok", "version": … }`
//! - `POST /v1/scrape` with `{ "url": …, "formats": ["markdown" | "json"], … }`
//!   → `{ "success": true, "data": { "markdown"?, "json"?, "metadata" } }`
//!
//! Firecrawl-compatible notes:
//! - `formats` defaults to `["markdown"]`. Draco's `"json"` is the tiered
//!   JSON-API extraction (embedded state → build-id replay → runtime
//!   interception) — a superset of "structured data from the page", surfaced
//!   under `data.json` like Firecrawl's json format. `html`, `rawHtml`, and
//!   `links` are also supported; only browser-only formats Draco's DOM-only
//!   engine cannot produce (`screenshot`, `actions`, …) are rejected with a
//!   clear `422` (`400` for a token that's unrecognized outright).
//! - `onlyMainContent` (default `true`) and `waitFor` (an alias for
//!   `captureWindowMs` — see below) are honored. Other unknown request fields
//!   (`mobile`, `headers`, `includeTags`, `excludeTags`, …) are accepted and
//!   ignored, so real-world Firecrawl client payloads still work.
//! - Failures use Firecrawl's `{ "success": false, "error": … }` envelope.
//! - Every response also carries a `draco` extension object (`sourceTier`,
//!   `timing`, `trace`) — Draco's honest execution report. Extra keys are
//!   invisible to clients that only read the Firecrawl fields.
//!
//! Concurrency is bounded by a semaphore (`--max-concurrency`): each in-flight
//! scrape may spawn a jailed V8 child, so an unbounded intake could exhaust the
//! host. Excess requests queue rather than fail.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use draco_core::{extract_with_pool, Config, FormatSet, Tier2Pool};
use draco_types::{DracoError, ExtractionResult, Status};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Semaphore;

/// `POST /v1/batch/scrape` + `GET|DELETE /v1/batch/scrape/{id}` — async
/// scrape-a-list-of-URLs jobs.
pub(crate) mod batch;
/// `POST /v1/crawl` + `GET|DELETE /v1/crawl/{id}` — async whole-site crawl jobs.
pub(crate) mod crawl;
/// `POST /v1/discover` — JSON/XHR API endpoint discovery + winner replay.
pub(crate) mod discover;
/// Shared async-job registry (`JobStore`) for crawl + batch scrape.
pub(crate) mod jobs;
/// `POST /v1/map` — fast site URL discovery (sitemap + on-page links).
pub(crate) mod map;
/// `POST /v1/search` — Firecrawl-compatible metasearch (parallel HTTP engines
/// + reciprocal-rank consensus; no rendering).
pub(crate) mod search;
/// Firecrawl-compatible webhook delivery for crawl + batch jobs.
pub(crate) mod webhook;

// ===================================================================
// Options & state
// ===================================================================

/// Server options assembled from `draco serve` flags. `defaults` seeds every
/// request's [`Config`]; per-request fields override it.
pub struct ServeOptions {
    pub host: String,
    pub port: u16,
    pub max_concurrency: usize,
    /// Warm Tier 2 workers to keep pooled (also caps concurrent isolates).
    pub isolate_pool_size: usize,
    /// Recycle a pooled worker after this many captures (leak hygiene).
    pub isolate_max_jobs: u32,
    pub defaults: Config,
}

pub(crate) struct AppState {
    pub(crate) defaults: Config,
    pub(crate) gate: Semaphore,
    /// Warm Tier 2 isolate pool: reused across requests so each scrape skips the
    /// jail spawn + snapshot cost. Its sandbox posture is fixed from `defaults`
    /// at startup; a request overriding the posture falls back to a one-shot
    /// capture inside the pool.
    pub(crate) tier2_pool: Tier2Pool,
    /// In-memory registry of async crawl jobs (`/v1/crawl`).
    pub(crate) crawl: jobs::JobStore,
    /// In-memory registry of async batch-scrape jobs (`/v1/batch/scrape`).
    pub(crate) batch: jobs::JobStore,
}

// ===================================================================
// Entry
// ===================================================================

/// Bind and run the daemon until ctrl-c / SIGTERM. Returns an error string only
/// for startup/bind failures (the caller maps it to a nonzero exit).
pub async fn serve(opts: ServeOptions) -> Result<(), String> {
    // The pool's workers inherit the daemon's default sandbox posture; per-request
    // posture overrides fall back to a one-shot capture (handled in the pool).
    let tier2_pool = Tier2Pool::new(
        opts.isolate_pool_size,
        opts.isolate_max_jobs,
        opts.defaults.no_jail,
        opts.defaults.strict_sandbox,
    );
    let state = Arc::new(AppState {
        defaults: opts.defaults,
        gate: Semaphore::new(opts.max_concurrency.max(1)),
        tier2_pool,
        crawl: jobs::JobStore::default(),
        batch: jobs::JobStore::default(),
    });
    let addr = format!("{}:{}", opts.host, opts.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    let local = listener.local_addr().map(|a| a.to_string()).unwrap_or(addr);
    eprintln!(
        "draco serve: listening on http://{local} (Firecrawl-compatible API at /v1/scrape); \
         warm isolate pool: {} workers",
        opts.isolate_pool_size
    );
    let result = axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| format!("server error: {e}"));
    // Retire pooled workers promptly on shutdown instead of leaving children to
    // exit on socket EOF.
    state.tier2_pool.shutdown();
    result
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/scrape", post(scrape))
        .route("/v1/map", post(map::map_handler))
        .route("/v1/search", post(search::search_handler))
        .route("/v1/discover", post(discover::discover_handler))
        .route("/v1/crawl", post(crawl::start_handler))
        .route(
            "/v1/crawl/{id}",
            get(crawl::status_handler).delete(crawl::cancel_handler),
        )
        .route("/v1/crawl/{id}/errors", get(crawl::errors_handler))
        .route("/v1/batch/scrape", post(batch::start_handler))
        .route(
            "/v1/batch/scrape/{id}",
            get(batch::status_handler).delete(batch::cancel_handler),
        )
        .route("/v1/batch/scrape/{id}/errors", get(batch::errors_handler))
        .route("/mcp", post(crate::mcp::http_handler))
        .with_state(state)
}

async fn shutdown_signal() {
    // Ctrl-C always; SIGTERM too on unix (containers / service managers).
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await;
    eprintln!("draco serve: shutting down");
}

// ===================================================================
// Handlers
// ===================================================================

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
}

/// Firecrawl-shaped scrape request. `onlyMainContent` and `waitFor` are
/// honored (see below); remaining unknown fields are deliberately ignored so
/// stock Firecrawl client payloads (`mobile`, `headers`, `includeTags`, …)
/// still deserialize cleanly. camelCase to match their wire format. The
/// `tierMax` / `captureWindowMs` / `noJail` / `allowUnsafeReplay` /
/// `ignoreRobots` / `proxy` fields are Draco extensions mirroring the CLI
/// flags.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScrapeRequest {
    url: String,
    #[serde(default)]
    formats: Vec<String>,
    /// Total request timeout in ms (Firecrawl field).
    #[serde(default)]
    timeout: Option<u64>,
    /// Strip boilerplate to the main content (Firecrawl field). Defaults to
    /// the daemon's `Config::only_main_content` default (`true`) when absent.
    #[serde(default)]
    only_main_content: Option<bool>,
    /// Firecrawl field: milliseconds to wait for the page to settle before
    /// extracting. Draco has no separate "wait" step — Tier 2's capture
    /// window already serves this purpose — so `waitFor` is treated as an
    /// alias for `captureWindowMs`: it only takes effect when the caller
    /// didn't also send an explicit `captureWindowMs` (see the handler).
    #[serde(default)]
    wait_for: Option<u64>,
    // ---- Draco extensions ------------------------------------------------
    #[serde(default)]
    tier_max: Option<u8>,
    #[serde(default)]
    capture_window_ms: Option<u64>,
    #[serde(default)]
    no_jail: Option<bool>,
    #[serde(default)]
    allow_unsafe_replay: Option<bool>,
    #[serde(default)]
    ignore_robots: Option<bool>,
    /// Surface Tier 2 page-side diagnostics as `runtime.log` trace steps
    /// (Draco extension; mirrors the CLI `--runtime-log` flag).
    #[serde(default)]
    runtime_log: Option<bool>,
    #[serde(default)]
    proxy: Option<String>,
    /// CSS selectors to keep (Firecrawl `includeTags`) / drop (`excludeTags`).
    #[serde(default)]
    include_tags: Option<Vec<String>>,
    #[serde(default)]
    exclude_tags: Option<Vec<String>>,
    /// Extra request headers forwarded to the fetch (Firecrawl `headers`).
    #[serde(default)]
    headers: Option<std::collections::HashMap<String, String>>,
}

async fn scrape(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ScrapeRequest>,
) -> (StatusCode, Json<Value>) {
    let formats = match parse_formats(&req.formats) {
        Ok(f) => f,
        Err(rej) => {
            let code = if rej.unsupported {
                StatusCode::UNPROCESSABLE_ENTITY
            } else {
                StatusCode::BAD_REQUEST
            };
            return (code, Json(error_body(&rej.message)));
        }
    };
    if req.url.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(error_body("\"url\" must be a non-empty string")),
        );
    }

    let config = Config {
        formats,
        only_main_content: req
            .only_main_content
            .unwrap_or(state.defaults.only_main_content),
        include_tags: req.include_tags.clone().unwrap_or_default(),
        exclude_tags: req.exclude_tags.clone().unwrap_or_default(),
        headers: req
            .headers
            .clone()
            .map(|m| m.into_iter().collect())
            .unwrap_or_default(),
        proxy: req.proxy.clone().or_else(|| state.defaults.proxy.clone()),
        timeout_ms: req.timeout.unwrap_or(state.defaults.timeout_ms),
        tier_max: req.tier_max.unwrap_or(state.defaults.tier_max),
        // `waitFor` is an alias for the capture window: an explicit
        // `captureWindowMs` always wins when both are given.
        capture_window_ms: req
            .capture_window_ms
            .or(req.wait_for)
            .unwrap_or(state.defaults.capture_window_ms),
        no_jail: req.no_jail.unwrap_or(state.defaults.no_jail),
        allow_unsafe_replay: req
            .allow_unsafe_replay
            .unwrap_or(state.defaults.allow_unsafe_replay),
        respect_robots: match req.ignore_robots {
            Some(ignore) => !ignore,
            None => state.defaults.respect_robots,
        },
        runtime_log: req.runtime_log.unwrap_or(state.defaults.runtime_log),
        force_render: false,
        ..state.defaults.clone()
    };

    // Bound concurrent extractions; queue (don't fail) when saturated. The
    // semaphore is never closed, so acquire can only fail on close — treat that
    // as a 503 just in case.
    let Ok(_permit) = state.gate.acquire().await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(error_body("server is shutting down")),
        );
    };
    let result = extract_with_pool(&req.url, &config, &state.tier2_pool).await;
    let (code, body) = to_firecrawl(&result);
    (code, Json(body))
}

// ===================================================================
// Mapping
// ===================================================================

/// A rejected `formats` entry, carrying whether it was *unknown* (HTTP 400 — we
/// don't recognize the token) or *unsupported* (HTTP 422 — recognized, but a
/// DOM-only engine can't satisfy it, e.g. `screenshot`).
#[derive(Debug)]
pub(crate) struct FormatReject {
    /// `true` → recognized but this engine can't produce it (map to 422);
    /// `false` → unknown token (map to 400).
    pub unsupported: bool,
    pub message: String,
}

impl FormatReject {
    fn unsupported(message: String) -> Self {
        Self {
            unsupported: true,
            message,
        }
    }
    fn unknown(message: String) -> Self {
        Self {
            unsupported: false,
            message,
        }
    }
}

/// Parse Firecrawl `formats` into a Draco [`FormatSet`]. Empty defaults to
/// `markdown` (Firecrawl's default). Supported: `markdown`, `html`, `rawHtml`,
/// `links`, `json`, `endpoints`. Browser-only formats (`screenshot`,
/// `screenshot@fullPage`, `actions`) and not-yet-implemented ones (`extract`,
/// `changeTracking`, `summary`, `branding`, `product`, `menu`) are rejected as
/// *unsupported* (422 — understood, but a DOM-only engine can't satisfy them);
/// anything else is *unknown* (400). A client asking for `screenshot` should get
/// a clear "needs a browser", not a silently different payload.
pub(crate) fn parse_formats(formats: &[String]) -> Result<FormatSet, FormatReject> {
    let mut set = FormatSet::none();
    for f in formats {
        match f.as_str() {
            "markdown" => set.markdown = true,
            "html" => set.html = true,
            "rawHtml" => set.raw_html = true,
            "links" => set.links = true,
            "json" => set.json = true,
            // Discovery: the ranked catalog of API endpoints the page calls.
            // Composes with the content formats and rides `data.endpoints`.
            "endpoints" => set.endpoints = true,
            "screenshot" | "screenshot@fullPage" | "actions" => {
                return Err(FormatReject::unsupported(format!(
                    "format {f:?} needs a real browser — Draco is a DOM-only engine \
                     and cannot capture screenshots or drive page actions"
                )));
            }
            "extract" | "changeTracking" | "summary" | "branding" | "product" | "menu" => {
                return Err(FormatReject::unsupported(format!(
                    "format {f:?} is not supported by this engine"
                )));
            }
            other => {
                return Err(FormatReject::unknown(format!(
                    "unknown format {other:?} — supported formats: \"markdown\", \
                     \"html\", \"rawHtml\", \"links\", \"json\", \"endpoints\""
                )));
            }
        }
    }
    // Empty `formats` → Firecrawl's default of markdown.
    if formats.is_empty() {
        set.markdown = true;
    }
    Ok(set)
}

/// Firecrawl error envelope.
pub(crate) fn error_body(message: &str) -> Value {
    json!({ "success": false, "error": message })
}

/// Whether a failed extraction was a `robots.txt` denial (draco-net's
/// [`draco_types::NetKind::Robots`]) rather than a transport/HTTP failure — so
/// the crawl/batch workers can route the URL to `robotsBlocked` instead of
/// `errors`, matching Firecrawl's split.
pub(crate) fn is_robots_blocked(result: &ExtractionResult) -> bool {
    matches!(
        &result.error,
        Some(DracoError::Network {
            reason: draco_types::NetKind::Robots,
            ..
        })
    )
}

/// Pagination query for async-job status endpoints (`?skip=&limit=`), shared by
/// `/v1/crawl/{id}` and `/v1/batch/scrape/{id}`. Both default to "from the
/// start, everything (up to the 10 MiB page cap)".
#[derive(Debug, Deserialize, Default)]
pub(crate) struct PageQuery {
    #[serde(default)]
    pub(crate) skip: Option<usize>,
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}

/// Map a terminal [`ExtractionResult`] to (HTTP status, Firecrawl body).
///
/// Each output rides on its presence in the result: the machine only populates
/// `markdown`/`html`/`rawHtml`/`links`/`data`/`endpoints` for formats the request
/// actually asked for, so emitting whatever is `Some` reproduces the requested
/// `formats` exactly — no separate format argument needed.
pub(crate) fn to_firecrawl(result: &ExtractionResult) -> (StatusCode, Value) {
    let draco_ext = json!({
        "sourceTier": result.source_tier,
        "timing": result.timing,
        "trace": result.trace,
    });

    if result.status == Status::Success {
        let mut data = serde_json::Map::new();
        if let Some(md) = &result.markdown {
            data.insert("markdown".into(), Value::String(md.clone()));
        }
        if let Some(h) = &result.html {
            data.insert("html".into(), Value::String(h.clone()));
        }
        if let Some(rh) = &result.raw_html {
            data.insert("rawHtml".into(), Value::String(rh.clone()));
        }
        if let Some(links) = &result.links {
            data.insert(
                "links".into(),
                serde_json::to_value(links).unwrap_or(Value::Null),
            );
        }
        if let Some(d) = &result.data {
            data.insert("json".into(), d.clone());
        }
        // The discovered API-endpoint catalog (the `endpoints` format), when
        // discovery ran. Rides `data.endpoints` alongside the content formats.
        if let Some(endpoints) = &result.endpoints {
            data.insert(
                "endpoints".into(),
                serde_json::to_value(endpoints).unwrap_or(Value::Null),
            );
        }
        // Draco's metadata is already Firecrawl-keyed (title, description,
        // og:*, sourceURL, statusCode, contentType). Synthesize the minimum
        // when the Markdown path didn't run (json-only requests).
        let metadata = result
            .metadata
            .clone()
            .unwrap_or_else(|| json!({ "sourceURL": result.url, "url": result.url }));
        data.insert("metadata".into(), metadata);
        let body = json!({ "success": true, "data": Value::Object(data), "draco": draco_ext });
        return (StatusCode::OK, body);
    }

    let code = match (result.status, &result.error) {
        // Upstream/network failure — Draco is the gateway to the target site.
        (Status::Error, Some(DracoError::Network { .. })) => StatusCode::BAD_GATEWAY,
        (Status::Error, _) => StatusCode::INTERNAL_SERVER_ERROR,
        // The ladder ran out of tiers / needs a real browser: the request was
        // well-formed but this target is beyond what the server can do.
        (Status::Unsupported | Status::NeedsBrowser, _) => StatusCode::UNPROCESSABLE_ENTITY,
        (Status::Success, _) => unreachable!("handled above"),
    };
    let mut body = error_body(&error_summary(result));
    body["draco"] = draco_ext;
    (code, body)
}

/// One-line human summary of a failed result for the `error` field.
pub(crate) fn error_summary(result: &ExtractionResult) -> String {
    match (&result.error, result.status) {
        (Some(DracoError::Network { reason, detail }), _) => {
            let reason = format!("{reason:?}").to_lowercase();
            format!("network error ({reason}): {detail}")
        }
        (Some(DracoError::Parse { detail }), _) => format!("parse error: {detail}"),
        (Some(DracoError::Jail { reason, detail }), _) => {
            let reason = format!("{reason:?}").to_lowercase();
            format!("sandbox error ({reason}): {detail}")
        }
        (Some(DracoError::Runtime { detail }), _) => format!("runtime error: {detail}"),
        (Some(DracoError::Ipc { detail }), _) => format!("ipc error: {detail}"),
        (Some(DracoError::Config { detail }), _) => format!("config error: {detail}"),
        (None, Status::Unsupported) => {
            "extraction unsupported for this target (exhausted the tier ladder)".into()
        }
        (None, Status::NeedsBrowser) => {
            "target needs a full browser (beyond the isolate's ceiling)".into()
        }
        (None, _) => "extraction failed".into(),
    }
}

// ===================================================================
// Tests
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use draco_types::Timing;
    use tower::ServiceExt;

    fn test_state(defaults: Config) -> Arc<AppState> {
        Arc::new(AppState {
            defaults,
            gate: Semaphore::new(2),
            tier2_pool: Tier2Pool::new(1, 100, true, false),
            crawl: jobs::JobStore::default(),
            batch: jobs::JobStore::default(),
        })
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    // ---- formats ----------------------------------------------------------

    #[test]
    fn formats_default_to_markdown() {
        assert_eq!(parse_formats(&[]).unwrap(), FormatSet::markdown_only());
        assert_eq!(
            parse_formats(&["markdown".into()]).unwrap(),
            FormatSet::markdown_only()
        );
    }

    #[test]
    fn formats_map_json_and_both() {
        assert_eq!(
            parse_formats(&["json".into()]).unwrap(),
            FormatSet::json_only()
        );
        assert_eq!(
            parse_formats(&["markdown".into(), "json".into()]).unwrap(),
            FormatSet {
                markdown: true,
                json: true,
                ..FormatSet::none()
            }
        );
    }

    #[test]
    fn endpoints_format_sets_discovery() {
        // Discovery alone → just the endpoints dimension set.
        assert_eq!(
            parse_formats(&["endpoints".into()]).unwrap(),
            FormatSet {
                endpoints: true,
                ..FormatSet::none()
            }
        );
        // Composes with markdown → markdown + endpoints.
        assert_eq!(
            parse_formats(&["markdown".into(), "endpoints".into()]).unwrap(),
            FormatSet {
                markdown: true,
                endpoints: true,
                ..FormatSet::none()
            }
        );
    }

    #[test]
    fn newly_supported_formats_succeed() {
        // html / rawHtml / links used to be rejected as unsupported; they're
        // now first-class formats the DOM-only engine can produce.
        assert_eq!(
            parse_formats(&["html".into(), "rawHtml".into(), "links".into()]).unwrap(),
            FormatSet {
                html: true,
                raw_html: true,
                links: true,
                ..FormatSet::none()
            }
        );
    }

    #[test]
    fn known_but_unsupported_formats_fail_loudly() {
        let err = parse_formats(&["screenshot".into()]).unwrap_err();
        assert!(err.unsupported, "{}", err.message);
        assert!(err.message.contains("real browser"), "{}", err.message);
        let err = parse_formats(&["bogus".into()]).unwrap_err();
        assert!(!err.unsupported, "{}", err.message);
        assert!(err.message.contains("unknown format"), "{}", err.message);
    }

    // ---- request deserialization -------------------------------------------

    #[test]
    fn firecrawl_client_payload_deserializes_with_unknown_fields() {
        // A realistic Firecrawl SDK payload: `onlyMainContent`/`waitFor` are
        // honored (see the dedicated tests below); genuinely unknown fields
        // (`mobile`, `headers`, …) must still be ignored rather than erroring.
        let req: ScrapeRequest = serde_json::from_value(json!({
            "url": "https://example.com",
            "formats": ["markdown"],
            "onlyMainContent": true,
            "waitFor": 123,
            "mobile": false,
            "timeout": 15000,
            "headers": { "User-Agent": "x" }
        }))
        .unwrap();
        assert_eq!(req.url, "https://example.com");
        assert_eq!(req.timeout, Some(15_000));
        assert_eq!(req.only_main_content, Some(true));
        assert_eq!(req.wait_for, Some(123));
        assert!(req.tier_max.is_none());
    }

    #[test]
    fn draco_extension_fields_deserialize() {
        let req: ScrapeRequest = serde_json::from_value(json!({
            "url": "https://example.com",
            "formats": ["json"],
            "tierMax": 1,
            "captureWindowMs": 500,
            "noJail": true,
            "allowUnsafeReplay": false,
            "ignoreRobots": true,
            "proxy": "http://127.0.0.1:8080"
        }))
        .unwrap();
        assert_eq!(req.tier_max, Some(1));
        assert_eq!(req.capture_window_ms, Some(500));
        assert_eq!(req.no_jail, Some(true));
        assert_eq!(req.ignore_robots, Some(true));
        assert_eq!(req.proxy.as_deref(), Some("http://127.0.0.1:8080"));
    }

    // ---- response mapping ---------------------------------------------------

    fn success_result() -> ExtractionResult {
        ExtractionResult {
            url: "https://site.example/a".into(),
            status: Status::Success,
            source_tier: None,
            // Baseline is a markdown-only extraction: `data` (the JSON-API
            // payload) rides on its own presence now that `to_firecrawl` no
            // longer takes a separate format argument, so tests that want
            // `data.json` in the body must set it explicitly (see
            // `json_format_attaches_data_json`).
            data: None,
            markdown: Some("# Title\n\nBody.".into()),
            metadata: Some(json!({
                "title": "Title",
                "sourceURL": "https://site.example/a",
                "statusCode": 200
            })),
            html: None,
            raw_html: None,
            links: None,
            endpoints: None,
            timing: Timing::default(),
            trace: vec![],
            error: None,
        }
    }

    #[test]
    fn success_maps_to_firecrawl_data_envelope() {
        let (code, body) = to_firecrawl(&success_result());
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body["success"], true);
        assert_eq!(body["data"]["markdown"], "# Title\n\nBody.");
        assert_eq!(
            body["data"]["metadata"]["sourceURL"],
            "https://site.example/a"
        );
        // markdown-only request: the JSON-API payload is not attached.
        assert!(body["data"].get("json").is_none());
        // The draco extension is always present.
        assert!(body["draco"].get("timing").is_some());
    }

    #[test]
    fn json_format_attaches_data_json() {
        let mut r = success_result();
        r.data = Some(json!({ "items": [1, 2] }));
        let (_, body) = to_firecrawl(&r);
        assert_eq!(body["data"]["json"]["items"][0], 1);
    }

    #[test]
    fn html_and_links_formats_attach_to_data() {
        // When the result carries html/links (the request asked for those
        // formats), to_firecrawl surfaces them under data.html / data.links.
        let mut r = success_result();
        r.html = Some("<h1>Title</h1><p>Body.</p>".into());
        r.links = Some(vec![
            "https://site.example/one".into(),
            "https://site.example/two".into(),
        ]);
        let (_, body) = to_firecrawl(&r);
        assert_eq!(body["data"]["html"], "<h1>Title</h1><p>Body.</p>");
        assert_eq!(body["data"]["links"][0], "https://site.example/one");
        assert_eq!(body["data"]["links"][1], "https://site.example/two");
    }

    #[test]
    fn json_only_synthesizes_minimal_metadata() {
        let mut r = success_result();
        r.markdown = None;
        r.metadata = None;
        let (_, body) = to_firecrawl(&r);
        assert_eq!(
            body["data"]["metadata"]["sourceURL"],
            "https://site.example/a"
        );
    }

    #[test]
    fn network_error_maps_to_bad_gateway() {
        let mut r = success_result();
        r.status = Status::Error;
        r.markdown = None;
        r.data = None;
        r.error = Some(DracoError::Network {
            reason: draco_types::NetKind::Timeout,
            detail: "connect timed out".into(),
        });
        let (code, body) = to_firecrawl(&r);
        assert_eq!(code, StatusCode::BAD_GATEWAY);
        assert_eq!(body["success"], false);
        let msg = body["error"].as_str().unwrap();
        assert!(msg.contains("connect timed out"), "{msg}");
    }

    #[test]
    fn robots_denial_is_detected_but_other_net_errors_are_not() {
        // A robots.txt denial (NetKind::Robots) → routed to robotsBlocked.
        let mut r = success_result();
        r.status = Status::Error;
        r.error = Some(DracoError::Network {
            reason: draco_types::NetKind::Robots,
            detail: "blocked by robots.txt: /private".into(),
        });
        assert!(is_robots_blocked(&r));

        // A plain HTTP/transport failure is NOT a robots block (→ errors).
        r.error = Some(DracoError::Network {
            reason: draco_types::NetKind::Status,
            detail: "HTTP 500".into(),
        });
        assert!(!is_robots_blocked(&r));

        // A success is not a robots block.
        assert!(!is_robots_blocked(&success_result()));
    }

    #[test]
    fn unsupported_maps_to_unprocessable() {
        let mut r = success_result();
        r.status = Status::Unsupported;
        r.markdown = None;
        r.data = None;
        let (code, body) = to_firecrawl(&r);
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["success"], false);
    }

    // ---- router-level (oneshot, no sockets) ---------------------------------

    #[tokio::test]
    async fn health_endpoint_reports_ok() {
        let app = router(test_state(Config::default()));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["status"], "ok");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn scrape_rejects_bad_format_before_extracting() {
        // "rawHtml" is a supported format now (see `newly_supported_formats_succeed`);
        // use an unrecognized token to exercise the pre-extraction 400 short-circuit.
        let app = router(test_state(Config::default()));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/scrape")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "url": "https://example.com", "formats": ["bogus"] }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["success"], false);
    }

    #[tokio::test]
    async fn scrape_rejects_empty_url() {
        let app = router(test_state(Config::default()));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/scrape")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({ "url": "  " }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// Full end-to-end through the router: a fixture HTTP server serves a real
    /// static article; POST /v1/scrape extracts it to Markdown via the actual
    /// ladder (tier 0 static path — no isolate needed).
    #[tokio::test]
    async fn scrape_end_to_end_static_page() {
        // Fixture site on an ephemeral port.
        let fixture = Router::new().route(
            "/article",
            get(|| async {
                axum::response::Html(
                    "<!doctype html><html><head><title>Fixture</title></head><body>\
                     <article><h1>Daemon Smoke</h1>\
                     <p>Served by the in-test fixture and scraped through the daemon's \
                     REST surface via the real extraction ladder.</p></article>\
                     </body></html>",
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, fixture).await.unwrap();
        });

        // Static-only config: the fixture page needs no isolate/jail.
        let defaults = Config {
            force_render: false,
            tier_max: 0,
            respect_robots: false,
            ..Config::default()
        };
        let app = router(test_state(defaults));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/scrape")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "url": format!("http://127.0.0.1:{port}/article") }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["success"], true);
        let md = body["data"]["markdown"].as_str().unwrap();
        assert!(md.contains("Daemon Smoke"), "markdown: {md}");
        assert_eq!(body["data"]["metadata"]["title"], "Fixture");
        assert_eq!(body["data"]["metadata"]["statusCode"], 200);
    }
}
