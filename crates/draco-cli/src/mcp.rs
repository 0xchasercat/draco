//! MCP (Model Context Protocol) server — Draco's scraping exposed as tools for
//! agent clients (Claude, editors, orchestrators).
//!
//! Two transports share one dispatch core ([`handle_message`]):
//!
//! - **stdio** (`draco mcp`): newline-delimited JSON-RPC 2.0 on stdin/stdout —
//!   MCP's stdio framing (one message per line, no `Content-Length` headers).
//!   stdout carries protocol messages *only*; anything else corrupts the
//!   stream, so incidental logging goes to stderr. Requests are processed
//!   sequentially: a stdio session has a single client, and ordered responses
//!   are simpler to reason about than interleaving.
//! - **HTTP** (`POST /mcp` on the daemon): the minimal Streamable-HTTP subset —
//!   a single JSON-RPC message per POST, answered with a single
//!   `application/json` response (`202 Accepted` for notifications). No SSE
//!   stream, no session management; stateless request/response is all the
//!   tools need today, and the subset is forward-compatible with clients that
//!   fall back from SSE.
//!
//! Protocol notes:
//! - Version negotiation: the client's requested `protocolVersion` is echoed
//!   when it's one we know (2025-06-18, 2025-03-26, 2024-11-05); anything else
//!   gets the latest we support. All three revisions are compatible for the
//!   feature set used here (tools only).
//! - The `initialize` → `notifications/initialized` lifecycle is *tolerated*
//!   but not enforced: every method works without a prior handshake. The HTTP
//!   binding is stateless, so enforcing per-connection lifecycle there would
//!   be fiction; the permissive behavior is uniform across transports.
//! - Tool-level failures (unreachable URL, unsupported target) are reported as
//!   tool *results* with `isError: true` — per spec, so the model can see and
//!   react to the failure — while protocol-level misuse (unknown tool, bad
//!   params) is a JSON-RPC error.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use draco_core::{extract, extract_with_pool, Config, OutputFormat, Tier2Pool};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Semaphore;

use crate::serve::{parse_formats, to_firecrawl, AppState};

/// Protocol revisions this server knows, newest first. The first entry is the
/// default offered to clients requesting an unknown revision.
const SUPPORTED_PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

// ===================================================================
// Transports
// ===================================================================

/// Run the MCP server over stdio until stdin closes. `defaults` seeds each
/// tool call's [`Config`]; per-call arguments override individual fields.
pub(crate) async fn run_stdio(defaults: Config) -> Result<(), String> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| format!("stdin read: {e}"))?
    {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            // stdio is a single-client session: no daemon gate, and no warm pool
            // (one-shot `extract` per call is fine for interactive use).
            Ok(msg) => handle_message(&msg, &defaults, None, None).await,
            Err(e) => Some(parse_error(&e)),
        };
        if let Some(resp) = response {
            let mut out = serde_json::to_string(&resp).map_err(|e| format!("serialize: {e}"))?;
            out.push('\n');
            stdout
                .write_all(out.as_bytes())
                .await
                .map_err(|e| format!("stdout write: {e}"))?;
            stdout
                .flush()
                .await
                .map_err(|e| format!("stdout flush: {e}"))?;
        }
    }
    Ok(())
}

/// `POST /mcp` — the same MCP server bound to the daemon. Tool calls inherit
/// the daemon's default [`Config`] and count against its concurrency gate.
pub(crate) async fn http_handler(
    State(state): State<Arc<AppState>>,
    body: String,
) -> (StatusCode, Json<Value>) {
    let msg = match serde_json::from_str::<Value>(&body) {
        Ok(m) => m,
        Err(e) => return (StatusCode::OK, Json(parse_error(&e))),
    };
    match handle_message(
        &msg,
        &state.defaults,
        Some(&state.gate),
        Some(&state.tier2_pool),
    )
    .await
    {
        Some(resp) => (StatusCode::OK, Json(resp)),
        // Notifications (and other id-less messages) get no JSON-RPC response.
        None => (StatusCode::ACCEPTED, Json(json!({}))),
    }
}

// ===================================================================
// Dispatch core
// ===================================================================

/// Handle one JSON-RPC message. Returns the response for requests, `None` for
/// notifications (which never get responses, including unknown ones). `gate`
/// bounds tool-call extractions on the daemon; stdio passes `None` (a single
/// stdio client processed sequentially needs no extra bound).
async fn handle_message(
    msg: &Value,
    defaults: &Config,
    gate: Option<&Semaphore>,
    pool: Option<&Tier2Pool>,
) -> Option<Value> {
    let id = msg.get("id").filter(|v| !v.is_null()).cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);

    // A message without a method is a client-side response/result — nothing for
    // a tools-only server to do with it.
    if method.is_empty() {
        return None;
    }

    let outcome: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(initialize_result(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => {
            Ok(json!({ "tools": [scrape_tool_descriptor(), discover_tool_descriptor()] }))
        }
        "tools/call" => call_tool(&params, defaults, gate, pool).await,
        _ => Err((-32601, format!("method not found: {method}"))),
    };

    // Notifications never get responses — success or failure.
    let id = id?;
    Some(match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message }
        }),
    })
}

/// JSON-RPC parse-error response (`id` is unknowable, so it's `null`).
fn parse_error(e: &serde_json::Error) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": { "code": -32700, "message": format!("parse error: {e}") }
    })
}

/// `initialize` result with version negotiation (see module docs).
fn initialize_result(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    let negotiated = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
        requested
    } else {
        SUPPORTED_PROTOCOL_VERSIONS[0]
    };
    json!({
        "protocolVersion": negotiated,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "draco",
            "title": "Draco web scraper",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": "Scrape web pages to clean Markdown (and structured JSON-API data) \
                         without a browser via the draco_scrape tool.",
    })
}

// ===================================================================
// The draco_scrape tool
// ===================================================================

/// Tool descriptor for `tools/list`.
fn scrape_tool_descriptor() -> Value {
    json!({
        "name": "draco_scrape",
        "title": "Scrape URL",
        "description": "Scrape a URL to clean Markdown of its main content (and/or the \
                        structured JSON data the page's own API serves) without a browser. \
                        Handles client-rendered SPAs by hydrating them in a sandboxed V8 \
                        isolate.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The http(s) URL to scrape."
                },
                "formats": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["markdown", "json"] },
                    "description": "Output formats; defaults to [\"markdown\"]. \"json\" is \
                                    the tiered JSON-API extraction."
                },
                "tierMax": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 2,
                    "description": "Cap the escalation ladder (0 static, 1 +build-id, 2 +runtime)."
                },
                "captureWindowMs": {
                    "type": "integer",
                    "description": "Tier 2 capture-window duration in ms."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Total request timeout in ms."
                },
                "ignoreRobots": {
                    "type": "boolean",
                    "description": "Bypass robots.txt."
                }
            },
            "required": ["url"]
        },
        "annotations": { "readOnlyHint": true, "openWorldHint": true }
    })
}

/// Descriptor for the `draco_discover` tool: surface the JSON/XHR API endpoints
/// a client-rendered page's own JavaScript calls, ranked, and replay the best
/// one — for callers who want the page's data API rather than its rendered text.
fn discover_tool_descriptor() -> Value {
    json!({
        "name": "draco_discover",
        "title": "Discover API endpoints",
        "description": "Discover the JSON/XHR API endpoints a page's JavaScript calls, ranked \
                        by how likely each is the real data API, and replay the best one. \
                        Returns the endpoint catalog (method, url, score, replayable, headers) \
                        plus the replayed winner's JSON. Use this to find and pull the API \
                        behind a client-rendered page instead of scraping its rendered text.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The http(s) URL to inspect." },
                "tierMax": { "type": "integer", "minimum": 2, "maximum": 2,
                             "description": "Must allow Tier 2 (the isolate); discovery needs it." },
                "captureWindowMs": { "type": "integer", "description": "Tier 2 capture-window duration in ms." },
                "timeout": { "type": "integer", "description": "Total request timeout in ms." },
                "ignoreRobots": { "type": "boolean", "description": "Bypass robots.txt." },
                "allowUnsafeReplay": { "type": "boolean",
                                       "description": "Mark non-idempotent (e.g. POST-mutation) endpoints replayable." }
            },
            "required": ["url"]
        },
        "annotations": { "readOnlyHint": true, "openWorldHint": true }
    })
}

/// `tools/call` dispatch. Protocol-level misuse (unknown tool, missing/invalid
/// params) is a JSON-RPC error; a scrape that *ran and failed* is a tool
/// result with `isError: true`.
async fn call_tool(
    params: &Value,
    defaults: &Config,
    gate: Option<&Semaphore>,
    pool: Option<&Tier2Pool>,
) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    // Two tools share the same execution path; `draco_discover` just forces
    // endpoint discovery and headlines the catalog in its result.
    let is_discover = name == "draco_discover";
    if name != "draco_scrape" && !is_discover {
        return Err((-32602, format!("unknown tool: {name:?}")));
    }
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let url = args
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .ok_or((-32602, "\"url\" (string) is required".to_string()))?;

    let formats: Vec<String> = args
        .get("formats")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let (format, discover) = parse_formats(&formats).map_err(|msg| (-32602, msg))?;

    let mut config = defaults.clone();
    config.format = format;
    // `draco_discover` always discovers (and replays the winner into `data`),
    // regardless of the `formats` argument.
    config.discover_endpoints = discover || is_discover;
    if is_discover {
        config.format = OutputFormat::Json;
    }
    if let Some(t) = args.get("tierMax").and_then(Value::as_u64) {
        config.tier_max = t.min(u8::MAX as u64) as u8;
    }
    if let Some(w) = args.get("captureWindowMs").and_then(Value::as_u64) {
        config.capture_window_ms = w;
    }
    if let Some(t) = args.get("timeout").and_then(Value::as_u64) {
        config.timeout_ms = t;
    }
    if let Some(true) = args.get("ignoreRobots").and_then(Value::as_bool) {
        config.respect_robots = false;
    }
    if let Some(true) = args.get("allowUnsafeReplay").and_then(Value::as_bool) {
        config.allow_unsafe_replay = true;
    }

    // Bound daemon-side tool calls with the shared gate (never closed in
    // practice; treat a closed gate as a failed tool call, not a protocol
    // error).
    let permit = match gate {
        Some(g) => match g.acquire().await {
            Ok(p) => Some(p),
            Err(_) => return Ok(tool_error("server is shutting down")),
        },
        None => None,
    };
    // Prefer the daemon's warm isolate pool when present (HTTP transport);
    // stdio has none and uses a one-shot capture.
    let result = match pool {
        Some(p) => extract_with_pool(url, &config, p).await,
        None => extract(url, &config).await,
    };
    drop(permit);

    let (code, body) = to_firecrawl(&result, format);
    if code != StatusCode::OK {
        let msg = body["error"].as_str().unwrap_or("extraction failed");
        return Ok(tool_error(&format!("{msg} (source: {url})")));
    }

    // Content assembly: agents want prose first — the markdown string itself is
    // the primary content item; the JSON-API payload (when requested) rides as
    // a second pretty-printed item. For discovery, the endpoint catalog leads.
    let data = &body["data"];
    let mut content = Vec::new();
    if is_discover {
        let endpoints = data.get("endpoints").cloned().unwrap_or(json!([]));
        let pretty =
            serde_json::to_string_pretty(&endpoints).unwrap_or_else(|_| endpoints.to_string());
        content.push(json!({ "type": "text", "text": pretty }));
    }
    if let Some(md) = data["markdown"].as_str() {
        content.push(json!({ "type": "text", "text": md }));
    }
    if matches!(format, OutputFormat::Json | OutputFormat::Both) {
        if let Some(d) = data.get("json") {
            let pretty = serde_json::to_string_pretty(d).unwrap_or_else(|_| d.to_string());
            content.push(json!({ "type": "text", "text": pretty }));
        }
    }
    if content.is_empty() {
        // Success with nothing renderable (e.g. json format matched no data
        // shape) — still a valid result; say so instead of returning nothing.
        content.push(json!({
            "type": "text",
            "text": format!("scrape succeeded but produced no {format:?} content for {url}")
        }));
    }
    Ok(json!({ "content": content, "isError": false }))
}

/// Tool-level failure result (`isError: true`), distinct from JSON-RPC errors.
fn tool_error(message: &str) -> Value {
    json!({
        "content": [ { "type": "text", "text": message } ],
        "isError": true
    })
}

// ===================================================================
// Tests
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use axum::routing::{get, post};
    use axum::Router;
    use tower::ServiceExt;

    fn defaults() -> Config {
        Config {
            tier_max: 0,
            respect_robots: false,
            ..Config::default()
        }
    }

    async fn dispatch(msg: Value) -> Option<Value> {
        // No gate, no pool: exercises the one-shot `extract` path.
        handle_message(&msg, &defaults(), None, None).await
    }

    // ---- initialize ---------------------------------------------------------

    #[tokio::test]
    async fn initialize_negotiates_known_and_unknown_versions() {
        let resp = dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-03-26", "capabilities": {},
                        "clientInfo": { "name": "t", "version": "0" } }
        }))
        .await
        .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(resp["result"]["serverInfo"]["name"], "draco");
        assert!(resp["result"]["capabilities"]["tools"].is_object());

        let resp = dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "initialize",
            "params": { "protocolVersion": "1999-01-01" }
        }))
        .await
        .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2025-06-18");
    }

    // ---- tools/list ---------------------------------------------------------

    #[tokio::test]
    async fn tools_list_advertises_scrape_and_discover() {
        let resp = dispatch(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
            .await
            .unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"draco_scrape"), "tools: {names:?}");
        assert!(names.contains(&"draco_discover"), "tools: {names:?}");
        // Both require a url and are read-only.
        for t in tools {
            assert_eq!(t["inputSchema"]["required"], json!(["url"]), "{t}");
            assert_eq!(t["annotations"]["readOnlyHint"], true, "{t}");
        }
    }

    // ---- protocol errors ----------------------------------------------------

    #[tokio::test]
    async fn unknown_method_and_tool_map_to_jsonrpc_errors() {
        let resp = dispatch(json!({ "jsonrpc": "2.0", "id": 1, "method": "bogus/method" }))
            .await
            .unwrap();
        assert_eq!(resp["error"]["code"], -32601);

        let resp = dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "not_a_tool", "arguments": {} }
        }))
        .await
        .unwrap();
        assert_eq!(resp["error"]["code"], -32602);

        // Missing url is invalid params too.
        let resp = dispatch(json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "draco_scrape", "arguments": {} }
        }))
        .await
        .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn notifications_get_no_response() {
        assert!(
            dispatch(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
                .await
                .is_none()
        );
        // Even an unknown notification is silently ignored (no id → no error).
        assert!(
            dispatch(json!({ "jsonrpc": "2.0", "method": "notifications/whatever" }))
                .await
                .is_none()
        );
    }

    // ---- HTTP binding -------------------------------------------------------

    fn mcp_router() -> Router {
        let state = Arc::new(AppState {
            defaults: defaults(),
            gate: Semaphore::new(2),
            tier2_pool: draco_core::Tier2Pool::new(1, 100, true, false),
            crawl: Default::default(),
        });
        Router::new()
            .route("/mcp", post(http_handler))
            .with_state(state)
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = to_bytes(resp.into_body(), 1 << 22).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn http_request_200_notification_202_parse_error() {
        let post_body = |body: String| {
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap()
        };

        // Request → 200 + JSON-RPC response.
        let resp = mcp_router()
            .oneshot(post_body(
                json!({ "jsonrpc": "2.0", "id": 7, "method": "ping" }).to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["id"], 7);
        assert!(body["result"].is_object());

        // Notification → 202, empty body.
        let resp = mcp_router()
            .oneshot(post_body(
                json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        // Malformed JSON → -32700 with null id.
        let resp = mcp_router()
            .oneshot(post_body("{not json".to_string()))
            .await
            .unwrap();
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], -32700);
        assert!(body["id"].is_null());
    }

    // ---- end-to-end tool call -----------------------------------------------

    /// A real tools/call against a local fixture article: the primary content
    /// item is the page's Markdown.
    #[tokio::test]
    async fn scrape_tool_end_to_end() {
        let fixture = Router::new().route(
            "/article",
            get(|| async {
                axum::response::Html(
                    "<!doctype html><html><head><title>MCP Fixture</title></head><body>\
                     <article><h1>Tool Call Smoke</h1>\
                     <p>Scraped through the MCP dispatch core by the draco_scrape tool, \
                     via the real extraction ladder against a local fixture.</p></article>\
                     </body></html>",
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, fixture).await.unwrap();
        });

        let resp = dispatch(json!({
            "jsonrpc": "2.0", "id": 42, "method": "tools/call",
            "params": {
                "name": "draco_scrape",
                "arguments": { "url": format!("http://127.0.0.1:{port}/article") }
            }
        }))
        .await
        .unwrap();
        let result = &resp["result"];
        assert_eq!(result["isError"], false, "result: {result}");
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Tool Call Smoke"), "content: {text}");
    }

    /// An unreachable target is a TOOL-level failure (isError), not a JSON-RPC
    /// error.
    #[tokio::test]
    async fn unreachable_target_is_tool_error() {
        let resp = dispatch(json!({
            "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": {
                "name": "draco_scrape",
                "arguments": { "url": "http://127.0.0.1:9/nope", "timeout": 2000 }
            }
        }))
        .await
        .unwrap();
        assert!(resp.get("error").is_none(), "resp: {resp}");
        assert_eq!(resp["result"]["isError"], true);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("http://127.0.0.1:9/nope"), "text: {text}");
    }
}
