//! `POST /v1/map` — Firecrawl-compatible site URL discovery.
//!
//! STUB: implementation pending (parallel workstream). Returns 501.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::Value;

use super::{error_body, AppState};

pub(crate) async fn map_handler(
    State(_state): State<Arc<AppState>>,
    Json(_req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(error_body("/v1/map is not implemented yet")),
    )
}
