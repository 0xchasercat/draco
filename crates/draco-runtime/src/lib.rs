//! # draco-runtime — Tier 2 V8 isolate + fetch/XHR interception (Slice 3)
//!
//! Boots a V8 isolate via `deno_core`, installs a hand-written DOM + scheduler
//! polyfill and a `fetch`/`XMLHttpRequest` interceptor, evaluates a page's inline
//! scripts, and drives a **capture window**: the isolate runs until the event
//! loop goes idle for `quiesce_ms` or the hard `capture_window_ms` cap elapses,
//! whichever comes first. Every intercepted request is recorded (rank-agnostic —
//! ranking is `draco-core`'s job) and answered with a synthetic stub so the page
//! keeps hydrating and reveals more endpoints.
//!
//! This crate is **self-contained**: no IPC lives here. Slice 4 (`draco-jail`)
//! calls [`run_capture`] from the jailed child and maps each [`CapturedRequest`]
//! to `draco_types::JailToSupervisor::Intercept` and [`CaptureReport::outcome`]
//! to a `Result`. The frozen contract is `draco-types` (we reuse its
//! [`draco_types::InterceptVia`] and [`draco_types::RuntimeOutcome`]); this
//! crate's own API is designed for that wiring but is not itself frozen.
//!
//! ## Implementation notes (canonical §8)
//!
//! * **Polyfill loading — runtime execution, not a build.rs snapshot.** The spec
//!   lists a `build.rs` snapshot as *preferred* but explicitly permits runtime
//!   execution "if the 0.406.0 snapshot API proves finicky". We chose runtime
//!   execution: a custom-extension snapshot must exactly match op registration
//!   between snapshot-build and runtime and must exclude JS from the extension,
//!   which turns `build.rs` into a second V8-linking compilation unit and
//!   interacts badly with `--jitless`. Executing the (small, hand-written)
//!   polyfill at startup costs a few ms per isolate, keeps the crate to a single
//!   compilation unit, and is far easier to test. See `js/polyfill.js`.
//!
//! * **`--jitless`.** We pass `--jitless` and `--single-threaded` via
//!   `deno_core::v8_set_flags` *before* the first isolate is created. Both are
//!   accepted by the V8 this deno_core ships (v8 149.x); `--jitless` disables
//!   the JIT (no RWX pages) and the WASM tier, and `--single-threaded` avoids
//!   V8 background compiler/GC threads — both reduce the syscall surface the
//!   jail's future seccomp policy must allow. `--jitless` does not conflict
//!   with anything here because we do **not** create a V8 heap snapshot of our
//!   own (see the polyfill note above). Flags V8 rejects are reported by
//!   `v8_set_flags` and skipped (best-effort) rather than aborting; we verified
//!   `--no-expose-wasm` is rejected by this build, so we do not pass it.
//!
//! * **Timers / event-loop driver.** deno_core 0.406.0's only timer reactor is
//!   tokio-based (`tokio::time::sleep_until`), so the event loop must run under a
//!   tokio time driver — a pure `futures::executor::block_on` would panic the
//!   moment a timer future is polled. Honoring the spec's *intent* (keep the
//!   jailed child's syscall surface small), we use a **current-thread** tokio
//!   runtime with **`enable_time()` only** — no worker pool, no I/O reactor. Our
//!   `setTimeout`/`setInterval` polyfill is backed by the `op_sleep` async op
//!   (tokio sleep); a pending `op_sleep` keeps `poll_event_loop` returning
//!   `Pending`, which is exactly the "loop is busy" signal the driver watches.

#![allow(clippy::type_complexity)]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Once;
use std::task::Poll;
use std::time::{Duration, Instant};

use deno_core::{JsRuntime, OpState, PollEventLoopOptions, RuntimeOptions};
use serde::Deserialize;

use draco_types::{InterceptVia, RuntimeOutcome};

// ===================================================================
// Public API (Slice 4 wires this into the jail child)
// ===================================================================

/// Knobs for a single capture run. Mirrors the `Hydrate` frame fields in
/// `draco_types::SupervisorToJail` so Slice 4 can map straight across.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Hard cap on the interception window (ms).
    pub capture_window_ms: u64,
    /// Close early if the event loop is idle this long (ms).
    pub quiesce_ms: u64,
    /// Safety cap on the number of captured requests.
    pub max_intercepts: u32,
    /// JSON body the stub response resolves with, to keep hydration going.
    /// Empty string is treated as `"{}"`. May be a bare JSON object/array/value
    /// (used verbatim as the response body) or an object of the shape
    /// `{"status":u16,"headers":[[k,v]],"body":"..."}` to control the whole
    /// synthetic response.
    pub stub_response_json: String,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            capture_window_ms: 3000,
            quiesce_ms: 300,
            max_intercepts: 64,
            stub_response_json: "{}".to_string(),
        }
    }
}

/// One request the page tried to make, captured verbatim (header order
/// preserved — it is fingerprint-relevant downstream).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub via: InterceptVia,
}

/// Terminal report for a capture run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureReport {
    pub outcome: RuntimeOutcome,
    pub requests: Vec<CapturedRequest>,
}

/// Boot an isolate, evaluate `html`'s inline scripts under `url`, run the capture
/// window, and return everything the page tried to fetch.
///
/// Never panics on page-author errors: a script that throws yields
/// [`RuntimeOutcome::Threw`] (with whatever was captured before the throw), and
/// the isolate is always torn down cleanly.
pub fn run_capture(url: &str, html: &str, cfg: &CaptureConfig) -> CaptureReport {
    ensure_v8_flags();

    // Current-thread tokio runtime, time driver only (see module docs).
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            // Can't even build the runtime — report as a throw with no captures.
            eprintln!("draco-runtime: failed to build tokio runtime: {e}");
            return CaptureReport {
                outcome: RuntimeOutcome::Threw,
                requests: Vec::new(),
            };
        }
    };

    rt.block_on(async move { run_capture_inner(url, html, cfg).await })
}

/// Thin legacy entry retained from the stub. Not used by the jail directly (Slice
/// 4 calls [`run_capture`]); kept so existing references still link.
pub fn run_runtime() -> ! {
    // The real runtime is [`run_capture`], invoked by the jail child with the
    // page HTML received over IPC. There is nothing meaningful to do without
    // that input, so this entry simply exits cleanly.
    std::process::exit(0)
}

// ===================================================================
// Capture buffer (shared Rust <-> op state)
// ===================================================================

/// The op-side capture buffer, stored in `OpState`. `op_raze_fetch` pushes into
/// `requests`; the driver drains it after the run. The intercept count is simply
/// `requests.len()` (that is also how the `max_intercepts` cap is measured), so
/// no separate counter is kept.
struct CaptureState {
    requests: Vec<CapturedRequest>,
    max_intercepts: u32,
    /// Verbatim stub body (already normalized; never empty).
    stub_body: String,
}

/// JSON shape `op_raze_fetch` receives from the interceptor JS.
#[derive(Deserialize)]
struct RawRequest {
    via: String,
    method: String,
    url: String,
    #[serde(default)]
    headers: Vec<(String, String)>,
    #[serde(default)]
    body: Option<String>,
}

// ===================================================================
// Ops
// ===================================================================

/// Record an intercepted request and return a synthetic stub-response JSON
/// string: `{"status":u16,"headers":[[k,v]],"body":"..."}`.
///
/// Returning a value (rather than blocking) is deliberate: the page's fetch
/// resolves immediately with our stub, so hydration proceeds and more endpoints
/// surface. Enforces `max_intercepts` by throwing once the cap is exceeded (the
/// JS side swallows the throw and falls back to an empty `{}` response).
#[deno_core::op2]
#[string]
fn op_raze_fetch(
    state: &mut OpState,
    #[string] request_json: String,
) -> Result<String, deno_error::JsErrorBox> {
    let raw: RawRequest = serde_json::from_str(&request_json)
        .map_err(|e| deno_error::JsErrorBox::generic(format!("op_raze_fetch bad payload: {e}")))?;

    let cap = state.borrow::<Rc<RefCell<CaptureState>>>().clone();
    let mut cs = cap.borrow_mut();

    if cs.requests.len() as u32 >= cs.max_intercepts {
        return Err(deno_error::JsErrorBox::generic("max_intercepts exceeded"));
    }

    let via = match raw.via.as_str() {
        "xhr" => InterceptVia::Xhr,
        _ => InterceptVia::Fetch,
    };
    cs.requests.push(CapturedRequest {
        method: raw.method,
        url: raw.url,
        headers: raw.headers,
        body: raw.body.map(|b| b.into_bytes()),
        via,
    });

    // Build the stub response. We always answer 200 with the configured body and
    // a JSON content-type so `res.json()` works on the page side.
    let resp = serde_json::json!({
        "status": 200,
        "headers": [["content-type", "application/json"]],
        "body": cs.stub_body,
    });
    Ok(resp.to_string())
}

/// Sleep `ms` milliseconds, then resolve. Backs the polyfill's timer scheduler.
/// A pending `op_sleep` future keeps the deno_core event loop non-idle.
///
/// NOTE: `async` is inferred from the `async fn` signature with a bare `#[op2]`;
/// `#[op2(async)]` would require an explicit sub-mode such as `async(lazy)`.
#[deno_core::op2]
async fn op_sleep(ms: f64) -> Result<(), deno_error::JsErrorBox> {
    let ms = if ms.is_finite() && ms > 0.0 {
        ms as u64
    } else {
        0
    };
    tokio::time::sleep(Duration::from_millis(ms)).await;
    Ok(())
}

/// Resolve `rel` against `base` (WHATWG URL join). Bare `deno_core` runtimes do
/// not ship the `URL` global (that lives in the separate `deno_url` extension),
/// so the polyfill/interceptor route absolutization through this op. Returns
/// `rel` unchanged if it cannot be resolved.
#[deno_core::op2]
#[string]
fn op_resolve_url(#[string] base: String, #[string] rel: String) -> String {
    match deno_core::url::Url::parse(&base) {
        Ok(b) => match b.join(&rel) {
            Ok(joined) => joined.to_string(),
            Err(_) => rel,
        },
        Err(_) => {
            // No valid base: try `rel` as an absolute URL, else pass through.
            match deno_core::url::Url::parse(&rel) {
                Ok(u) => u.to_string(),
                Err(_) => rel,
            }
        }
    }
}

deno_core::extension!(
    draco_runtime_ext,
    ops = [op_raze_fetch, op_sleep, op_resolve_url],
    options = { cap: Rc<RefCell<CaptureState>> },
    state = |state, options| {
        state.put::<Rc<RefCell<CaptureState>>>(options.cap);
    },
);

// ===================================================================
// Boot sequence + capture-window driver
// ===================================================================

const POLYFILL_JS: &str = include_str!("../js/polyfill.js");
const INTERCEPTOR_JS: &str = include_str!("../js/interceptor.js");

async fn run_capture_inner(url: &str, html: &str, cfg: &CaptureConfig) -> CaptureReport {
    let stub_body = normalize_stub_body(&cfg.stub_response_json);

    let cap = Rc::new(RefCell::new(CaptureState {
        requests: Vec::new(),
        max_intercepts: cfg.max_intercepts,
        stub_body,
    }));

    let mut runtime = JsRuntime::new(RuntimeOptions {
        extensions: vec![draco_runtime_ext::init(cap.clone())],
        ..Default::default()
    });

    // 1. Inject the page URL (for location/history) and the page <body> markup
    //    (so the polyfill can materialize a real, stable mount-container node
    //    tree — e.g. `<div id="app">` — that a client framework can find and
    //    render into) before the polyfill runs.
    let url_lit = json_string_literal(url);
    let body_lit = json_string_literal(&extract_body_inner(html));
    if let Err(e) = runtime.execute_script(
        "draco:url",
        format!(
            "globalThis.__DRACO_URL__ = {url_lit}; globalThis.__DRACO_BODY_HTML__ = {body_lit};"
        ),
    ) {
        return finish(cap, RuntimeOutcome::Threw, Some(e.to_string()));
    }

    // 2. DOM + scheduler polyfill, then fetch/XHR interceptor.
    if let Err(e) = runtime.execute_script("draco:polyfill", POLYFILL_JS) {
        return finish(cap, RuntimeOutcome::Threw, Some(e.to_string()));
    }
    if let Err(e) = runtime.execute_script("draco:interceptor", INTERCEPTOR_JS) {
        return finish(cap, RuntimeOutcome::Threw, Some(e.to_string()));
    }

    // 3. Evaluate the page's inline <script> contents in order. A throw in page
    //    script is *not* fatal to the whole run — later scripts and already-
    //    scheduled async work may still surface intercepts — but if it happens
    //    before anything is captured we remember it so the outcome is `Threw`.
    let scripts = extract_inline_scripts(html);
    let mut threw_in_page = false;
    for (i, code) in scripts.into_iter().enumerate() {
        let name = format!("draco:page[{i}]");
        if let Err(e) = runtime.execute_script(name, code) {
            threw_in_page = true;
            eprintln!("draco-runtime: page script {i} threw: {e}");
        }
    }

    // 4. Capture window: pump the event loop until quiescence or the hard cap.
    let outcome = drive_capture_window(&mut runtime, &cap, cfg, threw_in_page).await;

    let requests = cap.borrow().requests.clone();
    CaptureReport { outcome, requests }
}

/// Drive `poll_event_loop` manually, tracking wall-clock deadline and an
/// idle/quiesce streak. Returns the mapped [`RuntimeOutcome`].
async fn drive_capture_window(
    runtime: &mut JsRuntime,
    cap: &Rc<RefCell<CaptureState>>,
    cfg: &CaptureConfig,
    threw_in_page: bool,
) -> RuntimeOutcome {
    let start = Instant::now();
    let hard_cap = Duration::from_millis(cfg.capture_window_ms);
    let quiesce = Duration::from_millis(cfg.quiesce_ms);

    // Small tick so we re-check the wall clock even while timers are pending.
    let tick = Duration::from_millis(quiesce_tick_ms(cfg.quiesce_ms));

    // Count at which we last saw activity, to measure the quiesce streak.
    let mut last_count = cap.borrow().requests.len();
    let mut last_activity = Instant::now();
    let mut loop_threw = false;

    loop {
        // Poll one tick of the event loop with a self-contained waker.
        let poll_res = poll_once(runtime);

        match poll_res {
            Poll::Ready(Ok(())) => {
                // Event loop fully drained: no pending ops/timers/promises.
                // This is a clean, natural quiesce.
                break;
            }
            Poll::Ready(Err(e)) => {
                // A top-level / unhandled error propagated out of the loop.
                eprintln!("draco-runtime: event loop error: {e}");
                loop_threw = true;
                break;
            }
            Poll::Pending => {
                // Still work pending (typically a live op_sleep timer). Fall
                // through to the time-based checks, then sleep a tick so timers
                // can elapse and we can re-evaluate.
            }
        }

        // Refresh activity tracking.
        let now_count = cap.borrow().requests.len();
        if now_count != last_count {
            last_count = now_count;
            last_activity = Instant::now();
        }

        // Hard cap first.
        if start.elapsed() >= hard_cap {
            return classify_window_close(cap, threw_in_page, loop_threw, /*hard_cap=*/ true);
        }

        // Quiesce: only start counting the streak once *something* has been
        // captured OR once the page has had a moment to schedule work. We treat
        // "no pending progress for `quiesce_ms`" as quiesced. Because the event
        // loop is Pending here (a timer is live), we still allow quiesce to fire
        // when the only remaining work is idle repeating timers that produce no
        // new intercepts — otherwise a `setInterval` would pin the window open
        // to the hard cap.
        if last_activity.elapsed() >= quiesce {
            break;
        }

        tokio::time::sleep(tick).await;
    }

    classify_window_close(cap, threw_in_page, loop_threw, /*hard_cap=*/ false)
}

/// Poll the deno_core event loop exactly once using a no-op-ish waker. We drive
/// timing ourselves via `tokio::time::sleep` between ticks, so we don't rely on
/// the waker to re-schedule us.
fn poll_once(runtime: &mut JsRuntime) -> Poll<Result<(), deno_core::error::CoreError>> {
    let waker = futures::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    runtime.poll_event_loop(&mut cx, PollEventLoopOptions::default())
}

/// Map the end-of-window condition to a `RuntimeOutcome`.
fn classify_window_close(
    cap: &Rc<RefCell<CaptureState>>,
    threw_in_page: bool,
    loop_threw: bool,
    hard_cap: bool,
) -> RuntimeOutcome {
    let cs = cap.borrow();
    let captured = !cs.requests.is_empty();

    if captured {
        // We got intercepts — the run is a success regardless of a late throw.
        if hard_cap {
            RuntimeOutcome::WindowClosed
        } else {
            RuntimeOutcome::Quiesced
        }
    } else if threw_in_page || loop_threw {
        // Nothing captured and JS blew up: that's a throw.
        RuntimeOutcome::Threw
    } else if hard_cap {
        // Ran to the cap but never fetched.
        RuntimeOutcome::NoIntercepts
    } else {
        // Quiesced cleanly with nothing to show.
        RuntimeOutcome::NoIntercepts
    }
}

fn finish(
    cap: Rc<RefCell<CaptureState>>,
    outcome: RuntimeOutcome,
    detail: Option<String>,
) -> CaptureReport {
    if let Some(d) = detail {
        eprintln!("draco-runtime: {outcome:?}: {d}");
    }
    let requests = cap.borrow().requests.clone();
    CaptureReport { outcome, requests }
}

// ===================================================================
// V8 flags
// ===================================================================

static V8_FLAGS: Once = Once::new();

/// Set V8 flags once, before any isolate is created. Best-effort: flags V8 does
/// not understand are reported and ignored (we do not abort).
fn ensure_v8_flags() {
    V8_FLAGS.call_once(|| {
        // `--jitless` keeps V8 from allocating RWX pages / using the JIT, which
        // is what makes it compatible with the jail's future seccomp policy.
        // `--single-threaded` avoids V8 background compiler/GC threads (also a
        // seccomp win). The leading argv[0] is ignored by V8.
        // `--jitless` already disables the WASM/JIT tiers, so we don't add a
        // separate wasm flag (V8 rejects `--no-expose-wasm` in this build).
        let flags = vec![
            "draco".to_string(),
            "--jitless".to_string(),
            "--single-threaded".to_string(),
        ];
        let unrecognized = deno_core::v8_set_flags(flags);
        // unrecognized[0] is always argv[0] ("draco"); anything past that is a
        // flag V8 rejected. Report but continue.
        if unrecognized.len() > 1 {
            eprintln!(
                "draco-runtime: V8 ignored flags: {:?} (proceeding)",
                &unrecognized[1..]
            );
        }
    });
}

// ===================================================================
// Helpers: stub normalization, script extraction, JS string literals
// ===================================================================

/// Normalize the configured stub into a body string usable by the interceptor.
/// Empty → `"{}"`. Otherwise the value is used verbatim as the response body.
fn normalize_stub_body(raw: &str) -> String {
    let t = raw.trim();
    if t.is_empty() {
        "{}".to_string()
    } else {
        t.to_string()
    }
}

/// Extract the source of every runnable inline `<script>`, in document order.
///
/// This is a small, **rawtext-aware** scanner rather than a DOM parse: per the
/// HTML spec, a `<script>` element's content is CDATA/rawtext, so a bare `<`
/// (e.g. `i < 100`, minified `a<b`, compiled JSX) must NOT be treated as markup.
/// General HTML parsers (`tl`, `html5ever`'s text API, etc.) that hand back
/// "inner text" corrupt such scripts by splitting on `<`. We therefore capture
/// everything verbatim between each `<script ...>`'s `>` and the next
/// case-insensitive `</script>`.
///
/// External scripts (`src=...`) and non-executable `type`s (JSON-LD, importmap,
/// `application/json`, `__NEXT_DATA__`, `speculationrules`, templates, …) are
/// skipped — we only run real JS.
fn extract_inline_scripts(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = html.as_bytes();
    let lower = html.to_ascii_lowercase();
    let lb = lower.as_bytes();
    let mut i = 0usize;

    while let Some(rel) = find_subslice(&lb[i..], b"<script") {
        let tag_start = i + rel;
        // The char right after "<script" must be whitespace, '>' or '/' — else
        // it's something like "<scripting" and we skip past this match.
        let after = tag_start + b"<script".len();
        let ok_boundary = matches!(bytes.get(after), Some(c) if c.is_ascii_whitespace() || *c == b'>' || *c == b'/');
        if !ok_boundary {
            i = after;
            continue;
        }
        // Find end of the opening tag ('>').
        let Some(gt_rel) = find_subslice(&lb[after..], b">") else {
            break; // malformed; nothing runnable after.
        };
        let open_end = after + gt_rel; // index of '>'
        let open_tag = &html[tag_start..=open_end]; // includes <script ...>

        // Self-closing "<script .../>" has no body.
        let self_closing = open_tag.trim_end().ends_with("/>");

        // Locate the matching "</script>" (case-insensitive).
        let body_start = open_end + 1;
        let (body, next_i) = if self_closing {
            ("", body_start)
        } else {
            match find_subslice(&lb[body_start..], b"</script") {
                Some(close_rel) => {
                    let close_start = body_start + close_rel;
                    // Advance past the closing tag's '>'.
                    let after_close = match find_subslice(&lb[close_start..], b">") {
                        Some(g) => close_start + g + 1,
                        None => bytes.len(),
                    };
                    (&html[body_start..close_start], after_close)
                }
                None => {
                    // Unterminated <script>: take the rest of the document.
                    (&html[body_start..], bytes.len())
                }
            }
        };

        if is_runnable_script_tag(open_tag) && !body.trim().is_empty() {
            out.push(body.to_string());
        }
        i = next_i;
    }
    out
}

/// Extract the inner HTML of the document `<body>` (the static mount scaffold),
/// so the polyfill can build a real node tree for it.
///
/// This is intentionally coarse: a byte scan for the first `<body...>`'s `>` and
/// the last `</body>`. The polyfill's own parser drops any `<script>` elements
/// (their code is executed separately), so it is fine to hand it the whole body
/// including inline scripts. If there is no `<body>` tag we return the region
/// after `</head>` (or the whole document) — a framework that mounts into
/// `#app` still finds its container wherever the markup lives.
fn extract_body_inner(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let lb = lower.as_bytes();

    if let Some(rel) = find_subslice(lb, b"<body") {
        // Advance to the end of the opening <body ...> tag.
        if let Some(gt) = find_subslice(&lb[rel..], b">") {
            let start = rel + gt + 1;
            let end = match find_subslice(&lb[start..], b"</body") {
                Some(c) => start + c,
                None => html.len(),
            };
            return html[start..end].to_string();
        }
    }

    // No <body>: use everything after </head> if present, else the whole doc.
    if let Some(rel) = find_subslice(lb, b"</head>") {
        let start = rel + b"</head>".len();
        return html[start..].to_string();
    }
    html.to_string()
}

/// Decide whether an opening `<script ...>` tag is executable JS: skip if it has
/// a `src` attribute or a non-JS `type`.
fn is_runnable_script_tag(open_tag: &str) -> bool {
    let lower = open_tag.to_ascii_lowercase();
    if attr_present(&lower, "src") {
        return false;
    }
    match attr_value(open_tag, "type") {
        None => true,
        Some(ty) => {
            let ty = ty.trim().to_ascii_lowercase();
            ty.is_empty()
                || ty == "text/javascript"
                || ty == "application/javascript"
                || ty == "module"
                || ty == "text/ecmascript"
                || ty == "application/ecmascript"
                || ty == "text/babel"
                || ty == "text/jsx"
        }
    }
}

/// True if attribute `name` appears in the (already-lowercased) opening tag.
/// Matches `name=`, `name ` or `name>` / `name/` (bare boolean attribute).
fn attr_present(lower_tag: &str, name: &str) -> bool {
    let mut search_from = 0;
    while let Some(rel) = lower_tag[search_from..].find(name) {
        let at = search_from + rel;
        // Preceding char must be a separator (whitespace or the tag opener).
        let prev_ok = at == 0
            || lower_tag.as_bytes()[at - 1].is_ascii_whitespace()
            || lower_tag.as_bytes()[at - 1] == b'<';
        let after = at + name.len();
        let next_ok = matches!(
            lower_tag.as_bytes().get(after),
            Some(c) if c.is_ascii_whitespace() || *c == b'=' || *c == b'>' || *c == b'/'
        );
        if prev_ok && next_ok {
            return true;
        }
        search_from = at + name.len();
    }
    false
}

/// Extract the (case-insensitive) value of attribute `name` from an opening tag.
/// Handles single/double quotes and unquoted values. Returns `None` if absent.
fn attr_value(open_tag: &str, name: &str) -> Option<String> {
    let lower = open_tag.to_ascii_lowercase();
    let mut search_from = 0;
    loop {
        let rel = lower[search_from..].find(name)?;
        let at = search_from + rel;
        let prev_ok = at == 0
            || lower.as_bytes()[at - 1].is_ascii_whitespace()
            || lower.as_bytes()[at - 1] == b'<';
        let after = at + name.len();
        // Skip whitespace to find '='.
        let mut j = after;
        while matches!(open_tag.as_bytes().get(j), Some(c) if c.is_ascii_whitespace()) {
            j += 1;
        }
        if prev_ok && open_tag.as_bytes().get(j) == Some(&b'=') {
            j += 1;
            while matches!(open_tag.as_bytes().get(j), Some(c) if c.is_ascii_whitespace()) {
                j += 1;
            }
            let val = match open_tag.as_bytes().get(j) {
                Some(&b'"') => {
                    let start = j + 1;
                    let end = open_tag[start..].find('"').map(|e| start + e)?;
                    open_tag[start..end].to_string()
                }
                Some(&b'\'') => {
                    let start = j + 1;
                    let end = open_tag[start..].find('\'').map(|e| start + e)?;
                    open_tag[start..end].to_string()
                }
                _ => {
                    let start = j;
                    let end = open_tag[start..]
                        .find(|c: char| c.is_ascii_whitespace() || c == '>' || c == '/')
                        .map(|e| start + e)
                        .unwrap_or(open_tag.len());
                    open_tag[start..end].to_string()
                }
            };
            return Some(val);
        }
        search_from = at + name.len();
    }
}

/// Byte-substring search (no regex). Returns the index of the first occurrence
/// of `needle` in `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Produce a safe, double-quoted JS string literal for `s`.
fn json_string_literal(s: &str) -> String {
    // serde_json emits a valid JS/JSON string literal (handles quotes, control
    // chars, unicode). Good enough to embed in a script.
    serde_json::Value::String(s.to_string()).to_string()
}

/// Pick a polling tick: fine enough to honor `quiesce_ms` without busy-spinning.
fn quiesce_tick_ms(quiesce_ms: u64) -> u64 {
    // ~1/4 of quiesce, clamped to [5, 50] ms.
    (quiesce_ms / 4).clamp(5, 50)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_stub_body_defaults_empty_to_object() {
        assert_eq!(normalize_stub_body(""), "{}");
        assert_eq!(normalize_stub_body("   "), "{}");
        assert_eq!(normalize_stub_body(r#"{"a":1}"#), r#"{"a":1}"#);
    }

    #[test]
    fn extract_scripts_skips_external_and_data() {
        let html = r#"
            <html><head>
              <script src="/vendor.js"></script>
              <script type="application/json">{"not":"code"}</script>
              <script type="application/ld+json">{"@context":"x"}</script>
              <script>var a = 1;</script>
              <script type="module">import x from 'y';</script>
            </head><body></body></html>
        "#;
        let scripts = extract_inline_scripts(html);
        assert_eq!(scripts.len(), 2, "got: {scripts:?}");
        assert!(scripts[0].contains("var a = 1"));
        assert!(scripts[1].contains("import x"));
    }

    #[test]
    fn extract_scripts_preserves_less_than_in_rawtext() {
        // The whole point of the hand-written scanner: a bare `<` in JS must not
        // be treated as markup (which is what tl/html5ever inner-text does).
        let html = r#"<html><body><script>
            for (let i = 0; i < 100; i++) { if (i<<1 > 3 && a < b) fetch("/x/"+i); }
        </script></body></html>"#;
        let scripts = extract_inline_scripts(html);
        assert_eq!(scripts.len(), 1, "got: {scripts:?}");
        let s = &scripts[0];
        assert!(s.contains("i < 100"), "lost `<`: {s}");
        assert!(s.contains("i<<1"), "lost `<<`: {s}");
        assert!(s.contains("a < b"), "lost `< b`: {s}");
        assert!(s.contains(r#"fetch("/x/"+i)"#), "lost fetch call: {s}");
    }

    #[test]
    fn extract_scripts_case_insensitive_and_attrs() {
        let html = r#"<HTML><body>
            <SCRIPT TYPE="text/javascript">var ok = 1;</SCRIPT>
            <script SRC='/a.js'></script>
            <script type='application/json'>{"x":1}</script>
            <script defer>var deferred = 2;</script>
        </body></HTML>"#;
        let scripts = extract_inline_scripts(html);
        assert_eq!(scripts.len(), 2, "got: {scripts:?}");
        assert!(scripts[0].contains("var ok = 1"));
        assert!(scripts[1].contains("var deferred = 2"));
    }

    #[test]
    fn extract_scripts_handles_closing_tag_case_and_whitespace() {
        let html = "<script>var a=1;</SCRIPT >rest<script>var b=2;</script>";
        let scripts = extract_inline_scripts(html);
        assert_eq!(scripts.len(), 2, "got: {scripts:?}");
        assert!(scripts[0].contains("var a=1"));
        assert!(scripts[1].contains("var b=2"));
    }

    #[test]
    fn attr_value_variants() {
        assert_eq!(
            attr_value(r#"<script type="module">"#, "type").as_deref(),
            Some("module")
        );
        assert_eq!(
            attr_value(r#"<script type='text/babel'>"#, "type").as_deref(),
            Some("text/babel")
        );
        assert_eq!(
            attr_value("<script type=module >", "type").as_deref(),
            Some("module")
        );
        assert_eq!(attr_value("<script>", "type"), None);
    }

    #[test]
    fn attr_present_matches_boolean_and_valued() {
        assert!(attr_present(r#"<script src="/a.js">"#, "src"));
        assert!(attr_present("<script defer>", "defer"));
        assert!(!attr_present("<script>", "src"));
        // Must not match a substring inside another attr name/value.
        assert!(!attr_present(r#"<script data-nosrc="1">"#, "src"));
    }

    #[test]
    fn json_string_literal_escapes() {
        assert_eq!(json_string_literal("a\"b"), r#""a\"b""#);
        assert_eq!(
            json_string_literal("https://x/y?z=1"),
            r#""https://x/y?z=1""#
        );
    }

    #[test]
    fn quiesce_tick_is_clamped() {
        assert_eq!(quiesce_tick_ms(0), 5);
        assert_eq!(quiesce_tick_ms(40), 10);
        assert_eq!(quiesce_tick_ms(10_000), 50);
    }
}
