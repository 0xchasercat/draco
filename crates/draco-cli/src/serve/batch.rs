//! `POST /v1/batch/scrape`, `GET|DELETE /v1/batch/scrape/{id}` — Firecrawl-
//! compatible batch scraping: hand it a list of URLs, get a job id, poll for the
//! accumulated per-URL results.
//!
//! Batch is a crawl without the graph: every URL is known upfront (so `total` is
//! fixed at admission — no frontier growth), and each is run through the full
//! extraction ladder ([`draco_core::extract`]) independently. Unlike `/v1/crawl`,
//! the scrape options are **flat** at the top level (matching Firecrawl's batch
//! request), not nested under `scrapeOptions`.
//!
//! Parallelism is bounded by the daemon-wide concurrency gate (`--max-concurrency`):
//! every URL is dispatched as a task that acquires a permit before extracting, so
//! a 1000-URL batch saturates exactly the configured budget and no more, sharing
//! it with interactive `/v1/scrape` traffic. Results accumulate in completion
//! order; a cancelled job stops dispatching and keeps what it has.
//!
//! Status polling, pagination (`?skip=&limit=`, 10 MiB page cap), the `/errors`
//! endpoint, and cancellation all come from the shared [`super::jobs::JobStore`].

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use draco_core::{extract_with_pool, Config};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{error_body, parse_formats, to_firecrawl, AppState, PageQuery};

/// Firecrawl-shaped batch request (camelCase; unknown fields ignored). Scrape
/// options are flat at the top level — the same fields as `/v1/scrape`, applied
/// to every URL.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BatchRequest {
    urls: Vec<String>,
    #[serde(default)]
    formats: Vec<String>,
    #[serde(default)]
    only_main_content: Option<bool>,
    #[serde(default)]
    include_tags: Option<Vec<String>>,
    #[serde(default)]
    exclude_tags: Option<Vec<String>>,
    #[serde(default)]
    headers: Option<HashMap<String, String>>,
    #[serde(default)]
    wait_for: Option<u64>,
    /// Drop URLs that aren't valid http(s) and report them in `invalidURLs`
    /// instead of failing the whole request (Firecrawl semantics). Explicit
    /// rename: camelCase would give `ignoreInvalidUrls`, but Firecrawl's wire
    /// field capitalizes the acronym (`ignoreInvalidURLs`).
    #[serde(default, rename = "ignoreInvalidURLs")]
    ignore_invalid_urls: bool,
    // ---- Draco extensions (mirror /v1/scrape) ----------------------------
    #[serde(default)]
    timeout: Option<u64>,
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
    #[serde(default)]
    proxy: Option<String>,
}

pub(crate) async fn start_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<BatchRequest>,
) -> (StatusCode, Json<Value>) {
    if req.urls.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(error_body("\"urls\" must be a non-empty array")),
        );
    }

    // Formats parse the same way as /v1/scrape (400 unknown / 422 needs-browser).
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

    // Partition URLs into valid http(s) and invalid. With ignoreInvalidURLs the
    // invalid ones are dropped and reported; otherwise any invalid URL is a 400.
    let mut valid = Vec::new();
    let mut invalid = Vec::new();
    for u in &req.urls {
        match super::map::parse_http_url(u) {
            Ok(url) => valid.push(url.to_string()),
            Err(_) => invalid.push(u.clone()),
        }
    }
    if !invalid.is_empty() && !req.ignore_invalid_urls {
        return (
            StatusCode::BAD_REQUEST,
            Json(error_body(&format!(
                "invalid URL(s): {} — set ignoreInvalidURLs to skip them",
                invalid.join(", ")
            ))),
        );
    }
    if valid.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(error_body("no valid URLs to scrape")),
        );
    }

    let mut config = state.defaults.clone();
    config.formats = formats;
    config.only_main_content = req
        .only_main_content
        .unwrap_or(state.defaults.only_main_content);
    config.include_tags = req.include_tags.clone().unwrap_or_default();
    config.exclude_tags = req.exclude_tags.clone().unwrap_or_default();
    config.headers = req
        .headers
        .clone()
        .map(|m| m.into_iter().collect())
        .unwrap_or_default();
    if let Some(p) = req.proxy.clone() {
        config.proxy = Some(p);
    }
    config.timeout_ms = req.timeout.unwrap_or(state.defaults.timeout_ms);
    // waitFor is an alias for the capture window; explicit captureWindowMs wins.
    config.capture_window_ms = req
        .capture_window_ms
        .or(req.wait_for)
        .unwrap_or(state.defaults.capture_window_ms);
    config.tier_max = req.tier_max.unwrap_or(state.defaults.tier_max);
    config.no_jail = req.no_jail.unwrap_or(state.defaults.no_jail);
    config.allow_unsafe_replay = req
        .allow_unsafe_replay
        .unwrap_or(state.defaults.allow_unsafe_replay);
    config.respect_robots = match req.ignore_robots {
        Some(ignore) => !ignore,
        None => state.defaults.respect_robots,
    };

    let id = state.batch.create_with_total(valid.len());
    tokio::spawn(run_batch(state.clone(), id.clone(), valid, config));

    let mut body = json!({
        "success": true,
        "id": id,
        "url": format!("/v1/batch/scrape/{id}"),
    });
    // Only surface invalidURLs when the caller opted into ignoring them.
    if req.ignore_invalid_urls {
        body["invalidURLs"] = json!(invalid);
    }
    (StatusCode::OK, Json(body))
}

pub(crate) async fn status_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(page): Query<PageQuery>,
) -> (StatusCode, Json<Value>) {
    let next_base = format!("/v1/batch/scrape/{id}");
    match state
        .batch
        .snapshot(&id, page.skip.unwrap_or(0), page.limit, &next_base)
    {
        Some(body) => (StatusCode::OK, Json(body)),
        None => (
            StatusCode::NOT_FOUND,
            Json(error_body("batch scrape job not found")),
        ),
    }
}

/// `GET /v1/batch/scrape/{id}/errors` — per-URL failures + robots-blocked URLs.
pub(crate) async fn errors_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    match state.batch.errors_snapshot(&id) {
        Some(body) => (StatusCode::OK, Json(body)),
        None => (
            StatusCode::NOT_FOUND,
            Json(error_body("batch scrape job not found")),
        ),
    }
}

pub(crate) async fn cancel_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    if state.batch.cancel(&id) {
        (
            StatusCode::OK,
            Json(json!({ "success": true, "status": "cancelled" })),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(error_body("batch scrape job not found")),
        )
    }
}

/// The batch worker: dispatch every URL as a task that acquires the daemon gate
/// before extracting, so concurrency is bounded by `--max-concurrency`. Records
/// each result (or error) as it finishes; a cancelled job stops dispatching.
async fn run_batch(state: Arc<AppState>, id: String, urls: Vec<String>, config: Config) {
    let mut set = tokio::task::JoinSet::new();
    for url in urls {
        let state = state.clone();
        let id = id.clone();
        let config = config.clone();
        set.spawn(async move {
            if state.batch.is_cancelled(&id) {
                return;
            }
            let Ok(_permit) = state.gate.acquire().await else {
                return; // Gate closed: daemon shutting down.
            };
            if state.batch.is_cancelled(&id) {
                return;
            }
            let result = extract_with_pool(&url, &config, &state.tier2_pool).await;
            if super::is_robots_blocked(&result) {
                state.batch.record_robots_blocked(&id, &url);
                state.batch.record_page(&id, None);
                return;
            }
            let (code, mut body) = to_firecrawl(&result);
            if code == StatusCode::OK {
                state.batch.record_page(&id, Some(body["data"].take()));
            } else {
                let msg = body["error"]
                    .as_str()
                    .unwrap_or("extraction failed")
                    .to_string();
                state.batch.record_error(&id, &url, &msg);
                state.batch.record_page(&id, None);
            }
        });
    }
    while set.join_next().await.is_some() {}
    state.batch.finish(&id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use axum::routing::{get, post};
    use axum::Router;
    use tokio::sync::Semaphore;
    use tower::ServiceExt;

    fn test_state() -> Arc<AppState> {
        Arc::new(AppState {
            defaults: Config {
                tier_max: 0,
                respect_robots: false,
                ..Config::default()
            },
            gate: Semaphore::new(2),
            tier2_pool: draco_core::Tier2Pool::new(1, 100, true, false),
            crawl: Default::default(),
            batch: Default::default(),
        })
    }

    fn batch_router(state: Arc<AppState>) -> Router {
        Router::new()
            .route("/v1/batch/scrape", post(start_handler))
            .route(
                "/v1/batch/scrape/{id}",
                get(status_handler).delete(cancel_handler),
            )
            .route("/v1/batch/scrape/{id}/errors", get(errors_handler))
            .with_state(state)
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = to_bytes(resp.into_body(), 1 << 22).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn post_batch(app: &Router, payload: Value) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/batch/scrape")
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn empty_urls_is_bad_request() {
        let app = batch_router(test_state());
        let resp = post_batch(&app, json!({ "urls": [] })).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn invalid_url_rejected_without_ignore_flag() {
        let app = batch_router(test_state());
        let resp = post_batch(&app, json!({ "urls": ["not a url"] })).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["success"], false);
    }

    #[tokio::test]
    async fn invalid_url_reported_when_ignored() {
        let app = batch_router(test_state());
        // One valid, one invalid; ignoreInvalidURLs → 200 + invalidURLs list.
        let resp = post_batch(
            &app,
            json!({
                "urls": ["https://valid.example/", "not a url"],
                "ignoreInvalidURLs": true
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["success"], true);
        assert!(body["id"].is_string());
        assert_eq!(
            body["url"],
            format!("/v1/batch/scrape/{}", body["id"].as_str().unwrap())
        );
        let invalid = body["invalidURLs"].as_array().unwrap();
        assert_eq!(invalid.len(), 1);
        assert_eq!(invalid[0], "not a url");
    }

    #[tokio::test]
    async fn bad_format_is_bad_request() {
        let app = batch_router(test_state());
        let resp = post_batch(
            &app,
            json!({ "urls": ["https://x.example/"], "formats": ["bogus"] }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn screenshot_format_is_unprocessable() {
        let app = batch_router(test_state());
        let resp = post_batch(
            &app,
            json!({ "urls": ["https://x.example/"], "formats": ["screenshot"] }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn unknown_job_is_404() {
        let app = batch_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/batch/scrape/does-not-exist")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
