//! `POST /v1/crawl`, `GET|DELETE /v1/crawl/{id}` — Firecrawl-compatible async
//! crawl jobs.
//!
//! STUB: implementation pending (parallel workstream). Returns 501.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::Value;

use super::{error_body, AppState};

/// In-memory registry of crawl jobs. Internals are this module's business; the
/// daemon only constructs it with `Default` and threads it through `AppState`.
#[derive(Default)]
pub(crate) struct JobStore {}

pub(crate) async fn start_handler(
    State(_state): State<Arc<AppState>>,
    Json(_req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(error_body("/v1/crawl is not implemented yet")),
    )
}

pub(crate) async fn status_handler(
    State(_state): State<Arc<AppState>>,
    Path(_id): Path<String>,
) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(error_body("/v1/crawl is not implemented yet")),
    )
}

pub(crate) async fn cancel_handler(
    State(_state): State<Arc<AppState>>,
    Path(_id): Path<String>,
) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(error_body("/v1/crawl is not implemented yet")),
    )
}
