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
//!   `application/json` response (`202 Accepted` for notifications). There is no
//!   SSE stream or MCP transport session; daemon-scoped interact ids provide the
//!   only cross-call state. The subset remains forward-compatible with clients
//!   that fall back from SSE.
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
use draco_core::{extract, extract_with_pool, session_opts, Config, FormatSet, Tier2Pool};
use draco_types::Status;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Semaphore;

#[cfg(feature = "tier2")]
use crate::serve::interact::{SessionStore, SessionStoreError};
use crate::serve::{parse_formats, search, to_firecrawl, AppState};

/// Protocol revisions this server knows, newest first. The first entry is the
/// default offered to clients requesting an unknown revision.
const SUPPORTED_PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

#[cfg(feature = "tier2")]
type InteractStoreRef<'a> = Option<&'a SessionStore>;
#[cfg(not(feature = "tier2"))]
type InteractStoreRef<'a> = ();

// ===================================================================
// Transports
// ===================================================================

/// Run the MCP server over stdio until stdin closes. `defaults` seeds each
/// tool call's [`Config`]; per-call arguments override individual fields.
pub(crate) async fn run_stdio(defaults: Config) -> Result<(), String> {
    // A persistent interact registry for the stdio session so the
    // `draco_interact_*` tools are advertised AND usable over stdio — the
    // transport MCP clients (Claude Desktop/Code, editors) actually use. A stdio
    // process is one long-lived single client, so one store held for the life of
    // the loop is exactly right: `open` on one message, `exec`/`navigate` on the
    // next, `close` later, all against the same sessions (idle-reaped meanwhile).
    // Sized like the daemon's auto isolate pool (≈ CPU count); each live session
    // is a V8 isolate. tier2-only — a lean build has no interact surface.
    #[cfg(feature = "tier2")]
    let session_store = SessionStore::new(
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4),
    );

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
        #[cfg(feature = "tier2")]
        let sessions: InteractStoreRef<'_> = Some(&session_store);
        #[cfg(not(feature = "tier2"))]
        let sessions: InteractStoreRef<'_> = ();
        let response = match serde_json::from_str::<Value>(&line) {
            // stdio is a single-client session: no daemon gate or warm pool. It
            // does keep a persistent interact registry (above) so the session
            // tools work across messages.
            Ok(msg) => handle_message(&msg, &defaults, None, None, sessions).await,
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
    #[cfg(feature = "tier2")]
    let sessions: InteractStoreRef<'_> = Some(&state.sessions);
    #[cfg(not(feature = "tier2"))]
    let sessions: InteractStoreRef<'_> = ();
    match handle_message(
        &msg,
        &state.defaults,
        Some(&state.gate),
        Some(&state.tier2_pool),
        sessions,
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
    sessions: InteractStoreRef<'_>,
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
        "tools/list" => Ok(json!({
            "tools": tool_descriptors(interact_available(sessions))
        })),
        "tools/call" => call_tool(&params, defaults, gate, pool, sessions).await,
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

#[cfg(feature = "tier2")]
fn interact_available(store: InteractStoreRef<'_>) -> bool {
    store.is_some()
}

#[cfg(not(feature = "tier2"))]
fn interact_available(_store: InteractStoreRef<'_>) -> bool {
    false
}

fn tool_descriptors(interact: bool) -> Vec<Value> {
    let tools = vec![
        scrape_tool_descriptor(),
        discover_tool_descriptor(),
        search_tool_descriptor(),
        map_tool_descriptor(),
        crawl_tool_descriptor(),
        batch_scrape_tool_descriptor(),
    ];
    #[cfg(feature = "tier2")]
    {
        let mut tools = tools;
        if interact {
            tools.extend(interact_tool_descriptors());
        }
        tools
    }
    #[cfg(not(feature = "tier2"))]
    {
        let _ = interact;
        tools
    }
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
                    "items": { "type": "string", "enum": ["markdown", "html", "rawHtml", "links", "json", "endpoints"] },
                    "description": "Output formats; defaults to [\"markdown\"]. \"html\" is \
                                    cleaned main-content HTML, \"rawHtml\" the unmodified fetch, \
                                    \"links\" every absolutized link, \"json\" the tiered JSON-API \
                                    extraction, \"endpoints\" the ranked API-endpoint catalog."
                },
                "onlyMainContent": {
                    "type": "boolean",
                    "description": "Strip nav/header/footer/ads to the main content (default true)."
                },
                "waitFor": {
                    "type": "integer",
                    "description": "Alias for captureWindowMs: ms to let the page settle (Tier 2)."
                },
                "includeTags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "CSS selectors to keep; only matching subtrees survive into markdown/html."
                },
                "excludeTags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "CSS selectors to drop before extraction."
                },
                "extract": {
                    "type": "object",
                    "description": "Selector-schema structured extraction: an object mapping \
                                    output field names to CSS-selector specs (string shorthand, \
                                    or {selector, all, attr, fields} for arrays/attributes/nested \
                                    objects). Result rides the response as `extract`."
                },
                "headers": {
                    "type": "object",
                    "description": "Extra request headers (name→value) sent with the fetch."
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
                },
                "runtimeLog": {
                    "type": "boolean",
                    "description": "Surface Tier 2 page-side diagnostics (swallowed exceptions, \
                                    console.error lines) as runtime.log trace steps."
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
                                       "description": "Mark non-idempotent (e.g. POST-mutation) endpoints replayable." },
                "runtimeLog": { "type": "boolean",
                                "description": "Surface Tier 2 page-side diagnostics (swallowed exceptions, \
                                                console.error lines) as runtime.log trace steps." }
            },
            "required": ["url"]
        },
        "annotations": { "readOnlyHint": true, "openWorldHint": true }
    })
}

/// Descriptor for `draco_search`: metasearch across several engines over plain
/// HTTP (no browser), merged by reciprocal-rank consensus, tolerant of
/// individual engine failures. Optionally scrapes each result URL.
fn search_tool_descriptor() -> Value {
    json!({
        "name": "draco_search",
        "title": "Web search",
        "description": "Search the web across several engines in parallel over plain HTTP \
                        (no browser), merged by reciprocal-rank consensus so a captcha-walled \
                        or geo-blocked engine degrades gracefully instead of failing the query. \
                        Returns ranked results (title, description, url). If `formats` is given, \
                        each result URL is also scraped and its content merged onto the hit.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "The search query." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 100,
                           "description": "Max results after consensus (default 5)." },
                "tbs": { "type": "string", "description": "Time filter (e.g. qdr:d); best-effort per engine." },
                "location": { "type": "string", "description": "Free-text geo target; best-effort per engine." },
                "timeout": { "type": "integer", "description": "Overall search deadline in ms (default 60000)." },
                "formats": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["markdown", "html", "rawHtml", "links", "json", "endpoints"] },
                    "description": "If non-empty, scrape each result URL to these formats and merge the fields onto the result."
                }
            },
            "required": ["query"]
        },
        "annotations": { "readOnlyHint": true, "openWorldHint": true }
    })
}

/// Descriptor for `draco_map`: sitemap + on-page link discovery, the MCP face
/// of the daemon's `/v1/map`.
fn map_tool_descriptor() -> Value {
    json!({
        "name": "draco_map",
        "title": "Map site links",
        "description": "Discover a site's URLs fast: robots.txt sitemaps, the default \
                        sitemap, and on-page links, merged/deduped/filtered — no browser, \
                        a couple of fetches. The reconnaissance step before draco_crawl or \
                        draco_batch_scrape.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The site (or section) to map." },
                "search": { "type": "string", "description": "Case-insensitive substring filter on the URL list." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 5000,
                           "description": "Max links to return (default 100)." },
                "includeSubdomains": { "type": "boolean", "description": "Treat subdomains as same-site (default true)." },
                "ignoreSitemap": { "type": "boolean", "description": "Skip sitemap sources; on-page hrefs only." },
                "timeout": { "type": "integer", "description": "Per-fetch timeout in ms." },
                "ignoreRobots": { "type": "boolean", "description": "Bypass robots.txt." }
            },
            "required": ["url"]
        },
        "annotations": { "readOnlyHint": true, "openWorldHint": true }
    })
}

/// Descriptor for `draco_crawl`: a bounded synchronous crawl sized for agent
/// tool calls. Large async jobs stay on the daemon's `/v1/crawl`.
fn crawl_tool_descriptor() -> Value {
    json!({
        "name": "draco_crawl",
        "title": "Crawl site (bounded)",
        "description": "Bounded synchronous crawl: map the site's links, then scrape the \
                        first N same-site pages inline and return their content in one \
                        response (limit ≤ 25). Per-page failures are reported inline, not \
                        fatal. For large or long-running crawls use POST /v1/crawl on the \
                        daemon, which runs as an async job.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The site (or section) to crawl; scraped first." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 25,
                           "description": "Max pages to scrape including the seed (default 10)." },
                "search": { "type": "string", "description": "Substring filter applied when picking mapped links." },
                "formats": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["markdown", "html", "rawHtml", "links"] },
                    "description": "Per-page output formats; defaults to [\"markdown\"]."
                },
                "extract": { "type": "object", "description": "Selector-schema extraction run on every page." },
                "onlyMainContent": { "type": "boolean", "description": "Strip chrome to main content (default true)." },
                "timeout": { "type": "integer", "description": "Per-page timeout in ms." },
                "ignoreRobots": { "type": "boolean", "description": "Bypass robots.txt." }
            },
            "required": ["url"]
        },
        "annotations": { "readOnlyHint": true, "openWorldHint": true }
    })
}

/// Descriptor for `draco_batch_scrape`: the same bounded inline loop over a
/// caller-supplied URL list.
fn batch_scrape_tool_descriptor() -> Value {
    json!({
        "name": "draco_batch_scrape",
        "title": "Batch scrape URLs",
        "description": "Scrape up to 25 URLs in one call and return each page's content \
                        (per-URL failures reported inline). For larger batches use \
                        POST /v1/batch/scrape on the daemon, which runs as an async job.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "urls": {
                    "type": "array",
                    "items": { "type": "string" },
                    "maxItems": 25,
                    "description": "The http(s) URLs to scrape."
                },
                "formats": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["markdown", "html", "rawHtml", "links", "json", "endpoints"] },
                    "description": "Per-page output formats; defaults to [\"markdown\"]."
                },
                "extract": { "type": "object", "description": "Selector-schema extraction run on every page." },
                "onlyMainContent": { "type": "boolean", "description": "Strip chrome to main content (default true)." },
                "timeout": { "type": "integer", "description": "Per-page timeout in ms." },
                "ignoreRobots": { "type": "boolean", "description": "Bypass robots.txt." }
            },
            "required": ["urls"]
        },
        "annotations": { "readOnlyHint": true, "openWorldHint": true }
    })
}

#[cfg(feature = "tier2")]
fn interact_tool_descriptors() -> Vec<Value> {
    vec![
        json!({
            "name": "draco_interact_open",
            "title": "Open interact session",
            "description": "Open a resumable DOM session for a URL and return its id plus \
                            initial snapshot.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The http(s) URL to open." }
                },
                "required": ["url"]
            },
            "annotations": { "readOnlyHint": false, "openWorldHint": true }
        }),
        json!({
            "name": "draco_interact_exec",
            "title": "Execute JavaScript in session",
            "description": "Run one async JavaScript body in the live page scope and return \
                            its value and console output.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string" },
                    "js": { "type": "string" },
                    "full": {
                        "type": "boolean",
                        "description": "Return the full value regardless of maxBytes."
                    },
                    "maxBytes": { "type": "integer", "minimum": 1,
                                  "description": "Approximate serialized result budget." }
                },
                "required": ["sessionId", "js"]
            },
            "annotations": { "readOnlyHint": false, "openWorldHint": true }
        }),
        json!({
            "name": "draco_interact_act",
            "title": "Act in interact session",
            "description": "Dispatch faithful DOM interactions in order, settle after each, \
                            and return the step trace plus the post-action page snapshot.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string" },
                    "actions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "type": {
                                    "type": "string",
                                    "enum": ["click", "type", "press", "scroll", "select", "hover", "wait"]
                                }
                            },
                            "required": ["type"],
                            "additionalProperties": true
                        }
                    },
                    "extract": {
                        "type": "object",
                        "description": "Selector-schema extraction run on the post-action DOM."
                    }
                },
                "required": ["sessionId", "actions"]
            },
            "annotations": { "readOnlyHint": false, "openWorldHint": true }
        }),
        json!({
            "name": "draco_interact_navigate",
            "title": "Navigate interact session",
            "description": "Fetch and hydrate another URL in the same cookie-persisting session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string" },
                    "url": { "type": "string" }
                },
                "required": ["sessionId", "url"]
            },
            "annotations": { "readOnlyHint": false, "openWorldHint": true }
        }),
        json!({
            "name": "draco_interact_scrape",
            "title": "Scrape interact session",
            "description": "Serialize the current DOM and run Draco's content engine over it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string" },
                    "formats": {
                        "type": "array",
                        "items": {
                            "type": "string",
                            "enum": ["markdown", "html", "rawHtml", "links"]
                        }
                    },
                    "extract": {
                        "type": "object",
                        "description": "Selector-schema extraction run on the serialized DOM."
                    }
                },
                "required": ["sessionId"]
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        }),
        json!({
            "name": "draco_interact_close",
            "title": "Close interact session",
            "description": "Close a live interact session and release its isolate slot.",
            "inputSchema": {
                "type": "object",
                "properties": { "sessionId": { "type": "string" } },
                "required": ["sessionId"]
            },
            "annotations": { "readOnlyHint": false, "openWorldHint": false }
        }),
    ]
}

/// `tools/call` dispatch. Protocol-level misuse (unknown tool, missing/invalid
/// params) is a JSON-RPC error; a scrape that *ran and failed* is a tool
/// result with `isError: true`.
async fn call_tool(
    params: &Value,
    defaults: &Config,
    gate: Option<&Semaphore>,
    pool: Option<&Tier2Pool>,
    sessions: InteractStoreRef<'_>,
) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    #[cfg(feature = "tier2")]
    if name.starts_with("draco_interact_") {
        return call_interact(name, params, defaults, sessions).await;
    }
    #[cfg(not(feature = "tier2"))]
    let _ = sessions;
    // `draco_search` is a distinct path (query in, ranked results out) — no URL,
    // its own fan-out + consensus.
    if name == "draco_search" {
        return call_search(params, defaults, gate, pool).await;
    }
    // Link discovery + bounded multi-page scrapes: agent-sized mirrors of the
    // daemon's /v1/map, /v1/crawl, /v1/batch/scrape (which remain the path for
    // large async jobs).
    if name == "draco_map" {
        return call_map(params, defaults).await;
    }
    if name == "draco_crawl" || name == "draco_batch_scrape" {
        return call_multi_scrape(name, params, defaults, gate, pool).await;
    }
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
    let parsed_formats = parse_formats(&formats).map_err(|rej| (-32602, rej.message))?;

    let mut config = defaults.clone();
    config.formats = parsed_formats;
    // `draco_discover` always discovers and replays the winner into `data`,
    // regardless of the `formats` argument: force endpoints + json-only.
    if is_discover {
        config.formats = FormatSet {
            json: true,
            endpoints: true,
            ..FormatSet::none()
        };
    }
    if let Some(t) = args.get("tierMax").and_then(Value::as_u64) {
        config.tier_max = t.min(u8::MAX as u64) as u8;
    }
    if let Some(w) = args.get("captureWindowMs").and_then(Value::as_u64) {
        config.capture_window_ms = w;
    } else if let Some(w) = args.get("waitFor").and_then(Value::as_u64) {
        // Firecrawl-style alias; explicit captureWindowMs (above) wins.
        config.capture_window_ms = w;
    }
    if let Some(only_main) = args.get("onlyMainContent").and_then(Value::as_bool) {
        config.only_main_content = only_main;
    }
    if let Some(arr) = args.get("includeTags").and_then(Value::as_array) {
        config.include_tags = arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
    }
    if let Some(arr) = args.get("excludeTags").and_then(Value::as_array) {
        config.exclude_tags = arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
    }
    if let Some(schema) = args.get("extract").filter(|v| v.is_object()) {
        config.extract_schema = Some(schema.clone());
    }
    if let Some(obj) = args.get("headers").and_then(Value::as_object) {
        config.headers = obj
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect();
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
    if let Some(true) = args.get("runtimeLog").and_then(Value::as_bool) {
        config.runtime_log = true;
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

    let (code, body) = to_firecrawl(&result);
    if code != StatusCode::OK {
        let msg = body["error"].as_str().unwrap_or("extraction failed");
        return Ok(tool_error(&format!("{msg} (source: {url})")));
    }

    // Content assembly: agents want prose first — the markdown string itself is
    // the primary content item; the JSON-API payload (when requested) rides as
    // a second pretty-printed item. For discovery, the endpoint catalog leads.
    let data = &body["data"];
    let mut content = Vec::new();
    // Selector-schema extraction leads when it was requested: the structured
    // payload is the answer the agent asked for; prose follows.
    if config.extract_schema.is_some() {
        let mut payload = json!({ "extract": data.get("extract").cloned().unwrap_or(Value::Null) });
        if let Some(w) = data.get("extractWarnings") {
            payload["extractWarnings"] = w.clone();
        }
        let pretty = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
        content.push(json!({ "type": "text", "text": pretty }));
    }
    if is_discover {
        let endpoints = data.get("endpoints").cloned().unwrap_or(json!([]));
        let pretty =
            serde_json::to_string_pretty(&endpoints).unwrap_or_else(|_| endpoints.to_string());
        content.push(json!({ "type": "text", "text": pretty }));
    }
    if let Some(md) = data["markdown"].as_str() {
        content.push(json!({ "type": "text", "text": md }));
    }
    if config.formats.wants_data() {
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
            "text": format!("scrape succeeded but produced no {:?} content for {url}", config.formats)
        }));
    }
    Ok(json!({ "content": content, "isError": false }))
}

#[cfg(feature = "tier2")]
fn required_interact_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, (i64, String)> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or((-32602, format!("\"{key}\" (string) is required")))
}

/// Cap on pages per `draco_crawl` / `draco_batch_scrape` call. Tool calls are
/// synchronous; anything larger belongs on the daemon's async job endpoints.
const MAX_MULTI_PAGES: usize = 25;

/// `draco_map` — link discovery through the same core the REST `/v1/map`
/// handler wraps ([`crate::serve::map::map_site`]). A couple of fetches, no
/// isolate, so it skips the scrape gate.
async fn call_map(params: &Value, defaults: &Config) -> Result<Value, (i64, String)> {
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let url = args
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .ok_or((-32602, "\"url\" (string) is required".to_string()))?;
    let target = crate::serve::map::parse_http_url(url).map_err(|e| (-32602, e))?;

    let mut config = defaults.clone();
    if let Some(t) = args.get("timeout").and_then(Value::as_u64) {
        config.timeout_ms = t;
    }
    if let Some(true) = args.get("ignoreRobots").and_then(Value::as_bool) {
        config.respect_robots = false;
    }
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 5_000) as usize;

    let opts = crate::serve::map::MapOptions {
        target,
        session: session_opts(&config),
        search: args
            .get("search")
            .and_then(Value::as_str)
            .map(str::to_string),
        limit,
        include_subdomains: args
            .get("includeSubdomains")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        ignore_sitemap: args
            .get("ignoreSitemap")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        sitemap_only: false,
    };
    match crate::serve::map::map_site(&opts).await {
        Ok(out) => {
            // Tool results MUST ride the MCP content envelope — a bare JSON
            // payload renders as an empty result in MCP clients.
            let payload = json!({
                "success": true,
                "count": out.links.len(),
                "links": out.links,
            });
            let text =
                serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
            Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            }))
        }
        Err(crate::serve::map::MapError::BadRequest(m))
        | Err(crate::serve::map::MapError::Upstream(m)) => Ok(tool_error(&m)),
    }
}

/// `draco_crawl` (map the site, scrape the first N pages inline) and
/// `draco_batch_scrape` (the same loop over caller-given URLs). Bounded at
/// [`MAX_MULTI_PAGES`]; sequential under the shared gate so a tool call can
/// never monopolize the daemon. Per-page failures ride inline in `data` —
/// one dead URL must not cost the agent the other results.
async fn call_multi_scrape(
    name: &str,
    params: &Value,
    defaults: &Config,
    gate: Option<&Semaphore>,
    pool: Option<&Tier2Pool>,
) -> Result<Value, (i64, String)> {
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    // Shared per-page config from the common scrape knobs.
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
    let mut config = defaults.clone();
    config.formats = parse_formats(&formats).map_err(|rej| (-32602, rej.message))?;
    if let Some(only_main) = args.get("onlyMainContent").and_then(Value::as_bool) {
        config.only_main_content = only_main;
    }
    if let Some(schema) = args.get("extract").filter(|v| v.is_object()) {
        config.extract_schema = Some(schema.clone());
    }
    if let Some(t) = args.get("timeout").and_then(Value::as_u64) {
        config.timeout_ms = t;
    }
    if let Some(true) = args.get("ignoreRobots").and_then(Value::as_bool) {
        config.respect_robots = false;
    }

    // Resolve the page list.
    let urls: Vec<String> = if name == "draco_batch_scrape" {
        let list = args.get("urls").and_then(Value::as_array).ok_or((
            -32602,
            "\"urls\" (array of strings) is required".to_string(),
        ))?;
        let urls: Vec<String> = list
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .map(str::to_string)
            .collect();
        if urls.is_empty() {
            return Err((-32602, "\"urls\" must contain at least one URL".to_string()));
        }
        if urls.len() > MAX_MULTI_PAGES {
            return Err((
                -32602,
                format!(
                    "\"urls\" is capped at {MAX_MULTI_PAGES} per tool call; use \
                     POST /v1/batch/scrape on the daemon for larger jobs"
                ),
            ));
        }
        urls
    } else {
        // Crawl: the seed page first, then mapped same-site links up to `limit`.
        // A failed map is non-fatal — the seed alone still gets scraped.
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .ok_or((-32602, "\"url\" (string) is required".to_string()))?;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(10)
            .clamp(1, MAX_MULTI_PAGES as u64) as usize;
        let target = crate::serve::map::parse_http_url(url).map_err(|e| (-32602, e))?;
        let opts = crate::serve::map::MapOptions {
            target: target.clone(),
            session: session_opts(&config),
            search: args
                .get("search")
                .and_then(Value::as_str)
                .map(str::to_string),
            limit,
            include_subdomains: args
                .get("includeSubdomains")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            ignore_sitemap: false,
            sitemap_only: false,
        };
        let mut urls = vec![target.to_string()];
        if let Ok(out) = crate::serve::map::map_site(&opts).await {
            for link in out.links {
                if urls.len() >= limit {
                    break;
                }
                if !urls.contains(&link) {
                    urls.push(link);
                }
            }
        }
        urls
    };

    // Scrape sequentially, re-acquiring the shared gate per page so long
    // batches interleave fairly with other daemon work.
    let mut docs = Vec::with_capacity(urls.len());
    let mut completed = 0usize;
    for url in &urls {
        let permit = match gate {
            Some(g) => match g.acquire().await {
                Ok(p) => Some(p),
                Err(_) => return Ok(tool_error("server is shutting down")),
            },
            None => None,
        };
        let result = match pool {
            Some(p) => extract_with_pool(url, &config, p).await,
            None => extract(url, &config).await,
        };
        drop(permit);
        let (code, body) = to_firecrawl(&result);
        if code == StatusCode::OK {
            completed += 1;
            let mut doc = body["data"].clone();
            if let Some(obj) = doc.as_object_mut() {
                obj.insert("url".into(), json!(url));
            }
            docs.push(doc);
        } else {
            docs.push(json!({
                "url": url,
                "success": false,
                "error": body["error"].clone(),
            }));
        }
    }
    // Tool results MUST ride the MCP content envelope (see call_map).
    let payload = json!({
        "success": true,
        "total": urls.len(),
        "completed": completed,
        "data": docs,
    });
    let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    }))
}

#[cfg(feature = "tier2")]
async fn call_interact(
    name: &str,
    params: &Value,
    defaults: &Config,
    sessions: InteractStoreRef<'_>,
) -> Result<Value, (i64, String)> {
    let Some(store) = sessions else {
        return Ok(tool_error("interact requires the daemon HTTP transport"));
    };
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let payload = match name {
        "draco_interact_open" => {
            let url = required_interact_arg(&args, "url")?;
            let config = Config {
                formats: FormatSet::markdown_only(),
                force_render: false,
                ..defaults.clone()
            };
            let opened = match store.open(url, &config).await {
                Ok(opened) => opened,
                Err(error) => return Ok(interact_store_error(error)),
            };
            let snapshot = opened.snapshot_html.as_deref().map(|html| {
                let result = draco_core::scrape_interact_html(
                    url,
                    html,
                    FormatSet::markdown_only(),
                    true,
                    None,
                );
                json!({
                    "markdown": result.markdown,
                    "html": html,
                    "metadata": result.metadata,
                })
            });
            json!({
                "success": true,
                "sessionId": opened.id,
                "snapshot": snapshot,
            })
        }
        "draco_interact_exec" => {
            let id = required_interact_arg(&args, "sessionId")?;
            let js = required_interact_arg(&args, "js")?;
            let defaults = draco_core::ExecOptions::default();
            let max_bytes = match args.get("maxBytes").and_then(Value::as_u64) {
                Some(value) => usize::try_from(value)
                    .map_err(|_| (-32602, "\"maxBytes\" is too large".to_string()))?
                    .max(1),
                None => defaults.max_bytes,
            };
            let opts = draco_core::ExecOptions {
                settle: true,
                full: args
                    .get("full")
                    .and_then(Value::as_bool)
                    .unwrap_or(defaults.full),
                max_bytes,
            };
            let report = match store.exec(id, js.to_string(), opts).await {
                Ok(report) => report,
                Err(error) => return Ok(interact_store_error(error)),
            };
            json!({
                "success": report.ok,
                "result": report.result,
                "logs": report.logs,
                "error": report.error,
            })
        }
        "draco_interact_act" => {
            let id = required_interact_arg(&args, "sessionId")?;
            let actions = args
                .get("actions")
                .cloned()
                .ok_or((-32602, "\"actions\" (array) is required".to_string()))?;
            let actions: Vec<draco_core::Action> = serde_json::from_value(actions)
                .map_err(|error| (-32602, format!("invalid \"actions\": {error}")))?;
            let report = match store.act(id, actions).await {
                Ok(report) => report,
                Err(error) => return Ok(interact_store_error(error)),
            };
            let batch_ok = report.ok;
            let formats = FormatSet {
                markdown: true,
                raw_html: true,
                ..FormatSet::none()
            };
            let extract = args.get("extract").filter(|v| v.is_object()).cloned();
            let result = match store.scrape(id, formats, true, extract.as_ref()).await {
                Ok(result) => result,
                Err(error) => return Ok(interact_store_error(error)),
            };
            let (status, mut body) = to_firecrawl(&result);
            if status != StatusCode::OK {
                return Ok(tool_error(
                    body["error"]
                        .as_str()
                        .unwrap_or("interact act readback failed"),
                ));
            }
            if let Some(data) = body.get_mut("data").and_then(Value::as_object_mut) {
                data.insert("ok".into(), Value::Bool(report.ok));
                data.insert(
                    "steps".into(),
                    Value::Array(
                        report
                            .steps
                            .into_iter()
                            .map(|step| {
                                json!({
                                    "action": step.action,
                                    "ok": step.ok,
                                    "error": step.error,
                                })
                            })
                            .collect(),
                    ),
                );
                data.insert("logs".into(), json!(report.logs));
            }
            // Mirror the batch outcome at the top level (matches the REST act
            // handler): a failed step must not read as `success: true`.
            if !batch_ok {
                if let Some(obj) = body.as_object_mut() {
                    obj.insert("success".into(), Value::Bool(false));
                }
            }
            body
        }
        "draco_interact_navigate" => {
            let id = required_interact_arg(&args, "sessionId")?;
            let url = required_interact_arg(&args, "url")?;
            let report = match store.navigate(id, url.to_string()).await {
                Ok(report) => report,
                Err(error) => return Ok(interact_store_error(error)),
            };
            json!({
                "success": report.ok,
                "url": report.url,
                "error": report.error,
            })
        }
        "draco_interact_scrape" => {
            let id = required_interact_arg(&args, "sessionId")?;
            let formats: Vec<String> = args
                .get("formats")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let formats = parse_formats(&formats).map_err(|reject| (-32602, reject.message))?;
            if formats.json || formats.endpoints {
                return Err((
                    -32602,
                    "interact scrape supports markdown, html, rawHtml, and links".to_string(),
                ));
            }
            let extract = args.get("extract").filter(|v| v.is_object()).cloned();
            let result = match store.scrape(id, formats, true, extract.as_ref()).await {
                Ok(result) => result,
                Err(error) => return Ok(interact_store_error(error)),
            };
            let (status, body) = to_firecrawl(&result);
            if status != StatusCode::OK {
                return Ok(tool_error(
                    body["error"].as_str().unwrap_or("interact scrape failed"),
                ));
            }
            body
        }
        "draco_interact_close" => {
            let id = required_interact_arg(&args, "sessionId")?;
            if let Err(error) = store.close(id).await {
                return Ok(interact_store_error(error));
            }
            json!({ "success": true })
        }
        _ => return Err((-32602, format!("unknown tool: {name:?}"))),
    };

    let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    }))
}

#[cfg(feature = "tier2")]
fn interact_store_error(error: SessionStoreError) -> Value {
    match error {
        SessionStoreError::NotFound => tool_error("interact session not found"),
        SessionStoreError::Capacity => tool_error("interact session capacity reached"),
        SessionStoreError::Closed => tool_error("interact session is closed"),
        SessionStoreError::Runtime(message) => {
            tool_error(&format!("interact session error: {message}"))
        }
    }
}

/// `draco_search` execution: fan out across engines, consensus-merge, and
/// (optionally) scrape each result URL. Total engine failure is a tool-level
/// error; partial failure still returns the surviving engines' results.
async fn call_search(
    params: &Value,
    defaults: &Config,
    gate: Option<&Semaphore>,
    pool: Option<&Tier2Pool>,
) -> Result<Value, (i64, String)> {
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .ok_or((-32602, "\"query\" (string) is required".to_string()))?;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(1, 100))
        .unwrap_or(5);
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
    let scrape_formats = if formats.is_empty() {
        None
    } else {
        Some(parse_formats(&formats).map_err(|rej| (-32602, rej.message))?)
    };

    let params = search::SearchParams {
        query: query.to_string(),
        limit,
        tbs: args.get("tbs").and_then(Value::as_str).map(String::from),
        location: args
            .get("location")
            .and_then(Value::as_str)
            .map(String::from),
    };
    let overall = std::time::Duration::from_millis(
        args.get("timeout")
            .and_then(Value::as_u64)
            .unwrap_or(60_000),
    );

    // SERP session posture from the daemon defaults; browser-like robots.
    let serp_config = Config {
        force_render: false,
        timeout_ms: 15_000,
        respect_robots: false,
        ..defaults.clone()
    };
    let session = session_opts(&serp_config);

    // One gate permit spans the whole tool call (SERP fan-out + any scrapes).
    let permit = match gate {
        Some(g) => match g.acquire().await {
            Ok(p) => Some(p),
            Err(_) => return Ok(tool_error("server is shutting down")),
        },
        None => None,
    };

    let engines = search::default_engines();
    let fut = search::search_all_with_session(
        &params,
        &engines,
        search::DEFAULT_PER_ENGINE_TIMEOUT,
        &session,
    );
    let (hits, outcomes) = match tokio::time::timeout(overall, fut).await {
        Ok(pair) => pair,
        Err(_) => {
            drop(permit);
            return Ok(tool_error("search timed out before any engine returned"));
        }
    };
    if !outcomes
        .iter()
        .any(|o| matches!(o.status, search::EngineStatus::Ok(_)))
    {
        drop(permit);
        return Ok(tool_error("all search engines failed"));
    }

    let merged = search::consensus(hits, limit);
    let mut data = Vec::with_capacity(merged.len());
    match scrape_formats {
        Some(formats) => {
            // Scrape each result (sequential — one tool call, modest limit).
            // Prefer the warm pool when present (HTTP transport); stdio uses a
            // one-shot capture.
            for hit in &merged {
                let mut item = search::base_item(hit);
                let config = Config {
                    formats,
                    ..defaults.clone()
                };
                let result = match pool {
                    Some(p) => extract_with_pool(&hit.url, &config, p).await,
                    None => extract(&hit.url, &config).await,
                };
                if result.status == Status::Success {
                    search::merge_scrape_fields(&mut item, &result);
                }
                data.push(Value::Object(item));
            }
        }
        None => {
            for hit in &merged {
                data.push(Value::Object(search::base_item(hit)));
            }
        }
    }
    drop(permit);

    let payload = json!({
        "success": true,
        "data": data,
        "draco": { "engines": search::outcomes_json(&outcomes) },
    });
    let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    Ok(json!({ "content": [ { "type": "text", "text": text } ], "isError": false }))
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
    use axum::http::{header::CONTENT_TYPE, Request};
    use axum::routing::{get, post};
    use axum::Router;
    use tower::ServiceExt;

    fn defaults() -> Config {
        Config {
            force_render: false,
            tier_max: 0,
            respect_robots: false,
            ..Config::default()
        }
    }

    async fn dispatch(msg: Value) -> Option<Value> {
        // No gate, pool, or daemon interact registry.
        #[cfg(feature = "tier2")]
        let sessions: InteractStoreRef<'_> = None;
        #[cfg(not(feature = "tier2"))]
        let sessions: InteractStoreRef<'_> = ();
        handle_message(&msg, &defaults(), None, None, sessions).await
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
    async fn tools_list_matches_stdio_transport_capabilities() {
        let resp = dispatch(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
            .await
            .unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"draco_scrape"), "tools: {names:?}");
        assert!(names.contains(&"draco_discover"), "tools: {names:?}");
        assert!(names.contains(&"draco_search"), "tools: {names:?}");
        // Link discovery + bounded multi-page scrapes are transport-independent:
        // they run through direct calls (no daemon job store), so stdio has them.
        assert!(names.contains(&"draco_map"), "tools: {names:?}");
        assert!(names.contains(&"draco_crawl"), "tools: {names:?}");
        assert!(names.contains(&"draco_batch_scrape"), "tools: {names:?}");
        assert!(
            !names.iter().any(|name| name.starts_with("draco_interact_")),
            "stdio must not advertise daemon-only interact tools: {names:?}"
        );
        for name in ["draco_scrape", "draco_discover"] {
            let tool = tools.iter().find(|tool| tool["name"] == name).unwrap();
            assert_eq!(tool["inputSchema"]["required"], json!(["url"]));
            assert_eq!(tool["annotations"]["readOnlyHint"], true);
        }
    }

    #[cfg(feature = "tier2")]
    #[test]
    fn daemon_descriptors_include_all_interact_tools() {
        let tools = tool_descriptors(true);
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        for name in [
            "draco_interact_open",
            "draco_interact_exec",
            "draco_interact_act",
            "draco_interact_navigate",
            "draco_interact_scrape",
            "draco_interact_close",
        ] {
            assert!(names.contains(&name), "missing {name}: {names:?}");
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

    #[cfg(feature = "tier2")]
    #[tokio::test]
    async fn stdio_interact_call_returns_transport_error() {
        let resp = dispatch(json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {
                "name": "draco_interact_open",
                "arguments": { "url": "https://example.com" }
            }
        }))
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("daemon HTTP transport"));
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
        let (crawl, batch) = crate::serve::jobs::JobStore::shared_pair();
        let state = Arc::new(AppState {
            defaults: defaults(),
            gate: Semaphore::new(2),
            max_concurrency: 2,
            tier2_pool: draco_core::Tier2Pool::new(1, 100, true, false),
            crawl,
            batch,
            #[cfg(feature = "tier2")]
            sessions: crate::serve::interact::SessionStore::new(1),
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

    /// A lossy UTF-8 page used to panic inside Markdown post-processing while
    /// `draco_crawl` handled its seed. That killed the stdio MCP process and
    /// stranded every later tool call in the same session.
    #[tokio::test]
    async fn crawl_survives_lossy_utf8_and_keeps_the_session_usable() {
        let fixture = Router::new()
            .route(
                "/",
                get(|| async {
                    (
                        [(CONTENT_TYPE, "text/html; charset=utf-8")],
                        Body::from(
                            b"<html><body><p>[\xffSkip to Content](#main)</p></body></html>"
                                .to_vec(),
                        ),
                    )
                }),
            )
            .route(
                "/json",
                get(|| async {
                    (
                        [(CONTENT_TYPE, "application/json")],
                        Body::from(
                            b"{\n  \"slides\": [\n    { \"title\": \"One\" }\n  ]\n}".to_vec(),
                        ),
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, fixture).await.unwrap();
        });

        let crawl = dispatch(json!({
            "jsonrpc": "2.0", "id": 43, "method": "tools/call",
            "params": {
                "name": "draco_crawl",
                "arguments": {
                    "url": format!("http://127.0.0.1:{port}/"),
                    "limit": 1,
                    "formats": ["markdown"]
                }
            }
        }))
        .await
        .unwrap();
        assert!(crawl.get("error").is_none(), "crawl: {crawl}");
        assert_eq!(crawl["result"]["isError"], false, "crawl: {crawl}");

        // The same post-processing path is used by batch scrape. Array brackets
        // in a JSON response are data, not an unfinished Markdown link label.
        let batch = dispatch(json!({
            "jsonrpc": "2.0", "id": 44, "method": "tools/call",
            "params": {
                "name": "draco_batch_scrape",
                "arguments": {
                    "urls": [format!("http://127.0.0.1:{port}/json")],
                    "formats": ["markdown"]
                }
            }
        }))
        .await
        .unwrap();
        let text = batch["result"]["content"][0]["text"].as_str().unwrap();
        assert!(!text.contains(r"\["), "JSON array was escaped: {text}");

        // In a real stdio session this is the next RPC line; reaching it proves
        // the crawl path returned normally instead of taking down the process.
        let ping = dispatch(json!({ "jsonrpc": "2.0", "id": 45, "method": "ping" }))
            .await
            .unwrap();
        assert_eq!(ping["result"], json!({}));
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
