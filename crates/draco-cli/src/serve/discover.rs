//! `POST /v1/discover` — API endpoint discovery + winner replay.
//!
//! The dedicated surface for Draco's "what JSON APIs does this page call?"
//! capability: it runs the Tier 2 isolate, watches the page's `fetch`/XHR,
//! ranks them, and returns the catalog — plus the replayed winner's JSON as
//! `data` (discovery *and* replay). It's the endpoint analog of `/v1/map`
//! (which returns discovered *links*): a focused convenience route on top of
//! the same machinery `/v1/scrape` exposes via `formats: ["endpoints"]`.
//!
//! Response (top-level, like `/v1/map`'s `links`):
//! `{ "success": true, "endpoints": [ … ], "data": <winner JSON | null>, "draco": {…} }`.
//! Unknown request fields are ignored; the request mirrors `/v1/scrape`'s Draco
//! extensions (`tierMax`, `captureWindowMs`, `timeout`, `noJail`,
//! `ignoreRobots`, `allowUnsafeReplay`, `proxy`).

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use draco_core::{extract_with_pool, Config, FormatSet};
use draco_types::Status;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{error_body, error_summary, AppState};

/// Discovery request. Same shape as a scrape's Draco extensions; camelCase.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiscoverRequest {
    url: String,
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
    /// Surface Tier 2 page-side diagnostics as `runtime.log` trace steps
    /// (Draco extension; mirrors the CLI `--runtime-log` flag).
    #[serde(default)]
    runtime_log: Option<bool>,
    #[serde(default)]
    proxy: Option<String>,
}

pub(crate) async fn discover_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<DiscoverRequest>,
) -> (StatusCode, Json<Value>) {
    if req.url.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(error_body("\"url\" must be a non-empty string")),
        );
    }

    // Discovery needs the isolate; default the ladder to Tier 2 and force the
    // discovery flag. The content dimension is Json so the winner is replayed
    // into `data` (discovery + replay).
    let config = Config {
        // Discovery + replay: the ranked catalog plus the winner replayed into
        // `data`. Endpoints forces the Tier 2 capture; json carries the winner.
        formats: FormatSet {
            json: true,
            endpoints: true,
            ..FormatSet::none()
        },
        proxy: req.proxy.clone().or_else(|| state.defaults.proxy.clone()),
        timeout_ms: req.timeout.unwrap_or(state.defaults.timeout_ms),
        tier_max: req.tier_max.unwrap_or(2).max(2),
        capture_window_ms: req
            .capture_window_ms
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
        ..state.defaults.clone()
    };

    let Ok(_permit) = state.gate.acquire().await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(error_body("server is shutting down")),
        );
    };
    let result = extract_with_pool(&req.url, &config, &state.tier2_pool).await;

    // A hard failure (e.g. the page couldn't be fetched, or the isolate errored)
    // surfaces as the Firecrawl error envelope; otherwise the catalog + winner.
    if result.status != Status::Success {
        let code = match result.status {
            Status::Error => StatusCode::BAD_GATEWAY,
            _ => StatusCode::UNPROCESSABLE_ENTITY,
        };
        return (code, Json(error_body(&error_summary(&result))));
    }

    let endpoints = result
        .endpoints
        .as_ref()
        .and_then(|e| serde_json::to_value(e).ok())
        .unwrap_or_else(|| json!([]));
    let body = json!({
        "success": true,
        "endpoints": endpoints,
        "data": result.data.clone().unwrap_or(Value::Null),
        "draco": {
            "sourceTier": result.source_tier,
            "timing": result.timing,
            "trace": result.trace,
        }
    });
    (StatusCode::OK, Json(body))
}
