//! # draco-runtime — Tier 2 V8 isolate + fetch/XHR interception
//!
//! Boots a V8 isolate via `deno_core`, restores a **build-time snapshot** of the
//! DOM engine (**happy-dom** on a base of ecosystem web-primitive polyfills; see
//! `build.rs` + `vendor/happy-dom/`), runs the per-isolate glue (`js/glue.js`:
//! construct a happy-dom `Window`, mirror its DOM globals, install the
//! `fetch`/`XMLHttpRequest` interceptor, load the page HTML), evaluates the page's
//! inline scripts, and drives a **capture window**: the isolate runs until the
//! event loop goes idle for `quiesce_ms` or the hard `capture_window_ms` cap
//! elapses, whichever comes first. Every intercepted request is recorded
//! (rank-agnostic — ranking is `draco-core`'s job) and answered with a synthetic
//! stub so the page keeps hydrating and reveals more endpoints; when the window
//! closes the hydrated DOM is serialized for the render-then-Markdown escalation.
//!
//! This crate is **self-contained**: no IPC lives here. `draco-jail` calls
//! [`run_capture`] from the jailed child and maps each [`CapturedRequest`] to
//! `draco_types::JailToSupervisor::Intercept` and [`CaptureReport::outcome`] to a
//! `Result`. The frozen contract is `draco-types` (we reuse its
//! [`draco_types::InterceptVia`] and [`draco_types::RuntimeOutcome`]); this
//! crate's own API is designed for that wiring but is not itself frozen.
//!
//! ## Implementation notes
//!
//! * **DOM engine via a V8 startup snapshot.** happy-dom + its polyfill base are
//!   ~2.6 MB of JS; parsing that on every isolate spawn costs ~95 ms. Instead
//!   `build.rs` evaluates it once and serializes the V8 heap into a snapshot that
//!   each isolate restores in ~single-digit ms (see [`SNAPSHOT`]). The snapshot is
//!   heap + compiled code only — ops are registered per-isolate and resolved
//!   lazily by the baked JS (`Deno.core.ops.op_*`) after restore.
//!
//! * **`--jitless`.** We pass `--jitless` and `--single-threaded` via
//!   `deno_core::v8_set_flags` *before* the first isolate is created. `--jitless`
//!   disables the JIT (no RWX pages) so the jail's seccomp policy can forbid
//!   executable memory; `--single-threaded` avoids V8 background threads. Both
//!   reduce the jail's syscall surface. Measured cost is negligible: the isolate's
//!   work is snapshot restore + DOM construction, not hot JIT-tier loops, so JIT
//!   buys nothing here — we keep the W^X lockdown. Flags V8 rejects are reported
//!   and skipped (best-effort).
//!
//! * **Timers / event-loop driver.** deno_core 0.406.0's only timer reactor is
//!   tokio-based (`tokio::time::sleep_until`), so the event loop must run under a
//!   tokio time driver — a pure `futures::executor::block_on` would panic the
//!   moment a timer future is polled. Honoring the spec's *intent* (keep the
//!   jailed child's syscall surface small), we use a **current-thread** tokio
//!   runtime with **`enable_time()` only** — no worker pool, no I/O reactor. The
//!   base bundle's `setTimeout`/`setInterval` scheduler is backed by the
//!   `op_sleep` async op (tokio sleep); a pending `op_sleep` keeps
//!   `poll_event_loop` returning `Pending`, which is exactly the "loop is busy"
//!   signal the driver watches.

#![allow(clippy::type_complexity)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Once;
use std::task::Poll;
use std::time::{Duration, Instant};

use deno_core::{
    resolve_import, JsRuntime, ModuleLoadOptions, ModuleLoadReferrer, ModuleLoadResponse,
    ModuleLoader, ModuleResolveResponse, ModuleSource, ModuleSourceCode, ModuleSpecifier,
    ModuleType, OpState, PollEventLoopOptions, ResolutionKind, RuntimeOptions,
};
use deno_error::JsErrorBox;
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
    /// The hydrated DOM serialized as `document.documentElement.outerHTML` once
    /// the capture window closes — the raw material for the render-then-Markdown
    /// escalation (a thin client-rendered shell is hydrated here, then the
    /// content engine runs over *this* markup instead of the empty shell).
    /// `None` when serialization was not attempted or produced nothing usable
    /// (e.g. the isolate failed to boot, or the page left an empty body).
    pub rendered_html: Option<String>,
}

/// Boot an isolate, evaluate `html`'s inline scripts under `url`, run the capture
/// window, and return everything the page tried to fetch.
///
/// Never panics on page-author errors: a script that throws yields
/// [`RuntimeOutcome::Threw`] (with whatever was captured before the throw), and
/// the isolate is always torn down cleanly.
pub fn run_capture(url: &str, html: &str, cfg: &CaptureConfig) -> CaptureReport {
    run_capture_with_resources(url, html, cfg, HashMap::new())
}

/// As [`run_capture`], but with the page's script subresources pre-fetched by the
/// (air-gapped) supervisor: a `{ url -> source }` map used to run external
/// `<script src>` and to resolve `import` / `import()` for `type="module"` apps.
/// The isolate itself never fetches — this is how ES-module SPAs hydrate while
/// the child stays network-isolated.
pub fn run_capture_with_resources(
    url: &str,
    html: &str,
    cfg: &CaptureConfig,
    resources: HashMap<String, Vec<u8>>,
) -> CaptureReport {
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
                rendered_html: None,
            };
        }
    };

    rt.block_on(async move { run_capture_inner_with_resources(url, html, cfg, resources).await })
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
    /// Prefetched script/module/chunk resources keyed by absolute URL. Exposed to
    /// the page-side glue so dynamic `<script src>` chunk loaders can execute
    /// already-fetched chunks without giving the isolate network access.
    resources: Rc<RefCell<HashMap<String, Vec<u8>>>>,
    /// The hydrated DOM serialized after the capture window (via `op_raze_dom`),
    /// for the render-then-Markdown escalation. `None` until serialization runs.
    rendered_html: Option<String>,
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

/// Return a prefetched script/module/chunk resource by absolute URL. Used by the
/// glue's dynamic `<script src>` hook (webpack/Next chunk loader path). Missing
/// resources return `None` so the page-side loader can fire `onerror` exactly like
/// a failed network load.
#[deno_core::op2]
#[string]
fn op_raze_resource(state: &mut OpState, #[string] url: String) -> Option<String> {
    let cap = state.borrow::<Rc<RefCell<CaptureState>>>().clone();
    let resources = cap.borrow().resources.clone();
    let out = resources
        .borrow()
        .get(&url)
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned());
    out
}

/// Receive the hydrated DOM serialized by the page side
/// (`document.documentElement.outerHTML`) and stash it in [`CaptureState`] for the
/// render-then-Markdown escalation. Called once, after the capture window closes.
/// An empty or whitespace-only string is treated as "nothing to render".
#[deno_core::op2(fast)]
fn op_raze_dom(state: &mut OpState, #[string] html: String) {
    if html.trim().is_empty() {
        return;
    }
    let cap = state.borrow::<Rc<RefCell<CaptureState>>>().clone();
    cap.borrow_mut().rendered_html = Some(html);
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
    ops = [op_raze_fetch, op_sleep, op_resolve_url, op_raze_resource, op_raze_dom],
    options = { cap: Rc<RefCell<CaptureState>> },
    state = |state, options| {
        state.put::<Rc<RefCell<CaptureState>>>(options.cap);
    },
);

// ===================================================================
// Boot sequence + capture-window driver
// ===================================================================

/// The base web-primitive environment + happy-dom DOM engine, baked into a V8
/// startup snapshot at build time (see `build.rs`). Restoring it gives each
/// isolate the full DOM engine resident in ~ms instead of re-parsing ~2.6 MB of
/// JS. Ops are *not* in the snapshot — they are registered per-isolate below and
/// resolved lazily by the baked JS (`Deno.core.ops.op_*`) after restore.
static SNAPSHOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/DRACO_SNAPSHOT.bin"));

/// Per-isolate runtime glue (runs after snapshot restore): constructs a fresh
/// happy-dom `Window` for the target URL, mirrors its DOM globals onto
/// `globalThis`, installs the `op_raze_fetch` fetch/XHR interceptor, and loads the
/// fetched HTML so the framework's mount container exists.
const GLUE_JS: &str = include_str!("../js/glue.js");

// ===================================================================
// ES-module support: in-isolate module loader + script model
// ===================================================================

/// Module loader backed by a pre-fetched `{url -> source}` map (the page's script
/// subresources, fetched by the air-gapped supervisor and handed in). Serves
/// static + dynamic `import`s from the map; a module that isn't present (e.g. a
/// runtime-only lazy chunk the supervisor didn't prefetch) resolves to an **empty
/// module** rather than throwing, so a missing dynamic import can't crash the
/// hydration we're trying to observe.
struct MapModuleLoader {
    modules: Rc<RefCell<HashMap<String, Vec<u8>>>>,
}

impl ModuleLoader for MapModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
    ) -> ModuleResolveResponse {
        resolve_import(specifier, referrer).map_err(JsErrorBox::from_err)
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<&ModuleLoadReferrer>,
        _options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        let code = self
            .modules
            .borrow()
            .get(module_specifier.as_str())
            .cloned()
            .unwrap_or_default();
        let source = String::from_utf8_lossy(&code).into_owned();
        let module = ModuleSource::new(
            ModuleType::JavaScript,
            ModuleSourceCode::String(source.into()),
            module_specifier,
            None,
        );
        ModuleLoadResponse::Sync(Ok(module))
    }
}

/// One `<script>` from the page, in document order.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PageScript {
    /// `true` for an inline `<script>…</script>`; `false` for `<script src=…>`.
    inline: bool,
    /// `true` for `type="module"` (evaluated as an ES module).
    module: bool,
    /// Inline script body, or the raw `src` attribute for an external script.
    payload: String,
}

async fn run_capture_inner_with_resources(
    url: &str,
    html: &str,
    cfg: &CaptureConfig,
    resources: HashMap<String, Vec<u8>>,
) -> CaptureReport {
    let stub_body = normalize_stub_body(&cfg.stub_response_json);

    // Module loader + script-injection hook are backed by the supervisor-prefetched
    // script sources, so `<script type="module">`, `import()`, and webpack/Next
    // dynamic `<script src>` chunks resolve without the (air-gapped) isolate ever
    // touching the network.
    let modules = Rc::new(RefCell::new(resources));

    let cap = Rc::new(RefCell::new(CaptureState {
        requests: Vec::new(),
        max_intercepts: cfg.max_intercepts,
        stub_body: stub_body.clone(),
        resources: modules.clone(),
        rendered_html: None,
    }));

    // Restore the DOM-engine snapshot and register the ops for this isolate.
    let mut runtime = JsRuntime::new(RuntimeOptions {
        startup_snapshot: Some(SNAPSHOT),
        extensions: vec![draco_runtime_ext::init(cap.clone())],
        module_loader: Some(Rc::new(MapModuleLoader {
            modules: modules.clone(),
        })),
        ..Default::default()
    });

    // 1. Inject page inputs, then run the glue: it builds the happy-dom Window,
    //    mirrors DOM globals onto globalThis, installs the fetch/XHR interceptor,
    //    and loads the HTML into the document.
    let url_lit = json_string_literal(url);
    let html_lit = json_string_literal(html);
    let stub_lit = json_string_literal(&stub_body);
    if let Err(e) = runtime.execute_script(
        "draco:inputs",
        format!(
            "globalThis.__DRACO_URL__={url_lit}; globalThis.__DRACO_HTML__={html_lit}; \
             globalThis.__DRACO_STUB__={stub_lit};"
        ),
    ) {
        return finish(cap, RuntimeOutcome::Threw, Some(e.to_string()));
    }
    if let Err(e) = runtime.execute_script("draco:glue", GLUE_JS) {
        return finish(cap, RuntimeOutcome::Threw, Some(e.to_string()));
    }

    // 2. Evaluate the page's scripts in document order against the happy-dom
    //    document. Classic scripts (inline or fetched external) run via
    //    `execute_script`; ES modules (`type="module"`, inline or external) are
    //    loaded through the [`MapModuleLoader`] and evaluated, so `import` /
    //    `import()` resolve from the prefetched module map. A throw in page script
    //    is *not* fatal — later scripts and already-scheduled async work may still
    //    surface intercepts — but if it happens before anything is captured we
    //    remember it so the outcome is `Threw`.
    let scripts = extract_scripts(html);
    let mut threw_in_page = false;
    for (i, script) in scripts.into_iter().enumerate() {
        // Point document.currentScript at a fresh <script> for this block
        // (analytics/tag scripts read currentScript.parentElement); best-effort.
        let _ = runtime.execute_script(
            "draco:currentScript",
            "try { globalThis.__dracoSetCurrentScript(); } catch (_) {}",
        );

        // Resolve the source + its module specifier. Inline scripts use their
        // body verbatim and a synthetic per-index URL (based on the page URL, so
        // relative imports resolve against the page). External scripts use their
        // prefetched source, looked up by resolved URL; one we couldn't prefetch
        // is simply skipped (nothing to run).
        let (source, spec_str) = if script.inline {
            let base = url.split('#').next().unwrap_or(url);
            (script.payload.clone(), format!("{base}#draco-inline-{i}"))
        } else {
            let resolved = resolve_script_url(url, &script.payload);
            match modules.borrow().get(&resolved) {
                Some(bytes) => (String::from_utf8_lossy(bytes).into_owned(), resolved),
                None => continue,
            }
        };

        if script.module {
            // ES module: register the entry source under its specifier (so its own
            // relative imports resolve) and evaluate it, driving the event loop to
            // completion.
            match deno_core::url::Url::parse(&spec_str) {
                Ok(spec_url) => {
                    modules
                        .borrow_mut()
                        .insert(spec_url.as_str().to_string(), source.into_bytes());
                    if let Err(e) = eval_module(&mut runtime, &spec_url).await {
                        threw_in_page = true;
                        eprintln!("draco-runtime: module script {i} threw: {e}");
                    }
                }
                Err(e) => eprintln!("draco-runtime: bad module specifier for script {i}: {e}"),
            }
        } else {
            // Use an absolute script name so dynamic `import("./chunk.js")` inside
            // a classic inline script resolves against the page URL. A synthetic
            // non-hierarchical name like `draco:page[0]` becomes a cannot-be-a-base
            // referrer and breaks SvelteKit/Vite bootstraps.
            let name = if script.inline {
                spec_str.clone()
            } else {
                format!("draco:page[{i}]")
            };
            if let Err(e) = runtime.execute_script(name, source) {
                threw_in_page = true;
                eprintln!("draco-runtime: page script {i} threw: {e}");
            }
        }
    }
    let _ = runtime.execute_script(
        "draco:currentScript:clear",
        "try { globalThis.__dracoClearCurrentScript(); } catch (_) {}",
    );

    // 3. Capture window: pump the event loop until quiescence or the hard cap.
    let outcome = drive_capture_window(&mut runtime, &cap, cfg, threw_in_page).await;

    // 4. Serialize the hydrated DOM for the render-then-Markdown escalation, after
    //    the window so any content the framework mounted is present.
    serialize_dom(&mut runtime);

    let cs = cap.borrow();
    CaptureReport {
        outcome,
        requests: cs.requests.clone(),
        rendered_html: cs.rendered_html.clone(),
    }
}

/// Serialize the live hydrated DOM (`document.documentElement.outerHTML`, via the
/// glue's `__dracoSerialize`) and hand it back through `op_raze_dom`. Wrapped in
/// an in-page `try/catch` so a throwing getter can never propagate; a failure to
/// even run the script is swallowed (the render escalation simply sees no DOM).
fn serialize_dom(runtime: &mut JsRuntime) {
    const SERIALIZE_JS: &str = r#"(function () {
        try {
            var h = (typeof globalThis.__dracoSerialize === "function")
                ? globalThis.__dracoSerialize()
                : (globalThis.document && globalThis.document.documentElement
                    ? globalThis.document.documentElement.outerHTML : "");
            Deno.core.ops.op_raze_dom(h || "");
        } catch (_e) {
            try { Deno.core.ops.op_raze_dom(""); } catch (_e2) {}
        }
    })()"#;

    if let Err(e) = runtime.execute_script("draco:serialize-dom", SERIALIZE_JS) {
        eprintln!("draco-runtime: DOM serialization script failed: {e}");
    }
}

/// Load and evaluate one ES module to completion, driving the event loop so its
/// (and its imports') top-level bodies run before we return. The module source is
/// served by the [`MapModuleLoader`].
async fn eval_module(
    runtime: &mut JsRuntime,
    spec: &ModuleSpecifier,
) -> Result<(), deno_core::error::CoreError> {
    let id = runtime.load_side_es_module(spec).await?;
    let eval = runtime.mod_evaluate(id);
    runtime
        .run_event_loop(PollEventLoopOptions::default())
        .await?;
    eval.await
}

/// Resolve a script `src` against the page URL (WHATWG join); passes `src`
/// through unchanged if either cannot be parsed.
fn resolve_script_url(base: &str, src: &str) -> String {
    match deno_core::url::Url::parse(base).and_then(|b| b.join(src)) {
        Ok(u) => u.to_string(),
        Err(_) => src.to_string(),
    }
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
    // `finish` is only reached on a pre-hydration boot failure (URL inject /
    // polyfill / interceptor threw), so there is no meaningful hydrated DOM to
    // serialize.
    CaptureReport {
        outcome,
        requests,
        rendered_html: None,
    }
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
/// HTML **comments** (`<!-- … -->`) are skipped wholesale before we ever look
/// for a tag: a `<script>` written inside a comment is not a real element, so
/// treating it as an opening tag would desync extraction (we'd scan for a
/// `</script>` that closes the *real* script and swallow everything between).
/// When the scanner meets `<!--` at (or before) the next `<script`, it advances
/// past the matching `-->` and interprets nothing inside as markup. Note this is
/// only for comments *outside* a script body — a `<!--` appearing inside rawtext
/// script content is captured verbatim like any other byte.
///
/// External scripts (`src=...`) and non-executable `type`s (JSON-LD, importmap,
/// `application/json`, `__NEXT_DATA__`, `speculationrules`, templates, …) are
/// skipped — we only run real JS.
fn extract_scripts(html: &str) -> Vec<PageScript> {
    let mut out = Vec::new();
    let bytes = html.as_bytes();
    let lower = html.to_ascii_lowercase();
    let lb = lower.as_bytes();
    let mut i = 0usize;

    while let Some(rel) = find_subslice(&lb[i..], b"<script") {
        let tag_start = i + rel;

        // Skip any HTML comment that opens at or before this `<script`. A comment
        // is markup we must ignore entirely (§ rawtext note above): if `<!--`
        // begins before `tag_start`, this `<script` is inside a comment (or a
        // comment sits between us and it), so jump past the comment's `-->` and
        // re-scan. Advancing past the *matching* `-->` is what keeps a
        // `<script>` mention inside the comment from being read as a real tag.
        if let Some(crel) = find_subslice(&lb[i..], b"<!--") {
            let comment_start = i + crel;
            if comment_start <= tag_start {
                // Find the comment terminator; an unterminated comment eats the
                // rest of the document (matching how browsers treat it).
                i = match find_subslice(&lb[comment_start + b"<!--".len()..], b"-->") {
                    Some(erel) => comment_start + b"<!--".len() + erel + b"-->".len(),
                    None => bytes.len(),
                };
                continue;
            }
        }

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

        if let Some(is_module) = classify_script(open_tag) {
            if let Some(src) = attr_value(open_tag, "src") {
                let src = src.trim().to_string();
                if !src.is_empty() {
                    out.push(PageScript {
                        inline: false,
                        module: is_module,
                        payload: src,
                    });
                }
            } else if !body.trim().is_empty() {
                out.push(PageScript {
                    inline: true,
                    module: is_module,
                    payload: body.to_string(),
                });
            }
        }
        i = next_i;
    }
    out
}

/// Inline-only JS bodies, in document order — a thin filter over
/// [`extract_scripts`] retained for the scanner's focused unit tests.
#[cfg(test)]
fn extract_inline_scripts(html: &str) -> Vec<String> {
    extract_scripts(html)
        .into_iter()
        .filter(|s| s.inline)
        .map(|s| s.payload)
        .collect()
}

/// Classify an opening `<script ...>` tag: `Some(is_module)` if it is executable
/// JavaScript (any inline/external JS `type`, with `is_module` set for
/// `type="module"`); `None` for a non-JS type (`application/json`, `importmap`,
/// `speculationrules`, …) that must not be executed.
fn classify_script(open_tag: &str) -> Option<bool> {
    match attr_value(open_tag, "type") {
        None => Some(false),
        Some(ty) => {
            let ty = ty.trim().to_ascii_lowercase();
            if ty.is_empty()
                || ty == "text/javascript"
                || ty == "application/javascript"
                || ty == "text/ecmascript"
                || ty == "application/ecmascript"
                || ty == "text/babel"
                || ty == "text/jsx"
            {
                Some(false)
            } else if ty == "module" {
                Some(true)
            } else {
                None
            }
        }
    }
}

/// True if attribute `name` appears in the (already-lowercased) opening tag.
/// Matches `name=`, `name ` or `name>` / `name/` (bare boolean attribute).
#[cfg(test)]
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
    fn extract_scripts_skips_script_tag_inside_html_comment() {
        // A `<script>` *mentioned inside* an HTML comment must NOT be read as a
        // real opening tag — otherwise the scanner would pair it with the real
        // script's `</script>` and desync extraction. Only the one real inline
        // script should be extracted (and thus evaluated).
        let html = r#"<html><body>
            <!-- disabled during rollout:
                 <script>fetch("/api/legacy"); var x = a < b;</script>
                 keep this commented out -->
            <script>fetch("/api/real");</script>
            <!-- trailing note, also <script>ignored()</script> -->
        </body></html>"#;
        let scripts = extract_inline_scripts(html);
        assert_eq!(scripts.len(), 1, "only the real script; got: {scripts:?}");
        assert!(
            scripts[0].contains(r#"fetch("/api/real")"#),
            "wrong script extracted: {:?}",
            scripts[0]
        );
        // The commented-out script's contents must never surface.
        assert!(
            !scripts[0].contains("/api/legacy"),
            "commented script leaked into extraction: {:?}",
            scripts[0]
        );
        assert!(
            !scripts[0].contains("ignored()"),
            "trailing commented script leaked: {:?}",
            scripts[0]
        );
    }

    #[test]
    fn extract_scripts_preserves_comment_syntax_inside_rawtext_body() {
        // A `<!--` (or `-->`) appearing *inside* a real script body is rawtext,
        // not a comment: it must be captured verbatim and must not cause the
        // scanner to skip the script. (Legacy "hide from ancient browsers" trick.)
        let html =
            "<script><!--\nvar keep = 1; if (a < b) go(); //--></script><script>var two=2;</script>";
        let scripts = extract_inline_scripts(html);
        assert_eq!(scripts.len(), 2, "got: {scripts:?}");
        assert!(
            scripts[0].contains("var keep = 1"),
            "body 1: {:?}",
            scripts[0]
        );
        assert!(
            scripts[0].contains("<!--"),
            "lost `<!--` in body: {:?}",
            scripts[0]
        );
        assert!(
            scripts[0].contains("//-->"),
            "lost `//-->` in body: {:?}",
            scripts[0]
        );
        assert!(scripts[1].contains("var two=2"), "body 2: {:?}", scripts[1]);
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
    fn appended_script_chunk_runs_from_prefetched_resources() {
        let html = r#"<script>
            const s = document.createElement("script");
            s.src = "/_next/static/chunks/feature.abc123.js";
            document.head.appendChild(s);
        </script>"#;
        let mut resources = HashMap::new();
        resources.insert(
            "https://example.com/_next/static/chunks/feature.abc123.js".to_string(),
            br#"fetch("/api/from-chunk");"#.to_vec(),
        );
        let report = run_capture_with_resources(
            "https://example.com/",
            html,
            &CaptureConfig {
                capture_window_ms: 500,
                quiesce_ms: 20,
                max_intercepts: 8,
                stub_response_json: "{}".to_string(),
            },
            resources,
        );
        assert_eq!(report.requests.len(), 1, "{report:?}");
        assert_eq!(report.requests[0].url, "https://example.com/api/from-chunk");
    }

    #[test]
    fn classic_inline_dynamic_import_resolves_against_page_url() {
        let html = r#"<script>
            import("./entry.js").then((m) => m.run());
        </script>"#;
        let mut resources = HashMap::new();
        resources.insert(
            "https://example.com/app/entry.js".to_string(),
            br#"export function run() { fetch("./api/data"); }"#.to_vec(),
        );
        let report = run_capture_with_resources(
            "https://example.com/app/",
            html,
            &CaptureConfig {
                capture_window_ms: 500,
                quiesce_ms: 20,
                max_intercepts: 8,
                stub_response_json: "{}".to_string(),
            },
            resources,
        );
        assert_eq!(report.requests.len(), 1, "{report:?}");
        assert_eq!(report.requests[0].url, "https://example.com/app/api/data");
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
