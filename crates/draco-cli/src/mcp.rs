//! MCP (Model Context Protocol) server — stdio transport (`draco mcp`) and an
//! HTTP binding on the daemon (`POST /mcp`).
//!
//! STUB: implementation pending (parallel workstream).

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use draco_core::Config;
use serde_json::{json, Value};

use crate::serve::AppState;

/// Run the MCP server over stdio (newline-delimited JSON-RPC 2.0) until stdin
/// closes. `defaults` seeds each tool call's [`Config`].
pub(crate) async fn run_stdio(_defaults: Config) -> Result<(), String> {
    Err("MCP stdio transport is not implemented yet".into())
}

/// `POST /mcp` — the same MCP server bound to the daemon.
pub(crate) async fn http_handler(
    State(_state): State<Arc<AppState>>,
    _body: String,
) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": { "code": -32603, "message": "MCP HTTP transport is not implemented yet" }
        })),
    )
}
