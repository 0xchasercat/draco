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
//! This crate hosts the isolate **in-process**: `draco-core` calls [`run_capture`]
//! directly (from a dedicated `spawn_blocking` thread, since `JsRuntime` is
//! `!Send`) and maps each [`CapturedRequest`] and [`CaptureReport::outcome`] into
//! its ladder. There is no separate process and no IPC: script, module, and chunk
//! bytes are pulled by an async [`ScriptFetcher`] the caller supplies (backed by
//! the pooled `draco-net` client + the immutable chunk cache) and awaited directly
//! on the event loop. The shared contract is `draco-types` (we reuse its
//! [`draco_types::InterceptVia`] and [`draco_types::RuntimeOutcome`]).
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
//! * **Concurrent, non-blocking chunk loading.** The module loader and the
//!   dynamic-`<script>` op are async: each `import` / injected chunk `.await`s the
//!   [`ScriptFetcher`], so a code-split SPA's chunks fan out concurrently over the
//!   event loop (the reactor drives many `draco-net` sockets in parallel) instead
//!   of paying one blocking round-trip at a time — the whole point of the
//!   in-process rewrite. An in-flight-load counter keeps the capture window from
//!   quiescing while loads are still outstanding.
//!
//! * **JIT enabled; `--single-threaded`.** We pass `--single-threaded` via
//!   `deno_core::v8_set_flags` *before* the first isolate is created; V8's JIT is
//!   left ON. Real SPA hydration is hot JS (React/SvelteKit reconcilers, not just
//!   snapshot restore + DOM construction), and `--jitless` ran it 3–10× slower.
//!   Containment does not rest on W^X: the page JS runs in an isolate with **no
//!   host-capability bindings** — it cannot reach the network, filesystem, or
//!   process regardless of JIT; the only I/O it can cause is the script fetches we
//!   explicitly broker through the [`ScriptFetcher`]. `--single-threaded` keeps V8
//!   from spawning background compiler/GC threads (JIT still runs on the main
//!   thread). Flags V8 rejects are reported and skipped (best-effort).
//!
//! * **Timers / event-loop driver.** deno_core 0.406.0's timer reactor is
//!   tokio-based, and the isolate now performs real async network I/O, so
//!   [`run_capture`] drives the event loop under a **current-thread** tokio runtime
//!   built with **`enable_all()`** (I/O + time). Current-thread because `JsRuntime`
//!   is `!Send`; the full driver set because `draco-net` sockets and `op_sleep`
//!   both need it. The base bundle's `setTimeout`/`setInterval` scheduler is backed
//!   by the `op_sleep` async op; a pending `op_sleep` (or an in-flight fetch) keeps
//!   `poll_event_loop` returning `Pending`, which is exactly the "loop is busy"
//!   signal the driver watches.

#![allow(clippy::type_complexity)]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, Once, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use deno_core::{
    resolve_import, JsRuntime, ModuleLoadOptions, ModuleLoadReferrer, ModuleLoadResponse,
    ModuleLoader, ModuleResolveResponse, ModuleSource, ModuleSourceCode, ModuleSpecifier,
    ModuleType, OpState, PollEventLoopOptions, ResolutionKind, RuntimeOptions,
};
use deno_error::JsErrorBox;
use futures::future::LocalBoxFuture;
use futures::FutureExt;
use serde::{Deserialize, Serialize};

use draco_types::{InterceptVia, RuntimeOutcome};

/// Interact sessions — a resumable isolate driven turn-by-turn (v0.17.0).
/// Reuses this module's capture primitives; see `session` for the actor model.
pub mod session;

/// Bound the per-page V8 heap so allocation-heavy SPAs trigger collection
/// instead of growing toward V8's device-derived multi-gigabyte default.
const MIB: usize = 1024 * 1024;
const CAPTURE_MAX_HEAP_BYTES: usize = 192 * MIB;

/// Process-lifetime V8 isolate ownership counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IsolateStats {
    pub created: u64,
    pub dropped: u64,
    pub active: u64,
}

#[derive(Default)]
struct IsolateCounters {
    stats: Mutex<IsolateStats>,
}

impl IsolateCounters {
    fn track(&'static self) -> IsolateLifecycleGuard {
        let mut stats = self.stats.lock().unwrap_or_else(|p| p.into_inner());
        stats.created = stats.created.saturating_add(1);
        stats.active = stats.active.saturating_add(1);
        IsolateLifecycleGuard { counters: self }
    }

    fn snapshot(&self) -> IsolateStats {
        *self.stats.lock().unwrap_or_else(|p| p.into_inner())
    }
}

fn isolate_counters() -> &'static IsolateCounters {
    static COUNTERS: OnceLock<IsolateCounters> = OnceLock::new();
    COUNTERS.get_or_init(IsolateCounters::default)
}

/// Snapshot the process-lifetime isolate lifecycle counters.
pub fn isolate_stats() -> IsolateStats {
    isolate_counters().snapshot()
}

struct IsolateLifecycleGuard {
    counters: &'static IsolateCounters,
}

impl Drop for IsolateLifecycleGuard {
    fn drop(&mut self) {
        let mut stats = self
            .counters
            .stats
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        stats.dropped = stats.dropped.saturating_add(1);
        stats.active = stats.active.saturating_sub(1);
    }
}

/// Owns a production isolate and its lifecycle guard. Rust drops struct fields
/// in declaration order, so `runtime` tears down before `_lifecycle` decrements
/// the active counter.
pub(crate) struct TrackedJsRuntime {
    runtime: JsRuntime,
    _lifecycle: IsolateLifecycleGuard,
}

impl TrackedJsRuntime {
    fn new(options: RuntimeOptions) -> Self {
        let runtime = JsRuntime::new(options);
        Self {
            runtime,
            _lifecycle: isolate_counters().track(),
        }
    }
}

impl Deref for TrackedJsRuntime {
    type Target = JsRuntime;

    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}

impl DerefMut for TrackedJsRuntime {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.runtime
    }
}

fn capture_create_params() -> deno_core::v8::CreateParams {
    deno_core::v8::CreateParams::default().heap_limits(0, CAPTURE_MAX_HEAP_BYTES)
}

/// Turn a near-heap-limit condition into a catchable execution termination.
/// Doubling the current limit gives V8 enough headroom to unwind cleanly; the
/// configured starting limit remains [`CAPTURE_MAX_HEAP_BYTES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeapLimitExceeded;

#[derive(Clone)]
pub(crate) struct HeapLimitGuard {
    tripped: Rc<Cell<bool>>,
}

impl HeapLimitGuard {
    fn new() -> Self {
        Self {
            tripped: Rc::new(Cell::new(false)),
        }
    }

    fn is_tripped(&self) -> bool {
        self.tripped.get()
    }

    fn check(&self) -> Result<(), HeapLimitExceeded> {
        if self.is_tripped() {
            Err(HeapLimitExceeded)
        } else {
            Ok(())
        }
    }

    fn run<T>(&self, operation: impl FnOnce() -> T) -> Result<T, HeapLimitExceeded> {
        self.check()?;
        let value = operation();
        self.check()?;
        Ok(value)
    }

    #[cfg(test)]
    fn new_for_test() -> Self {
        Self::new()
    }

    #[cfg(test)]
    fn trip_for_test(&self) {
        self.tripped.set(true);
    }
}

fn install_near_heap_limit_guard(runtime: &mut JsRuntime) -> HeapLimitGuard {
    let guard = HeapLimitGuard::new();
    let callback_guard = guard.clone();
    let handle = runtime.v8_isolate().thread_safe_handle();
    runtime.add_near_heap_limit_callback(move |current_limit, _initial_limit| {
        callback_guard.tripped.set(true);
        handle.terminate_execution();
        current_limit.saturating_mul(2)
    });
    guard
}

const HEAP_LIMIT_DIAGNOSTIC: &str = "V8 heap limit reached; isolate terminated and abandoned";
pub(crate) const EXECUTION_DEADLINE_DIAGNOSTIC: &str =
    "V8 execution deadline reached; isolate terminated and abandoned";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecutionBoundaryError {
    HeapLimit,
    Deadline,
    WatchdogUnavailable,
}

pub(crate) struct ExecutionWatchdog {
    commands: Option<mpsc::SyncSender<WatchdogCommand>>,
    join: Option<std::thread::JoinHandle<()>>,
    tripped_generation: Arc<AtomicU64>,
    next_generation: u64,
}

enum WatchdogCommand {
    Arm {
        generation: u64,
        deadline: Duration,
    },
    Disarm {
        generation: u64,
        acknowledged: mpsc::SyncSender<()>,
    },
    Shutdown,
}

impl ExecutionWatchdog {
    pub(crate) fn start(runtime: &mut JsRuntime) -> Result<Self, ExecutionBoundaryError> {
        let handle = runtime.v8_isolate().thread_safe_handle();
        let (commands, command_rx) = mpsc::sync_channel(1);
        let tripped_generation = Arc::new(AtomicU64::new(0));
        let thread_tripped = Arc::clone(&tripped_generation);
        let join = std::thread::Builder::new()
            .name("draco-v8-watchdog".to_string())
            .spawn(move || loop {
                match command_rx.recv() {
                    Ok(WatchdogCommand::Arm {
                        generation,
                        deadline,
                    }) => match command_rx.recv_timeout(deadline) {
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            thread_tripped.store(generation, Ordering::Release);
                            handle.terminate_execution();
                        }
                        Ok(WatchdogCommand::Disarm {
                            generation: disarmed_generation,
                            acknowledged,
                        }) if disarmed_generation == generation => {
                            let _ = acknowledged.send(());
                        }
                        Ok(WatchdogCommand::Disarm { acknowledged, .. }) => {
                            let _ = acknowledged.send(());
                        }
                        Ok(WatchdogCommand::Shutdown)
                        | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Ok(WatchdogCommand::Arm { .. }) => {
                            // Arms are serialized by synchronous disarm acknowledgements.
                            break;
                        }
                    },
                    Ok(WatchdogCommand::Disarm { acknowledged, .. }) => {
                        // The deadline won the race; acknowledge the late disarm.
                        let _ = acknowledged.send(());
                    }
                    Ok(WatchdogCommand::Shutdown) | Err(_) => break,
                }
            })
            .map_err(|_| ExecutionBoundaryError::WatchdogUnavailable)?;
        Ok(Self {
            commands: Some(commands),
            join: Some(join),
            tripped_generation,
            next_generation: 0,
        })
    }

    fn arm(&mut self, deadline: Duration) -> Result<u64, ExecutionBoundaryError> {
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let generation = self.next_generation;
        self.commands
            .as_ref()
            .ok_or(ExecutionBoundaryError::WatchdogUnavailable)?
            .send(WatchdogCommand::Arm {
                generation,
                deadline,
            })
            .map_err(|_| ExecutionBoundaryError::WatchdogUnavailable)?;
        Ok(generation)
    }

    fn disarm(&mut self, generation: u64) -> Result<bool, ExecutionBoundaryError> {
        let (acknowledged, ack_rx) = mpsc::sync_channel(1);
        self.commands
            .as_ref()
            .ok_or(ExecutionBoundaryError::WatchdogUnavailable)?
            .send(WatchdogCommand::Disarm {
                generation,
                acknowledged,
            })
            .map_err(|_| ExecutionBoundaryError::WatchdogUnavailable)?;
        ack_rx
            .recv()
            .map_err(|_| ExecutionBoundaryError::WatchdogUnavailable)?;
        Ok(self.tripped_generation.load(Ordering::Acquire) == generation)
    }

    fn stop(&mut self) {
        if let Some(commands) = self.commands.take() {
            let _ = commands.send(WatchdogCommand::Shutdown);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for ExecutionWatchdog {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(crate) fn run_with_execution_deadline<T>(
    runtime: &mut JsRuntime,
    heap_guard: &HeapLimitGuard,
    watchdog: &mut ExecutionWatchdog,
    deadline: Duration,
    operation: impl FnOnce(&mut JsRuntime) -> T,
) -> Result<T, ExecutionBoundaryError> {
    let generation = watchdog.arm(deadline)?;
    let result = heap_guard.run(|| operation(runtime));
    if watchdog.disarm(generation)? {
        return Err(ExecutionBoundaryError::Deadline);
    }
    result.map_err(|_| ExecutionBoundaryError::HeapLimit)
}

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
    /// Bounded page-side diagnostics: glue-swallowed exceptions/rejections,
    /// `console.error`/`console.warn` lines, and page-script throws. The raw
    /// material for debugging *why* a page failed to hydrate (e.g. a missing
    /// browser API aborting a framework boot) without a browser devtools.
    /// Count- and length-capped in [`CaptureState::push_log`]; the supervisor
    /// surfaces them as `runtime.log` trace steps when asked to.
    pub logs: Vec<String>,
}

/// Immutable script/module bytes shared across caches, registries, and fetchers.
pub type SharedSource = Arc<[u8]>;

/// Async, in-process source of script / module / chunk bytes for the isolate.
///
/// The runtime itself stays network-agnostic: `draco-core` implements this over
/// its pooled `draco-net` client (plus the immutable chunk cache), and the isolate
/// `.await`s it directly on the event loop. The module loader and
/// dynamic-`<script>` op await this directly; parser-inserted external scripts use
/// a bounded two-script lookahead while preserving document order. There is no IPC
/// or blocking round-trip. `None` rejects exactly that one load, the way a browser
/// treats a 404'd chunk.
///
/// The future is `!Send` on purpose: the whole capture is single-threaded (V8 is
/// thread-bound), so loads are driven on the isolate's own event loop as a
/// [`LocalBoxFuture`], matching deno_core's `boxed_local` module-loader idiom.
pub trait ScriptFetcher {
    fn fetch<'a>(&'a self, url: &'a str) -> LocalBoxFuture<'a, Option<SharedSource>>;
}

/// A page-side network request the isolate wants to make (`window.fetch` /
/// `XMLHttpRequest` / an SSE/WebSocket open), carried in full so the caller can
/// issue it faithfully.
#[derive(Debug, Clone)]
pub struct ApiRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

/// The response handed back to the page for an [`ApiRequest`] — the *real* status,
/// headers, and body when the caller fetched it live, so a framework router runs
/// its native success/error paths (a genuine 403-JSON renders the logged-out view
/// instead of throwing on a synthetic 404).
#[derive(Debug, Clone)]
pub struct ApiResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Async, in-process responder for the page's own data requests (`fetch`/XHR).
///
/// Distinct from [`ScriptFetcher`] (code loads are `url -> bytes`); a data request
/// needs full fidelity both ways: method + headers + body in, real status +
/// headers + body out. `draco-core` implements this over its pooled `draco-net`
/// client under a stub-vs-live **policy** — a pure-CSR SPA's content only exists
/// after its data fetches resolve, so `scrape` runs the safe ones live while
/// `discover` stubs. Returning `None` means "no live response — use the built-in
/// synthetic stub" (Observe mode, or a request the policy declined). The runtime
/// records the request for `discover` regardless of what this returns.
pub trait ApiFetcher {
    fn fetch<'a>(&'a self, req: &'a ApiRequest) -> LocalBoxFuture<'a, Option<ApiResponse>>;
}

/// Boot an isolate, evaluate `html`'s scripts under `url`, run the capture window,
/// and return everything the page tried to fetch plus the hydrated DOM.
///
/// Runs **in-process**: the caller owns the thread (V8 is thread-bound and `!Send`,
/// so `draco-core` invokes this from a dedicated `spawn_blocking` thread), and this
/// function owns a current-thread tokio runtime with the full I/O + time drivers so
/// the isolate's async script/chunk loads — served by `fetcher` over `draco-net` —
/// make progress and fan out concurrently on the event loop.
///
/// Never panics on page-author errors: a script that throws yields
/// [`RuntimeOutcome::Threw`] (with whatever was captured before the throw), and
/// the isolate is always torn down cleanly.
pub fn run_capture(
    url: &str,
    html: &str,
    cfg: &CaptureConfig,
    fetcher: Rc<dyn ScriptFetcher>,
) -> CaptureReport {
    run_capture_impl(url, html, cfg, fetcher, None)
}

/// As [`run_capture`], but in **Render mode**: the page's own data requests
/// (`fetch`/XHR) are answered by `api_fetcher`, which `draco-core` routes to the
/// live network (`draco-net`) for the requests its policy deems safe — so a
/// pure-CSR shell's content actually materializes before the DOM is serialized.
/// Requests the policy declines fall back to the same synthetic stub
/// [`run_capture`] always uses.
pub fn run_capture_render(
    url: &str,
    html: &str,
    cfg: &CaptureConfig,
    fetcher: Rc<dyn ScriptFetcher>,
    api_fetcher: Rc<dyn ApiFetcher>,
) -> CaptureReport {
    run_capture_impl(url, html, cfg, fetcher, Some(api_fetcher))
}

fn run_capture_impl(
    url: &str,
    html: &str,
    cfg: &CaptureConfig,
    fetcher: Rc<dyn ScriptFetcher>,
    api_fetcher: Option<Rc<dyn ApiFetcher>>,
) -> CaptureReport {
    ensure_v8_flags();

    // Current-thread tokio runtime with the FULL driver set. The isolate's async
    // ops (`op_raze_load_script`, `op_raze_fetch`) and module loader `.await` real
    // `draco-net` fetches, which need the I/O reactor; `enable_all()` also provides
    // the time driver that backs `op_sleep`. Current-thread because `JsRuntime` is
    // `!Send`.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
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
                logs: vec![format!("boot: failed to build tokio runtime: {e}")],
            };
        }
    };

    rt.block_on(async move { run_capture_inner(url, html, cfg, fetcher, api_fetcher).await })
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
    /// In-process async source of script/module/chunk bytes (net + chunk cache),
    /// awaited by `op_raze_load_script` for dynamic `<script src>` chunks. The
    /// module loader holds its own clone for `import` / `import()`.
    fetcher: Rc<dyn ScriptFetcher>,
    /// Optional in-process responder for the page's own data requests (fetch/XHR),
    /// awaited by `op_raze_fetch`. `Some` in Render mode (live data via draco-net
    /// under a stub-vs-live policy); `None` in Observe mode (built-in synthetic
    /// stub, the default that `discover` and SSR/hybrid `scrape` use).
    api_fetcher: Option<Rc<dyn ApiFetcher>>,
    /// Count of script/module loads currently in flight. The capture-window driver
    /// treats a non-zero count as activity so the window cannot quiesce while the
    /// page is still pulling code concurrently — the async analogue of the old
    /// blocking loader keeping the single thread busy.
    inflight: Rc<Cell<u32>>,
    /// Monotonically-increasing count of **content** requests (non-tracker
    /// `fetch`/XHR). The capture window measures its quiesce streak against THIS
    /// rather than the raw request count, so analytics/session-replay/ad beacons
    /// that fire after the content has settled cannot keep the window open. They
    /// are still recorded (discovery) — they just don't count as progress.
    content_activity: Rc<Cell<u32>>,
    /// The hydrated DOM serialized after the capture window (via `op_raze_dom`),
    /// for the render-then-Markdown escalation. `None` until serialization runs.
    rendered_html: Option<String>,
    /// Page-side diagnostic lines (see [`CaptureReport::logs`]). Fed by
    /// `op_raze_log` (glue) and the Rust-side script-throw sites; bounded by
    /// [`CaptureState::push_log`].
    logs: Vec<String>,
    /// Serialized JSON of the most recent interact `exec` turn's completion value,
    /// stashed by `op_raze_exec_result` and drained per turn. Unused by the
    /// one-shot capture path (`run_capture`); it is the devtools-console return
    /// channel for [`session`](crate::session).
    exec_result: Option<String>,
    /// Monotonic clock for this capture, started at isolate construction. The
    /// `[raze.*]` diagnostics stamp `[+{ms}]` off it so `--runtime-log` reads as a
    /// timeline — when each fetch/chunk resolved and when the window closed — the
    /// raw material for tuning the capture-window early-exit.
    started: Instant,
}

/// Hard bounds on collected diagnostic log lines, so a pathological page (e.g. a
/// `console.error` in a render loop) cannot balloon the report or the IPC frames
/// that carry it.
const MAX_RUNTIME_LOGS: usize = 96;
const MAX_LOG_CHARS: usize = 1_024;

impl CaptureState {
    fn take_output_buffers(&mut self) -> (Vec<CapturedRequest>, Option<String>, Vec<String>) {
        (
            std::mem::take(&mut self.requests),
            self.rendered_html.take(),
            std::mem::take(&mut self.logs),
        )
    }

    /// Append one diagnostic line, enforcing the count cap, truncating overlong
    /// lines on a char boundary, and **deduplicating exact repeats**. A framework
    /// warning emitted once per component (e.g. a CSS-variable warn ×25) would
    /// otherwise exhaust the line budget and evict the one error that explains the
    /// failure; each distinct line carries all the diagnostic signal its repeats
    /// would. At the cap, structured `[raze.memory]` records may evict the oldest
    /// ordinary record so phase telemetry survives page-log saturation; ordinary
    /// records are still silently dropped.
    fn push_log(&mut self, line: &str) {
        let mut s: String = line.chars().take(MAX_LOG_CHARS).collect();
        if s.len() < line.len() {
            s.push('…');
        }
        // Bounded scan (≤ MAX_RUNTIME_LOGS entries): an exact repeat adds nothing.
        if self.logs.contains(&s) {
            return;
        }
        if self.logs.len() >= MAX_RUNTIME_LOGS {
            if !s.starts_with("[raze.memory] ") {
                return;
            }
            let Some(ordinary_index) = self
                .logs
                .iter()
                .position(|existing| !existing.starts_with("[raze.memory] "))
            else {
                return;
            };
            self.logs.remove(ordinary_index);
        }
        self.logs.push(s);
    }
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

/// Structured response crossing the Rust/V8 op boundary. Keeping the body as a
/// string is intentional: page API responses have historically been decoded
/// with `String::from_utf8_lossy`, and changing that policy would alter SPA
/// behavior. `serde_v8` serializes this value directly into a JS object, avoiding
/// the former JSON string envelope around an already-JSON-shaped response.
#[derive(Serialize)]
struct ApiResponseWire {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

/// Known third-party analytics / session-replay / ad / tag-manager / bot-detection
/// hosts that never contribute page **content**. A request to one is still recorded
/// (so `discover` sees it) and still fetched live (so page hydration behaves
/// normally), but it does **not** hold the capture window open or reset its quiesce
/// streak. Without this, a tracker beacon firing seconds after the content settled
/// pins the window to the hard cap — e.g. target.com's main content lands at ~1.8s
/// but FullStory (257 KB), DoubleVerify, googlesyndication, Attentive and Medallia
/// keep firing until ~3.5s, tripling the render time for zero extra content.
///
/// Matched as a case-insensitive substring of the request URL's host. Deliberately
/// conservative: only unambiguous non-content vendors, so a first-party data host is
/// never misclassified.
fn is_tracker(url: &str) -> bool {
    // Host = between "://" and the next '/', '?' or '#'. Falls back to the whole
    // string (matching still works; we simply didn't isolate the host).
    let host = url
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(rest))
        .unwrap_or(url)
        .to_ascii_lowercase();
    const TRACKERS: &[&str] = &[
        "fullstory.com",
        "doubleverify.com",
        "doubleclick.net",
        "googlesyndication.com",
        "google-analytics.com",
        "googletagmanager.com",
        "analytics.google.com",
        "amplitude.com",
        "segment.io",
        "segment.com",
        "attentivemobile.com",
        "attn.tv",
        "medallia.com",
        "intercomcdn.com",
        "intercom.io",
        "api-iam.intercom",
        "px-cloud.net",
        "perimeterx",
        "zeronaught.com",
        "hotjar.com",
        "hotjar.io",
        "mixpanel.com",
        "heapanalytics.com",
        "sentry.io",
        "bugsnag.com",
        "datadoghq.com",
        "nr-data.net",
        "newrelic.com",
        "optimizely.com",
        "criteo.com",
        "criteo.net",
        "taboola.com",
        "outbrain.com",
        "connect.facebook.net",
        "bat.bing.com",
        "clarity.ms",
        "onetrust.com",
        "cookielaw.org",
        "branch.io",
        "appsflyer.com",
        "adjust.com",
        "quantserve.com",
        "scorecardresearch.com",
        "launchdarkly.com",
        "mouseflow.com",
        "analytics.tiktok.com",
        "ads.linkedin.com",
    ];
    TRACKERS.iter().any(|t| host.contains(t))
}

// ===================================================================
// Ops
// ===================================================================

/// Record an intercepted request, then return the structured response the page
/// should see: `{status:u16, headers:[[k,v]], body:"..."}`.
///
/// **Async.** Recording (and the `max_intercepts` cap) happens synchronously up
/// front — so `discover` sees every endpoint regardless of the response — then the
/// op consults the optional [`ApiFetcher`]. In Render mode a safe request is
/// fetched live and its REAL status/headers/body are returned (so a framework
/// router runs its native success/error paths — a genuine 403-JSON renders the
/// logged-out view instead of throwing on a synthetic 404). With no fetcher, or
/// when the policy declines a request (Observe mode, unsafe method, streaming,
/// analytics), it falls back to the synthetic stub so the page's fetch still
/// resolves and hydration proceeds. A live fetch bumps the in-flight counter so
/// the capture window stays open while it is outstanding.
///
/// NOTE: `async` is inferred from the `async fn` with a bare `#[op2]` (matching
/// `op_sleep`); state is taken as `Rc<RefCell<OpState>>` and every borrow is
/// dropped before the `.await`.
#[deno_core::op2]
#[serde]
async fn op_raze_fetch(
    state: Rc<RefCell<OpState>>,
    #[string] request_json: String,
) -> Result<ApiResponseWire, deno_error::JsErrorBox> {
    let raw: RawRequest = serde_json::from_str(&request_json)
        .map_err(|e| deno_error::JsErrorBox::generic(format!("op_raze_fetch bad payload: {e}")))?;

    // Record the request + enforce the cap + snapshot what we need, all under a
    // short borrow that ends before the `.await` (no `Ref` may cross it).
    let (api_fetcher, inflight, stub_body, api_req, tracker) = {
        let op_state = state.borrow();
        let cap = op_state.borrow::<Rc<RefCell<CaptureState>>>().clone();
        let mut cs = cap.borrow_mut();

        if cs.requests.len() as u32 >= cs.max_intercepts {
            return Err(deno_error::JsErrorBox::generic("max_intercepts exceeded"));
        }

        let via = match raw.via.as_str() {
            "xhr" => InterceptVia::Xhr,
            _ => InterceptVia::Fetch,
        };
        let tracker = is_tracker(&raw.url);
        let body = raw.body.map(|b| b.into_bytes());
        cs.requests.push(CapturedRequest {
            method: raw.method.clone(),
            url: raw.url.clone(),
            headers: raw.headers.clone(),
            body: body.clone(),
            via,
        });
        // A non-tracker request is content progress → advance the quiesce clock.
        // Trackers are recorded above (discovery) but deliberately do not count.
        if !tracker {
            cs.content_activity.set(cs.content_activity.get() + 1);
        }
        let api_req = ApiRequest {
            method: raw.method,
            url: raw.url,
            headers: raw.headers,
            body,
        };
        (
            cs.api_fetcher.clone(),
            cs.inflight.clone(),
            cs.stub_body.clone(),
            api_req,
            tracker,
        )
    };

    // Render mode: try a live response. `None` (Observe mode, or a request the
    // policy declined) falls through to the synthetic stub below.
    let render_mode = api_fetcher.is_some();
    let live = match api_fetcher {
        Some(f) => {
            // A non-tracker fetch holds the capture window open while it's in flight
            // (content may still be arriving); a tracker is fetched live so the page
            // behaves normally, but must not pin the window past content-settle.
            if !tracker {
                inflight.set(inflight.get() + 1);
            }
            let r = f.fetch(&api_req).await;
            if !tracker {
                inflight.set(inflight.get().saturating_sub(1));
            }
            r
        }
        None => None,
    };

    // Decode the real byte body exactly as before: invalid UTF-8 is replaced via
    // `from_utf8_lossy`. Constructing the serde wire value here lets the bounded
    // runtime log compare bytes received from the API with the Rust decoded UTF-8
    // string's byte length, without adding public report fields.
    let (resp, log_kind, raw_body_len, decoded_utf8_len) = match live {
        Some(r) => {
            let raw_body_len = r.body.len();
            let body = String::from_utf8_lossy(&r.body).into_owned();
            let decoded_utf8_len = body.len();
            (
                ApiResponseWire {
                    status: r.status,
                    headers: r.headers,
                    body,
                },
                "live",
                raw_body_len,
                decoded_utf8_len,
            )
        }
        // Synthetic stub: always 200 + the configured body + a JSON content-type
        // so `res.json()` works page-side and hydration keeps moving.
        None => {
            let body_len = stub_body.len();
            (
                ApiResponseWire {
                    status: 200,
                    headers: vec![("content-type".to_string(), "application/json".to_string())],
                    body: stub_body,
                },
                if render_mode {
                    "stub(declined/failed)"
                } else {
                    "stub(observe)"
                },
                body_len,
                body_len,
            )
        }
    };

    // Observability (surfaced under `--runtime-log`): record what the broker did
    // with THIS request — the one signal that says whether a data-driven page's
    // fetch actually fired and what came back. `live` = a real draco-net response
    // (Render mode, request allowed by policy); `stub(declined/failed)` = Render
    // mode but the policy withheld it or the live fetch failed; `stub(observe)` =
    // Observe mode's built-in synthetic stub. Logged in a short borrow that opens
    // and closes after every `.await` above.
    {
        let op_state = state.borrow();
        let cap = op_state.borrow::<Rc<RefCell<CaptureState>>>().clone();
        let mut cs = cap.borrow_mut();
        let t = cs.started.elapsed().as_millis();
        let line = format!(
            "[+{t}ms] [raze.fetch] {} {} → {} (raw={}b, decoded_utf8={}b, {log_kind})",
            api_req.method, api_req.url, resp.status, raw_body_len, decoded_utf8_len
        );
        cs.push_log(&line);
    }
    Ok(resp)
}

/// Load a dynamic script chunk on demand — in-process and asynchronously — through
/// the [`ScriptFetcher`] (pooled `draco-net` + immutable chunk cache). Awaited by
/// the glue's dynamic `<script src>` hook; because it is a real async op, many
/// chunk loads kicked off in a burst fan out concurrently on the event loop rather
/// than serializing. A miss returns `None` so the page-side loader fires `onerror`
/// like a failed network load. Bumps the in-flight counter for the duration so the
/// capture window stays open while the fetch is outstanding.
///
/// NOTE: `async` is inferred from the `async fn` with a bare `#[op2]` (matching
/// `op_sleep`); state is taken as `Rc<RefCell<OpState>>` and every borrow is
/// dropped before the `.await` (no `Ref` may be held across it).
#[deno_core::op2]
#[string]
async fn op_raze_load_script(state: Rc<RefCell<OpState>>, #[string] url: String) -> Option<String> {
    let (fetcher, inflight) = {
        let op_state = state.borrow();
        let cap = op_state.borrow::<Rc<RefCell<CaptureState>>>().clone();
        let cs = cap.borrow();
        (cs.fetcher.clone(), cs.inflight.clone())
    };
    inflight.set(inflight.get() + 1);
    let bytes = fetcher.fetch(&url).await;
    inflight.set(inflight.get().saturating_sub(1));
    // A dynamic `<script src>` chunk that couldn't be fetched fires the page-side
    // `onerror` — but from the outside that looks like a silent stall. Surface the
    // miss so `--runtime-log` shows a failed chunk that would break hydration.
    if bytes.is_none() {
        let op_state = state.borrow();
        let cap = op_state.borrow::<Rc<RefCell<CaptureState>>>().clone();
        let mut cs = cap.borrow_mut();
        let t = cs.started.elapsed().as_millis();
        let line = format!("[+{t}ms] [raze.chunk] MISS {url}");
        cs.push_log(&line);
    }
    bytes.map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Record one page-side diagnostic line (glue-swallowed exception/rejection,
/// `console.error`/`console.warn`, dynamic-chunk throw). Count- and length-bounded
/// by [`CaptureState::push_log`], so page JS cannot balloon the report.
#[deno_core::op2(fast)]
fn op_raze_log(state: &mut OpState, #[string] line: String) {
    let cap = state.borrow::<Rc<RefCell<CaptureState>>>().clone();
    cap.borrow_mut().push_log(&line);
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

/// Receive the serialized JSON of an interact `exec` turn's completion value and
/// stash it in [`CaptureState`] for [`session`](crate::session) to drain. Unused
/// by the one-shot capture path; the devtools-console return channel. The page
/// side already applies the size budget (`full`/`maxBytes`), so this stores the
/// string verbatim.
#[deno_core::op2(fast)]
fn op_raze_exec_result(state: &mut OpState, #[string] json: String) {
    let cap = state.borrow::<Rc<RefCell<CaptureState>>>().clone();
    cap.borrow_mut().exec_result = Some(json);
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
    ops = [
        op_raze_fetch,
        op_sleep,
        op_resolve_url,
        op_raze_load_script,
        op_raze_dom,
        op_raze_exec_result,
        op_raze_log,
    ],
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

/// Pre-window hooks: preserve V8's URL constructor and inject happy-dom's
/// supported internal-fetch interceptor before the page Window is constructed.
const PRELUDE_JS: &str = include_str!("../js/prelude.js");

/// Per-isolate runtime glue (runs after snapshot restore): constructs a fresh
/// happy-dom `Window` for the target URL, mirrors its DOM globals onto
/// `globalThis`, installs the `op_raze_fetch` fetch/XHR interceptor, and loads the
/// fetched HTML so the framework's mount container exists.
const GLUE_JS: &str = include_str!("../js/glue.js");

/// Release large bootstrap payloads after the glue has materialized the live DOM.
/// The page URL remains available for resolution and runtime compatibility.
const RELEASE_INPUTS_JS: &str =
    "delete globalThis.__DRACO_HTML__; delete globalThis.__DRACO_STUB__;";

/// Compatibility shims that depend on the freshly mirrored DOM globals.
const RUNTIME_COVERAGE_JS: &str = include_str!("../js/runtime_coverage.js");

// ===================================================================
// ES-module support: in-isolate module loader + script model
// ===================================================================

/// Module loader for every `import` / `import()` the isolate makes, backed by the
/// in-process async [`ScriptFetcher`] (pooled `draco-net` + immutable chunk cache).
///
/// Resolution order:
///   1. **Pending entry registry** — concurrent loader calls may clone the same
///      shared Arc while deno_core loads a parser entry graph.
///   2. **Async fetch** — dependencies are otherwise pulled via `fetcher`,
///      concurrently with sibling imports. Duplicate loader calls share one
///      in-flight future; after they finish, V8's module map owns later import
///      deduplication and the coalescer releases its source.
///   3. **Honest failure** — a module that cannot be fetched rejects the import
///      with a real *load* error.
///
/// Returning an **empty module** on a miss (the pre-v0.13.8 behavior) is a trap:
/// a chunk served as empty satisfies the load but then fails V8 *linking* with a
/// phantom `SyntaxError: … does not provide an export named 'x'`, which aborts
/// hydration far from the real cause and blames the page's own code. That single
/// `unwrap_or_default()` was the shared root cause of the stake.com and chaser.sh
/// "0 endpoints" field failures. An honest load error rejects only the specific
/// dynamic import (a browser rejects a 404'd chunk the same way); sibling scripts
/// and already-scheduled fetches still surface.
///
/// `load` returns [`ModuleLoadResponse::Async`] so module fetching is driven on
/// the event loop rather than compiled synchronously inside V8's dynamic-import
/// host callback: the whole graph is pulled concurrently and accepted into the
/// module map before evaluation, so a re-entrant `import()` resolves against an
/// already-loaded module instead of forcing a fresh recursive loader call. This
/// concurrency lets code-split SPAs load chunks in parallel instead of paying one
/// blocking IPC round trip at a time.
struct MapModuleLoader {
    /// Parser entry sources retained only while deno_core loads their graph.
    modules: Rc<RefCell<HashMap<String, SharedSource>>>,
    /// One shared network future per dependency URL, retained only while loader
    /// waiters exist for that URL.
    inflight_module_fetches: Rc<RefCell<HashMap<String, SharedModuleFetch>>>,
    /// In-process async source for `import` / `import()` chunk bytes (net + cache).
    fetcher: Rc<dyn ScriptFetcher>,
    /// Shared in-flight-load counter (see [`CaptureState::inflight`]).
    inflight: Rc<Cell<u32>>,
    /// Capture state, for surfacing module-load misses as diagnostics.
    cap: Rc<RefCell<CaptureState>>,
}

type SharedModuleFetchFuture =
    futures::future::Shared<LocalBoxFuture<'static, Option<SharedSource>>>;

struct SharedModuleFetch {
    future: SharedModuleFetchFuture,
    waiters: usize,
    retained_bytes: Rc<Cell<usize>>,
}

struct InflightLoadGuard {
    inflight: Rc<Cell<u32>>,
}

impl InflightLoadGuard {
    fn new(inflight: Rc<Cell<u32>>) -> Self {
        inflight.set(inflight.get() + 1);
        Self { inflight }
    }
}

impl Drop for InflightLoadGuard {
    fn drop(&mut self) {
        self.inflight.set(self.inflight.get().saturating_sub(1));
    }
}

struct ModuleFetchWaiterGuard {
    key: String,
    fetches: Rc<RefCell<HashMap<String, SharedModuleFetch>>>,
}

impl Drop for ModuleFetchWaiterGuard {
    fn drop(&mut self) {
        let remove = {
            let mut fetches = self.fetches.borrow_mut();
            match fetches.get_mut(&self.key) {
                Some(entry) => {
                    entry.waiters = entry.waiters.saturating_sub(1);
                    entry.waiters == 0
                }
                None => false,
            }
        };
        if remove {
            // Dropping the stored Shared future can cancel the underlying fetch.
            // Keep that drop outside the RefCell borrow to avoid re-entrancy.
            let removed = { self.fetches.borrow_mut().remove(&self.key) };
            drop(removed);
        }
    }
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
        let modules = self.modules.clone();
        let inflight_module_fetches = self.inflight_module_fetches.clone();
        let fetcher = self.fetcher.clone();
        let inflight = self.inflight.clone();
        let cap = self.cap.clone();
        let spec = module_specifier.clone();

        // Parser entries are already available and may be requested concurrently
        // while their graph is loading. Every response shares the same Arc bytes.
        if let Some(bytes) = modules.borrow().get(spec.as_str()).map(Arc::clone) {
            return ModuleLoadResponse::Async(
                async move { Ok(js_module_source(&bytes, &spec)) }.boxed_local(),
            );
        }

        // Acquire/create dependency work synchronously so concurrent load() calls
        // cannot both start the same network request before either future polls.
        let key = spec.as_str().to_string();
        let shared_fetch = {
            let mut fetches = inflight_module_fetches.borrow_mut();
            if let Some(existing) = fetches.get_mut(&key) {
                existing.waiters = existing.waiters.saturating_add(1);
                existing.future.clone()
            } else {
                let fetcher = fetcher.clone();
                let inflight = inflight.clone();
                let fetch_url = key.clone();
                let retained_bytes = Rc::new(Cell::new(0));
                let retained_bytes_for_fetch = retained_bytes.clone();
                let future = async move {
                    let _inflight_guard = InflightLoadGuard::new(inflight);
                    let fetched = fetcher.fetch(&fetch_url).await;
                    retained_bytes_for_fetch.set(
                        fetched
                            .as_ref()
                            .map(|source| source.len())
                            .unwrap_or_default(),
                    );
                    fetched
                }
                .boxed_local()
                .shared();
                fetches.insert(
                    key.clone(),
                    SharedModuleFetch {
                        future: future.clone(),
                        waiters: 1,
                        retained_bytes,
                    },
                );
                future
            }
        };
        let waiter_guard = ModuleFetchWaiterGuard {
            key: key.clone(),
            fetches: inflight_module_fetches.clone(),
        };

        ModuleLoadResponse::Async(
            async move {
                let _waiter_guard = waiter_guard;
                let fetched = shared_fetch.await;
                if let Some(bytes) = fetched {
                    return Ok(js_module_source(&bytes, &spec));
                }

                // Genuinely unavailable: reject THIS import honestly instead of
                //    poisoning the graph with a silent empty module. Surface the
                //    miss — a route/entry chunk that fails to load stalls hydration
                //    and would otherwise look like an unexplained empty render.
                {
                    let mut cs = cap.borrow_mut();
                    let t = cs.started.elapsed().as_millis();
                    let line = format!("[+{t}ms] [raze.module] MISS {key}");
                    cs.push_log(&line);
                }
                Err(JsErrorBox::generic(format!(
                    "draco: failed to load module (no on-demand fetch could \
                     retrieve it): {key}"
                )))
            }
            .boxed_local(),
        )
    }
}

/// Sum source payloads retained in a map without cloning any values.
fn retained_source_bytes<K>(sources: &HashMap<K, SharedSource>) -> usize {
    sources.values().map(|source| source.len()).sum()
}

fn log_module_fetches(
    cap: &Rc<RefCell<CaptureState>>,
    phase: &str,
    fetches: &Rc<RefCell<HashMap<String, SharedModuleFetch>>>,
    inflight: &Rc<Cell<u32>>,
) {
    let fetches = fetches.borrow();
    let retained_bytes: usize = fetches
        .values()
        .map(|fetch| fetch.retained_bytes.get())
        .sum();
    cap.borrow_mut().push_log(&format!(
        "[raze.module-fetches] phase={phase} entries={} retained_bytes={retained_bytes} inflight={}",
        fetches.len(),
        inflight.get(),
    ));
}

/// Record one V8/source-retention sample through the existing bounded capture log.
fn log_memory_phase(
    runtime: &mut JsRuntime,
    cap: &Rc<RefCell<CaptureState>>,
    phase: &str,
    module_registry_bytes: usize,
    retained_external_script_bytes: usize,
) {
    let heap = runtime.v8_isolate().get_heap_statistics();
    cap.borrow_mut().push_log(&format!(
        "[raze.memory] phase={phase} used_heap_size={} total_heap_size={} \
         total_physical_size={} external_memory={} heap_size_limit={} \
         module_registry_bytes={module_registry_bytes} \
         retained_external_script_bytes={retained_external_script_bytes}",
        heap.used_heap_size(),
        heap.total_heap_size(),
        heap.total_physical_size(),
        heap.external_memory(),
        heap.heap_size_limit(),
    ));
}

/// Build a JavaScript [`ModuleSource`] from raw chunk bytes (lossy-decoded — page
/// bytes are not guaranteed valid UTF-8 and V8 wants a `str`).
fn js_module_source(bytes: &[u8], spec: &ModuleSpecifier) -> ModuleSource {
    let source = String::from_utf8_lossy(bytes).into_owned();
    ModuleSource::new(
        ModuleType::JavaScript,
        ModuleSourceCode::String(source.into()),
        spec,
        None,
    )
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

const EXTERNAL_SCRIPT_PREFETCH_BYTES: usize = 16 * 1024 * 1024;

enum ExternalFetchState<'a> {
    Pending(LocalBoxFuture<'a, Option<SharedSource>>),
    Ready(Option<SharedSource>),
    Taken,
}

struct ExternalFetchSlot<'a> {
    script_index: usize,
    state: ExternalFetchState<'a>,
}

/// External-script fetches with a two-script lookahead window and bounded
/// completed-source retention. Only the current document-order target and its
/// immediate external successor are polled; one result may cross the soft byte
/// budget because its size is unknown until completion.
struct ExternalScriptPrefetch<'a> {
    slots: Vec<ExternalFetchSlot<'a>>,
    budget: usize,
    retained_bytes: usize,
    peak_retained_bytes: usize,
}

impl<'a> ExternalScriptPrefetch<'a> {
    fn new(
        fetcher: &'a dyn ScriptFetcher,
        external: impl IntoIterator<Item = (usize, &'a str)>,
        budget: usize,
    ) -> Self {
        Self {
            slots: external
                .into_iter()
                .map(|(script_index, url)| ExternalFetchSlot {
                    script_index,
                    state: ExternalFetchState::Pending(fetcher.fetch(url)),
                })
                .collect(),
            budget,
            retained_bytes: 0,
            peak_retained_bytes: 0,
        }
    }

    fn poll_slot(&mut self, position: usize, cx: &mut Context<'_>) -> bool {
        let completed = match &mut self.slots[position].state {
            ExternalFetchState::Pending(future) => match future.as_mut().poll(cx) {
                Poll::Ready(source) => Some(source),
                Poll::Pending => None,
            },
            ExternalFetchState::Ready(_) | ExternalFetchState::Taken => return false,
        };
        let Some(source) = completed else {
            return false;
        };
        if let Some(bytes) = &source {
            self.retained_bytes = self.retained_bytes.saturating_add(bytes.len());
            self.peak_retained_bytes = self.peak_retained_bytes.max(self.retained_bytes);
        }
        self.slots[position].state = ExternalFetchState::Ready(source);
        true
    }

    fn target_ready(&self, position: usize) -> bool {
        matches!(self.slots[position].state, ExternalFetchState::Ready(_))
    }

    fn poll_ahead(&mut self, target: usize, cx: &mut Context<'_>) -> bool {
        let mut completed = self.poll_slot(target, cx);
        if self.retained_bytes < self.budget {
            if let Some(next) = target
                .checked_add(1)
                .filter(|next| *next < self.slots.len())
            {
                completed |= self.poll_slot(next, cx);
            }
        }
        completed
    }

    async fn take(&mut self, script_index: usize) -> Option<SharedSource> {
        let position = self
            .slots
            .iter()
            .position(|slot| slot.script_index == script_index)?;
        loop {
            if self.target_ready(position) {
                // One non-blocking pass starts/harvests later fetches before this
                // source is released for execution.
                futures::future::poll_fn(|cx| {
                    self.poll_ahead(position, cx);
                    Poll::Ready(())
                })
                .await;
                let state =
                    std::mem::replace(&mut self.slots[position].state, ExternalFetchState::Taken);
                if let ExternalFetchState::Ready(source) = state {
                    if let Some(bytes) = &source {
                        self.retained_bytes = self.retained_bytes.saturating_sub(bytes.len());
                    }
                    return source;
                }
                unreachable!("target readiness checked before take");
            }

            futures::future::poll_fn(|cx| {
                let completed = self.poll_ahead(position, cx);
                if self.target_ready(position) || completed {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
        }
    }

    fn peak_retained_bytes(&self) -> usize {
        self.peak_retained_bytes
    }
}

async fn run_capture_inner(
    url: &str,
    html: &str,
    cfg: &CaptureConfig,
    fetcher: Rc<dyn ScriptFetcher>,
    api_fetcher: Option<Rc<dyn ApiFetcher>>,
) -> CaptureReport {
    let stub_body = normalize_stub_body(&cfg.stub_response_json);
    // Phase clock: bucket runtime_ms into setup / scripts / window / serialize so
    // `--runtime-log` shows where a heavy render spends its wall + V8-CPU time.
    let t0 = Instant::now();

    // One-time parser-entry handoff map. Dependency sources are fetched on demand
    // and passed straight into deno_core; V8's module map owns later deduplication.
    let modules: Rc<RefCell<HashMap<String, SharedSource>>> = Rc::new(RefCell::new(HashMap::new()));
    let inflight_module_fetches = Rc::new(RefCell::new(HashMap::new()));
    // Shared in-flight-load counter: keeps the capture window open while chunks or
    // modules are still being pulled concurrently.
    let inflight: Rc<Cell<u32>> = Rc::new(Cell::new(0));

    let cap = Rc::new(RefCell::new(CaptureState {
        requests: Vec::new(),
        max_intercepts: cfg.max_intercepts,
        stub_body: stub_body.clone(),
        fetcher: fetcher.clone(),
        api_fetcher,
        inflight: inflight.clone(),
        content_activity: Rc::new(Cell::new(0)),
        rendered_html: None,
        logs: Vec::new(),
        exec_result: None,
        started: Instant::now(),
    }));

    // Restore the DOM-engine snapshot and register the ops for this isolate.
    let mut runtime = TrackedJsRuntime::new(RuntimeOptions {
        startup_snapshot: Some(SNAPSHOT),
        create_params: Some(capture_create_params()),
        extensions: vec![draco_runtime_ext::init(cap.clone())],
        module_loader: Some(Rc::new(MapModuleLoader {
            modules: modules.clone(),
            inflight_module_fetches: inflight_module_fetches.clone(),
            fetcher: fetcher.clone(),
            inflight: inflight.clone(),
            cap: cap.clone(),
        })),
        ..Default::default()
    });
    let heap_guard = install_near_heap_limit_guard(&mut runtime);
    let execution_deadline = Duration::from_millis(cfg.capture_window_ms.max(1));
    let mut execution_watchdog = match ExecutionWatchdog::start(&mut runtime) {
        Ok(watchdog) => watchdog,
        Err(_) => {
            return finish(
                cap.clone(),
                RuntimeOutcome::Terminated,
                Some("failed to start V8 execution watchdog".to_string()),
            )
        }
    };
    macro_rules! v8_boundary {
        ($operation:expr) => {{
            let generation = match execution_watchdog.arm(execution_deadline) {
                Ok(generation) => generation,
                Err(_) => {
                    return finish(
                        cap.clone(),
                        RuntimeOutcome::Terminated,
                        Some("failed to start V8 execution watchdog".to_string()),
                    )
                }
            };
            let guarded = heap_guard.run(|| $operation);
            let timed_out = match execution_watchdog.disarm(generation) {
                Ok(timed_out) => timed_out,
                Err(_) => {
                    return finish(
                        cap.clone(),
                        RuntimeOutcome::Terminated,
                        Some("V8 execution watchdog stopped unexpectedly".to_string()),
                    )
                }
            };
            if timed_out {
                return execution_deadline_capture_report(cap.clone());
            }
            match guarded {
                Ok(value) => value,
                Err(_) => return heap_limit_capture_report(cap.clone()),
            }
        }};
    }
    log_memory_phase(&mut runtime, &cap, "snapshot", 0, 0);

    // 1. Install hooks that must precede Window construction, inject page inputs,
    //    then run the glue and its post-mirror compatibility layer.
    if let Err(e) = v8_boundary!(runtime.execute_script("draco:prelude", PRELUDE_JS)) {
        return finish(cap, RuntimeOutcome::Threw, Some(e.to_string()));
    }
    let url_lit = json_string_literal(url);
    let html_lit = json_string_literal(html);
    let stub_lit = json_string_literal(&stub_body);
    if let Err(e) = v8_boundary!(runtime.execute_script(
        "draco:inputs",
        format!(
            "globalThis.__DRACO_URL__={url_lit}; globalThis.__DRACO_HTML__={html_lit}; \
             globalThis.__DRACO_STUB__={stub_lit};"
        ),
    )) {
        return finish(cap, RuntimeOutcome::Threw, Some(e.to_string()));
    }
    if let Err(e) = v8_boundary!(runtime.execute_script("draco:glue", GLUE_JS)) {
        return finish(cap, RuntimeOutcome::Threw, Some(e.to_string()));
    }
    if let Err(e) = v8_boundary!(runtime.execute_script("draco:release-inputs", RELEASE_INPUTS_JS))
    {
        return finish(cap, RuntimeOutcome::Threw, Some(e.to_string()));
    }
    if let Err(e) =
        v8_boundary!(runtime.execute_script("draco:runtime-coverage", RUNTIME_COVERAGE_JS))
    {
        return finish(cap, RuntimeOutcome::Threw, Some(e.to_string()));
    }
    log_memory_phase(
        &mut runtime,
        &cap,
        "dom",
        retained_source_bytes(&modules.borrow()),
        0,
    );

    // 2. Evaluate the page's scripts in document order against the happy-dom
    //    document. Classic scripts (inline or fetched external) run via
    //    `execute_script`; ES modules (`type="module"`, inline or external) are
    //    loaded through the [`MapModuleLoader`] and evaluated, so `import` /
    //    `import()` resolve through V8's module map. A throw in page script
    //    is *not* fatal — later scripts and already-scheduled async work may still
    //    surface intercepts — but if it happens before anything is captured we
    //    remember it so the outcome is `Threw`.
    let setup_ms = t0.elapsed().as_millis();
    let t_scripts = Instant::now();
    let scripts = extract_scripts(html);

    // Resolve parser-inserted externals up front, then poll only the current script
    // and its immediate external successor. This preserves document-order execution
    // and useful network overlap without retaining the whole page's source set.
    let external: Vec<(usize, String)> = scripts
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.inline)
        .map(|(i, s)| (i, resolve_script_url(url, &s.payload)))
        .collect();
    let mut external_prefetch = ExternalScriptPrefetch::new(
        fetcher.as_ref(),
        external.iter().map(|(index, url)| (*index, url.as_str())),
        EXTERNAL_SCRIPT_PREFETCH_BYTES,
    );

    let mut threw_in_page = false;
    for (i, script) in scripts.into_iter().enumerate() {
        // Resolve the source + its module specifier. Inline scripts use their body
        // verbatim and a synthetic per-index URL (based on the page URL, so
        // relative imports resolve against the page). External scripts use the
        // bytes supplied by the bounded lookahead; one we couldn't fetch is skipped.
        let (source, spec_str, fetched_source) = if script.inline {
            let base = url.split('#').next().unwrap_or(url);
            (
                script.payload.clone(),
                format!("{base}#draco-inline-{i}"),
                None,
            )
        } else {
            let resolved = resolve_script_url(url, &script.payload);
            match external_prefetch.take(i).await {
                Some(bytes) => (
                    String::from_utf8_lossy(&bytes).into_owned(),
                    resolved,
                    Some(bytes),
                ),
                None => continue,
            }
        };

        // Point document.currentScript at the REAL parsed <script> node for this
        // block — matched by inline source text or external src — so a bootstrap
        // that reads `currentScript.parentElement` (SvelteKit's mount target)
        // resolves to the true parent in the document tree (the app's mount <div>)
        // instead of a synthetic node grafted onto <head>, which silently
        // misdirects the mount. Best-effort; the glue falls back to an inert node.
        let set_cs = if script.inline {
            format!(
                "try {{ globalThis.__dracoSetCurrentScript({}, null); }} catch (_) {{}}",
                json_string_literal(&source)
            )
        } else {
            format!(
                "try {{ globalThis.__dracoSetCurrentScript(null, {}); }} catch (_) {{}}",
                json_string_literal(&spec_str)
            )
        };
        let _ = v8_boundary!(runtime.execute_script("draco:currentScript", set_cs));

        if script.module {
            // ES module: register the entry source under its specifier (so its own
            // relative imports resolve) and evaluate it, driving the event loop to
            // completion.
            match deno_core::url::Url::parse(&spec_str) {
                Ok(spec_url) => {
                    modules.borrow_mut().insert(
                        spec_url.as_str().to_string(),
                        fetched_source.unwrap_or_else(|| source.into_bytes().into()),
                    );
                    let eval = match eval_module(
                        &mut runtime,
                        &spec_url,
                        &modules,
                        &heap_guard,
                        &mut execution_watchdog,
                        execution_deadline,
                    )
                    .await
                    {
                        Ok(eval) => eval,
                        Err(ExecutionBoundaryError::HeapLimit) => {
                            return heap_limit_capture_report(cap.clone())
                        }
                        Err(ExecutionBoundaryError::Deadline) => {
                            return execution_deadline_capture_report(cap.clone())
                        }
                        Err(ExecutionBoundaryError::WatchdogUnavailable) => {
                            return finish(
                                cap.clone(),
                                RuntimeOutcome::Terminated,
                                Some("V8 execution watchdog stopped unexpectedly".to_string()),
                            )
                        }
                    };
                    if let Err(e) = eval {
                        threw_in_page = true;
                        let line = format!("module script {i} threw: {e}");
                        eprintln!("draco-runtime: {line}");
                        cap.borrow_mut().push_log(&line);
                    }
                }
                Err(e) => {
                    let line = format!("bad module specifier for script {i}: {e}");
                    eprintln!("draco-runtime: {line}");
                    cap.borrow_mut().push_log(&line);
                }
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
            if let Err(e) = v8_boundary!(runtime.execute_script(name, source)) {
                threw_in_page = true;
                let line = format!("page script {i} threw: {e}");
                eprintln!("draco-runtime: {line}");
                cap.borrow_mut().push_log(&line);
            }
        }
    }
    let _ = v8_boundary!(runtime.execute_script(
        "draco:currentScript:clear",
        "try { globalThis.__dracoClearCurrentScript(); } catch (_) {}",
    ));

    // Preserve the existing phase record while reporting the actual high-water
    // mark of completed, not-yet-executed external sources.
    log_memory_phase(
        &mut runtime,
        &cap,
        "scripts-fetched",
        retained_source_bytes(&modules.borrow()),
        external_prefetch.peak_retained_bytes(),
    );

    // Parsing-finished moment: the document-order scripts have all evaluated.
    // Fire the document lifecycle (readyState transitions, DOMContentLoaded,
    // window load) so boot code gated on those signals proceeds — a real browser
    // fires both events here, BEFORE dynamic import()s settle, and late-running
    // chunk code then observes readyState === "complete" (see glue §7).
    let _ = v8_boundary!(runtime.execute_script(
        "draco:lifecycle",
        "try { globalThis.__dracoFireLifecycle(); } catch (_) {}",
    ));
    log_memory_phase(
        &mut runtime,
        &cap,
        "scripts-run",
        retained_source_bytes(&modules.borrow()),
        0,
    );

    let scripts_ms = t_scripts.elapsed().as_millis();

    // 3. Capture window: pump the event loop until quiescence or the hard cap.
    let t_window = Instant::now();
    let (outcome, window_cpu) = drive_capture_window(
        &mut runtime,
        &cap,
        cfg,
        threw_in_page,
        &heap_guard,
        &mut execution_watchdog,
    )
    .await;
    if outcome == RuntimeOutcome::Terminated {
        return finish(cap, RuntimeOutcome::Terminated, None);
    }
    let window_ms = t_window.elapsed().as_millis();
    let window_cpu_ms = window_cpu.as_millis();
    log_memory_phase(
        &mut runtime,
        &cap,
        "settled",
        retained_source_bytes(&modules.borrow()),
        0,
    );
    log_module_fetches(&cap, "settled", &inflight_module_fetches, &inflight);

    // 4. Serialize the hydrated DOM for the render-then-Markdown escalation, after
    //    the window so any content the framework mounted is present.
    let t_serialize = Instant::now();
    v8_boundary!(serialize_dom(&mut runtime));
    let serialize_ms = t_serialize.elapsed().as_millis();
    log_memory_phase(
        &mut runtime,
        &cap,
        "serialized",
        retained_source_bytes(&modules.borrow()),
        0,
    );

    // Phase breakdown (surfaced under `--runtime-log`): the raw material for the
    // render-tier CPU question — how much of runtime_ms is initial script eval vs
    // the capture window, and within the window how much is V8 executing (poll)
    // vs idle-waiting on the network.
    {
        let total_ms = t0.elapsed().as_millis();
        let idle_ms = window_ms.saturating_sub(window_cpu_ms);
        cap.borrow_mut().push_log(&format!(
            "[raze.phases] runtime {total_ms}ms = setup {setup_ms} + scripts {scripts_ms} \
             + window {window_ms} (v8-cpu {window_cpu_ms} / idle {idle_ms}) + serialize {serialize_ms}"
        ));
    }

    let (requests, rendered_html, logs) = {
        let mut cs = cap.borrow_mut();
        cs.take_output_buffers()
    };
    CaptureReport {
        outcome,
        requests,
        rendered_html,
        logs,
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
    modules: &Rc<RefCell<HashMap<String, SharedSource>>>,
    heap_guard: &HeapLimitGuard,
    execution_watchdog: &mut ExecutionWatchdog,
    execution_deadline: Duration,
) -> Result<Result<(), deno_core::error::CoreError>, ExecutionBoundaryError> {
    heap_guard
        .check()
        .map_err(|_| ExecutionBoundaryError::HeapLimit)?;
    let loaded = runtime.load_side_es_module(spec).await;
    // Concurrent load calls may clone the shared Arc while the graph is in
    // flight. Success or failure ends that window, so release the raw entry now.
    modules.borrow_mut().remove(spec.as_str());
    heap_guard
        .check()
        .map_err(|_| ExecutionBoundaryError::HeapLimit)?;
    let id = match loaded {
        Ok(id) => id,
        Err(error) => return Ok(Err(error)),
    };
    let eval = run_with_execution_deadline(
        runtime,
        heap_guard,
        execution_watchdog,
        execution_deadline,
        |runtime| runtime.mod_evaluate(id),
    )?;
    let generation = execution_watchdog.arm(execution_deadline)?;
    let event_loop = runtime
        .run_event_loop(PollEventLoopOptions::default())
        .await;
    if execution_watchdog.disarm(generation)? {
        return Err(ExecutionBoundaryError::Deadline);
    }
    heap_guard
        .check()
        .map_err(|_| ExecutionBoundaryError::HeapLimit)?;
    if let Err(error) = event_loop {
        return Ok(Err(error));
    }
    let evaluated = eval.await;
    heap_guard
        .check()
        .map_err(|_| ExecutionBoundaryError::HeapLimit)?;
    Ok(evaluated)
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
    heap_guard: &HeapLimitGuard,
    execution_watchdog: &mut ExecutionWatchdog,
) -> (RuntimeOutcome, Duration) {
    let start = Instant::now();
    let hard_cap = Duration::from_millis(cfg.capture_window_ms);
    let quiesce = Duration::from_millis(cfg.quiesce_ms);
    // Accumulated wall-time spent inside `poll_once` = time V8 was actually
    // executing (a long synchronous JS task — hydration, a 2.5 MB JSON parse —
    // blocks the poll for its whole duration). Its complement is idle wait between
    // ticks. This is the CPU-bound-vs-wait-bound signal for the render tier.
    let mut poll_busy = Duration::ZERO;

    // Small tick so we re-check the wall clock even while timers are pending.
    let tick = Duration::from_millis(quiesce_tick_ms(cfg.quiesce_ms));

    // Content-activity count at which we last saw progress, to measure the quiesce
    // streak. Uses the non-tracker request counter (see `CaptureState`), so tracker
    // beacons firing after the content settled do not reset the streak and cannot
    // pin the window open.
    let content_activity = cap.borrow().content_activity.clone();
    let mut last_count = content_activity.get();
    let mut last_activity = Instant::now();
    let mut loop_threw = false;
    // Shared in-flight-load counter: while the page is still pulling script/module
    // chunks concurrently, the window must not quiesce (the async analogue of the
    // old blocking loader keeping the single thread busy).
    let inflight = cap.borrow().inflight.clone();
    // Why the window ended (surfaced as a `[raze.window]` diagnostic): "drained"
    // (event loop empty — no pending ops/timers/promises), "quiesce" (no new
    // activity for quiesce_ms), "hard-cap" (hit capture_window_ms), or
    // "loop-error". Distinguishes a timing stall (window closed before a late
    // async fetch fired) from a genuine run to the cap.
    let mut close_reason: &'static str = "quiesce";

    loop {
        // Poll one tick of the event loop with a self-contained waker.
        let t_poll = Instant::now();
        let poll_res = match run_with_execution_deadline(
            runtime,
            heap_guard,
            execution_watchdog,
            Duration::from_millis(cfg.capture_window_ms.max(1)),
            poll_once,
        ) {
            Ok(poll) => poll,
            Err(ExecutionBoundaryError::HeapLimit) => {
                cap.borrow_mut()
                    .push_log(&format!("[raze.heap] {HEAP_LIMIT_DIAGNOSTIC}"));
                return (RuntimeOutcome::Terminated, poll_busy);
            }
            Err(ExecutionBoundaryError::Deadline) => {
                cap.borrow_mut()
                    .push_log(&format!("[raze.watchdog] {EXECUTION_DEADLINE_DIAGNOSTIC}"));
                return (RuntimeOutcome::Terminated, poll_busy);
            }
            Err(ExecutionBoundaryError::WatchdogUnavailable) => {
                cap.borrow_mut()
                    .push_log("[raze.watchdog] V8 execution watchdog stopped unexpectedly");
                return (RuntimeOutcome::Terminated, poll_busy);
            }
        };
        poll_busy += t_poll.elapsed();

        match poll_res {
            Poll::Ready(Ok(())) => {
                // Event loop fully drained: no pending ops/timers/promises.
                // This is a clean, natural quiesce.
                close_reason = "drained";
                break;
            }
            Poll::Ready(Err(e)) => {
                // A top-level / unhandled error propagated out of the loop.
                let line = format!("event loop error: {e}");
                eprintln!("draco-runtime: {line}");
                cap.borrow_mut().push_log(&line);
                loop_threw = true;
                close_reason = "loop-error";
                break;
            }
            Poll::Pending => {
                // Still work pending (typically a live op_sleep timer). Fall
                // through to the time-based checks, then sleep a tick so timers
                // can elapse and we can re-evaluate.
            }
        }

        // Refresh activity tracking (content requests only; trackers excluded).
        let now_count = content_activity.get();
        if now_count != last_count {
            last_count = now_count;
            last_activity = Instant::now();
        }
        // In-flight script/module loads count as activity: keep the window open
        // (and reset the quiesce streak) so a page can't be truncated mid-load, and
        // so hydration triggered by a just-arrived chunk still gets its quiesce_ms.
        if inflight.get() > 0 {
            last_activity = Instant::now();
        }

        // Hard cap first.
        if start.elapsed() >= hard_cap {
            close_reason = "hard-cap";
            break;
        }

        // Quiesce: only start counting the streak once *something* has been
        // captured OR once the page has had a moment to schedule work. We treat
        // "no pending progress for `quiesce_ms`" as quiesced. Because the event
        // loop is Pending here (a timer is live), we still allow quiesce to fire
        // when the only remaining work is idle repeating timers that produce no
        // new intercepts — otherwise a `setInterval` would pin the window open
        // to the hard cap.
        if last_activity.elapsed() >= quiesce {
            // Quiesce is the default exit — leave `close_reason` at its initial
            // "quiesce" (reassigning would make that initializer dead code).
            break;
        }

        tokio::time::sleep(tick).await;
    }

    // Summarize the window close (surfaced under `--runtime-log`): the reason, the
    // wall-clock spent, how many fetch/XHR requests were captured, and whether any
    // script/module load was still outstanding when we stopped. A "drained"/
    // "quiesce" close with 0 requests and a late-firing page is the fingerprint of
    // an event-loop stall before the data fetch dispatched.
    let elapsed_ms = start.elapsed().as_millis();
    {
        let mut cs = cap.borrow_mut();
        let n = cs.requests.len();
        let infl = inflight.get();
        let t = cs.started.elapsed().as_millis();
        let cpu_ms = poll_busy.as_millis();
        let line = format!(
            "[+{t}ms] [raze.window] closed via {close_reason} (window {elapsed_ms}ms, \
             v8-cpu {cpu_ms}ms); {n} request(s) captured, {infl} load(s) inflight"
        );
        cs.push_log(&line);
    }

    let outcome = classify_window_close(cap, threw_in_page, loop_threw, close_reason == "hard-cap");
    (outcome, poll_busy)
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
        cap.borrow_mut().push_log(&format!("boot: {d}"));
    }
    let (requests, _rendered_html, logs) = {
        let mut cs = cap.borrow_mut();
        cs.take_output_buffers()
    };
    // `finish` is only reached on a pre-hydration boot failure (URL inject /
    // polyfill / interceptor threw), so there is no meaningful hydrated DOM to
    // serialize.
    CaptureReport {
        outcome,
        requests,
        rendered_html: None,
        logs,
    }
}

fn heap_limit_capture_report(cap: Rc<RefCell<CaptureState>>) -> CaptureReport {
    cap.borrow_mut()
        .push_log(&format!("[raze.heap] {HEAP_LIMIT_DIAGNOSTIC}"));
    finish(cap, RuntimeOutcome::Terminated, None)
}

fn execution_deadline_capture_report(cap: Rc<RefCell<CaptureState>>) -> CaptureReport {
    cap.borrow_mut()
        .push_log(&format!("[raze.watchdog] {EXECUTION_DEADLINE_DIAGNOSTIC}"));
    finish(cap, RuntimeOutcome::Terminated, None)
}

// ===================================================================
// V8 flags
// ===================================================================

static V8_FLAGS: Once = Once::new();

/// Set V8 flags once, before any isolate is created. Best-effort: flags V8 does
/// not understand are reported and ignored (we do not abort).
fn ensure_v8_flags() {
    V8_FLAGS.call_once(|| {
        // JIT is ON (see module docs): SPA hydration is hot JS and `--jitless`
        // ran it 3–10× slower — slow enough to blow the capture/on-demand
        // budgets and fail to extract content. Containment is the infra layer +
        // an isolate with no host bindings, not W^X. `--single-threaded` keeps
        // V8 from spawning background compiler/GC threads (JIT runs on the main
        // thread), keeping the jailed child's syscall surface small. argv[0] is
        // ignored by V8.
        let flags = vec!["draco".to_string(), "--single-threaded".to_string()];
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

/// Pick a polling tick for the capture loop's `Pending` sleep.
///
/// This bounds how long after an async op completes (a chunk/module `import()` or a
/// data fetch landing) we take to poll again and run the JS it unblocks. With the
/// no-op waker we drive timing ourselves, so this tick *is* the op-completion
/// latency — and phase timing shows the render window is ~60% idle (network-bound),
/// not CPU-bound, with that latency compounding across a page's sequential fetch
/// chain. The old `quiesce_ms/4` gave a 50 ms tick for the render window (500 ms
/// quiesce) — up to 50 ms of dead air after every fetch. A small fixed-ceiling tick
/// reclaims most of it: idle polls are cheap (`poll_event_loop` returns fast when
/// nothing is ready), so this is not a busy-spin, and the `>= 3 ms` floor keeps a
/// pathological page from pegging a core. Quiesce is still honored to within a tick.
fn quiesce_tick_ms(quiesce_ms: u64) -> u64 {
    (quiesce_ms / 50).clamp(3, 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_shot_isolate_lifecycle_is_balanced() {
        let test_binary = std::env::current_exe().expect("current test binary path");
        let output = std::process::Command::new(test_binary)
            .args([
                "--ignored",
                "--exact",
                "tests::one_shot_isolate_lifecycle_subprocess",
                "--nocapture",
            ])
            .output()
            .expect("run isolated lifecycle test");
        assert!(
            output.status.success(),
            "isolated lifecycle test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    #[ignore]
    fn one_shot_isolate_lifecycle_subprocess() {
        let before = isolate_stats();
        let _ = run_capture(
            "https://lifecycle.example/",
            "<main>stable</main>",
            &CaptureConfig {
                capture_window_ms: 20,
                quiesce_ms: 3,
                max_intercepts: 1,
                stub_response_json: "{}".to_string(),
            },
            null_fetcher(),
        );
        let after = isolate_stats();

        assert_eq!(after.created, before.created + 1);
        assert_eq!(after.dropped, before.dropped + 1);
        assert_eq!(after.active, before.active);
        assert_eq!(after.active, after.created.saturating_sub(after.dropped));
    }

    #[test]
    fn shared_source_clones_share_the_payload_allocation() {
        let source: SharedSource = Arc::from(&b"console.log('shared')"[..]);
        let cloned = Arc::clone(&source);

        assert!(Arc::ptr_eq(&source, &cloned));
    }

    #[test]
    fn capture_heap_policy_caps_v8_growth() {
        ensure_v8_flags();
        let mut runtime = JsRuntime::new(RuntimeOptions {
            create_params: Some(capture_create_params()),
            ..Default::default()
        });
        let effective_limit = runtime.v8_isolate().get_heap_statistics().heap_size_limit();
        assert!(
            effective_limit <= CAPTURE_MAX_HEAP_BYTES + 8 * MIB,
            "V8 effective heap limit {effective_limit} exceeded the configured cap plus overhead"
        );
    }

    #[test]
    fn capture_default_heap_limit_is_192_mib() {
        assert_eq!(CAPTURE_MAX_HEAP_BYTES, 192 * 1024 * 1024);
    }

    #[test]
    fn near_heap_limit_guard_terminates_execution_before_abort() {
        let test_binary = std::env::current_exe().expect("current test binary path");
        let output = std::process::Command::new(test_binary)
            .args([
                "--ignored",
                "--exact",
                "tests::near_heap_limit_guard_subprocess",
                "--nocapture",
            ])
            .output()
            .expect("run isolated heap-limit test");
        assert!(
            output.status.success(),
            "isolated heap-limit test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    /// Deliberately exhausting a tiny V8 heap is process-global stress that can
    /// destabilize other isolates running concurrently in Rust's unit-test process.
    /// The public test above runs this exact ignored test in a dedicated subprocess.
    #[test]
    #[ignore]
    fn near_heap_limit_guard_subprocess() {
        ensure_v8_flags();
        let mut runtime = JsRuntime::new(RuntimeOptions {
            create_params: Some(
                deno_core::v8::CreateParams::default().heap_limits(0, 5 * 1024 * 1024),
            ),
            ..Default::default()
        });
        let guard = install_near_heap_limit_guard(&mut runtime);

        let err = runtime
            .execute_script(
                "draco:test:heap-limit",
                r#"let s = ""; while (true) { s += "Hello"; }"#,
            )
            .expect_err("allocation loop must be terminated at the heap limit");

        assert_eq!(
            err.exception_message,
            "Uncaught Error: execution terminated"
        );
        assert!(guard.is_tripped(), "near-heap callback was not invoked");

        let second_script_ran = Rc::new(Cell::new(false));
        let second_script_ran_inner = second_script_ran.clone();
        let second = guard.run(|| {
            second_script_ran_inner.set(true);
            runtime.execute_script("draco:test:must-not-run", "globalThis.afterOom = true")
        });
        assert!(
            second.is_err(),
            "poisoned isolate must reject another boundary"
        );
        assert!(
            !second_script_ran.get(),
            "second script ran after heap limit"
        );
        drop(runtime);
    }

    #[test]
    fn phase_memory_logs_are_ordered() {
        let script_url = "https://memory.example.com/app.js";
        let report = run_capture(
            "https://memory.example.com/",
            r#"<html><body><script src="/app.js"></script></body></html>"#,
            &CaptureConfig {
                capture_window_ms: 100,
                quiesce_ms: 10,
                max_intercepts: 8,
                stub_response_json: "{}".to_string(),
            },
            map_fetcher(HashMap::from([(
                script_url.to_string(),
                b"globalThis.__memory_test_loaded = true;".to_vec(),
            )])),
        );

        let memory_lines: Vec<&str> = report
            .logs
            .iter()
            .map(String::as_str)
            .filter(|line| line.starts_with("[raze.memory] "))
            .collect();
        assert_eq!(
            memory_lines.len(),
            6,
            "expected exactly six memory lines: {memory_lines:?}"
        );

        let numeric_keys = [
            "used_heap_size",
            "total_heap_size",
            "total_physical_size",
            "external_memory",
            "heap_size_limit",
            "module_registry_bytes",
            "retained_external_script_bytes",
        ];
        let phases: Vec<&str> = memory_lines
            .iter()
            .map(|line| {
                let fields: Vec<&str> = line.split_whitespace().collect();
                assert_eq!(
                    fields.len(),
                    2 + numeric_keys.len(),
                    "unexpected field count in {line}"
                );
                assert_eq!(fields[0], "[raze.memory]", "unexpected prefix in {line}");
                let phase = fields[1]
                    .strip_prefix("phase=")
                    .filter(|phase| !phase.is_empty())
                    .unwrap_or_else(|| panic!("invalid phase field in {line}"));

                for (field, expected_key) in fields[2..].iter().zip(numeric_keys) {
                    let (key, value) = field
                        .split_once('=')
                        .unwrap_or_else(|| panic!("invalid key/value field {field} in {line}"));
                    assert_eq!(key, expected_key, "unexpected field in {line}");
                    value.parse::<usize>().unwrap_or_else(|_| {
                        panic!("nonnumeric value for {expected_key} in {line}")
                    });
                }
                phase
            })
            .collect();

        assert_eq!(
            phases,
            [
                "snapshot",
                "dom",
                "scripts-fetched",
                "scripts-run",
                "settled",
                "serialized",
            ]
        );
    }

    /// Test [`ScriptFetcher`] backed by a fixed `{ url -> bytes }` map, returning a
    /// ready future per lookup — the offline stand-in for the net+cache fetcher.
    struct MapFetcher(HashMap<String, SharedSource>);
    impl ScriptFetcher for MapFetcher {
        fn fetch<'a>(&'a self, url: &'a str) -> LocalBoxFuture<'a, Option<SharedSource>> {
            let hit = self.0.get(url).map(Arc::clone);
            Box::pin(async move { hit })
        }
    }
    /// A fetcher that resolves nothing (pages with no external code).
    fn null_fetcher() -> Rc<dyn ScriptFetcher> {
        Rc::new(MapFetcher(HashMap::new()))
    }
    /// A fetcher serving a fixed `{ url -> bytes }` set.
    fn map_fetcher(entries: HashMap<String, Vec<u8>>) -> Rc<dyn ScriptFetcher> {
        Rc::new(MapFetcher(
            entries
                .into_iter()
                .map(|(url, bytes)| (url, bytes.into()))
                .collect(),
        ))
    }

    /// Test [`ApiFetcher`] serving a fixed `{ url -> (status, json_body) }` map as
    /// real responses (Render mode) — the offline stand-in for the net-backed one.
    struct MapApiFetcher(HashMap<String, (u16, String)>);
    impl ApiFetcher for MapApiFetcher {
        fn fetch<'a>(&'a self, req: &'a ApiRequest) -> LocalBoxFuture<'a, Option<ApiResponse>> {
            let hit = self.0.get(&req.url).map(|(status, body)| ApiResponse {
                status: *status,
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: body.clone().into_bytes(),
            });
            Box::pin(async move { hit })
        }
    }
    fn api_fetcher(entries: HashMap<String, (u16, String)>) -> Rc<dyn ApiFetcher> {
        Rc::new(MapApiFetcher(entries))
    }

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
    fn appended_script_chunk_runs_via_fetcher() {
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
        let report = run_capture(
            "https://example.com/",
            html,
            &CaptureConfig {
                capture_window_ms: 500,
                quiesce_ms: 20,
                max_intercepts: 8,
                stub_response_json: "{}".to_string(),
            },
            map_fetcher(resources),
        );
        assert_eq!(report.requests.len(), 1, "{report:?}");
        assert_eq!(report.requests[0].url, "https://example.com/api/from-chunk");
    }

    #[test]
    fn window_self_globalthis_are_the_same_object() {
        // Browser top-level contract: window === self === globalThis. happy-dom
        // instantiates its Window as a separate object, so the glue must unify
        // the aliases. This is the exact shape of the Next.js hydration crash on
        // bluff.com: write a global via `window`, read it back via `self` — if
        // the aliases diverge the read is `undefined` and `.gssp` throws,
        // aborting hydration before any fetch. A captured fetch here proves the
        // write is visible through the other alias.
        let html = r#"<script>
            window.__NEXT_DATA__ = { gssp: true, props: {} };
            if (self.__NEXT_DATA__.gssp && globalThis.__NEXT_DATA__ === window.__NEXT_DATA__) {
                fetch("/api/hydrated");
            }
        </script>"#;
        let report = run_capture(
            "https://example.com/",
            html,
            &CaptureConfig {
                capture_window_ms: 300,
                quiesce_ms: 20,
                max_intercepts: 8,
                stub_response_json: "{}".to_string(),
            },
            null_fetcher(),
        );
        assert!(
            report
                .requests
                .iter()
                .any(|r| r.url == "https://example.com/api/hydrated"),
            "window/self/globalThis must alias the same object (Next.js gssp \
             crash regression); logs={:?}, requests={:?}",
            report.logs,
            report.requests.iter().map(|r| &r.url).collect::<Vec<_>>()
        );
        assert!(
            !report.logs.iter().any(|l| l.contains("gssp")),
            "no gssp read error expected, got logs={:?}",
            report.logs
        );
    }

    #[test]
    fn page_diagnostics_land_in_report_logs() {
        // console.error goes through the glue's console hook (op_raze_log);
        // the sync throw is caught by the Rust script driver. Both must land
        // in `report.logs` — the raw material for `runtime.log` trace steps.
        let html = r#"<script>
            console.error("hydration", "failed:", { code: 42 });
            throw new Error("boot exploded");
        </script>"#;
        let report = run_capture(
            "https://example.com/",
            html,
            &CaptureConfig {
                capture_window_ms: 300,
                quiesce_ms: 20,
                max_intercepts: 8,
                stub_response_json: "{}".to_string(),
            },
            null_fetcher(),
        );
        assert!(
            report.logs.iter().any(|l| l.starts_with("[console.error]")
                && l.contains("hydration failed:")
                && l.contains("42")),
            "console.error line missing: {:?}",
            report.logs
        );
        assert!(
            report
                .logs
                .iter()
                .any(|l| l.contains("threw") && l.contains("boot exploded")),
            "page-script throw missing: {:?}",
            report.logs
        );
    }

    #[test]
    fn performance_api_shim_keeps_web_vitals_chunks_running() {
        let html = r#"<script>
            const s = document.createElement("script");
            s.src = "/_next/static/chunks/web-vitals.js";
            document.head.appendChild(s);
        </script>"#;
        let mut resources = HashMap::new();
        resources.insert(
            "https://example.com/_next/static/chunks/web-vitals.js".to_string(),
            br#"
                globalThis.performance.getEntriesByType("layout-shift");
                globalThis.performance.getEntries();
                globalThis.performance.mark("draco-test");
                fetch("/api/after-performance");
            "#
            .to_vec(),
        );
        let report = run_capture(
            "https://example.com/",
            html,
            &CaptureConfig {
                capture_window_ms: 500,
                quiesce_ms: 20,
                max_intercepts: 8,
                stub_response_json: "{}".to_string(),
            },
            map_fetcher(resources),
        );
        assert_eq!(report.requests.len(), 1, "{report:?}");
        assert_eq!(
            report.requests[0].url,
            "https://example.com/api/after-performance"
        );
    }

    #[test]
    fn appended_script_chunk_miss_is_survivable() {
        // A chunk the fetcher cannot supply must fail like a 404'd <script> — the
        // async load rejects, hydration continues, and the capture is not crashed.
        // The inline fetch queued right after the (doomed) append still surfaces.
        let html = r#"<script>
            const s = document.createElement("script");
            s.src = "/_next/static/chunks/missing.js";
            document.head.appendChild(s);
            fetch("/api/still-runs");
        </script>"#;
        let report = run_capture(
            "https://example.com/",
            html,
            &CaptureConfig {
                capture_window_ms: 300,
                quiesce_ms: 20,
                max_intercepts: 8,
                stub_response_json: "{}".to_string(),
            },
            null_fetcher(),
        );
        assert!(
            report
                .requests
                .iter()
                .any(|r| r.url == "https://example.com/api/still-runs"),
            "inline fetch after a chunk miss must still be captured: {report:?}"
        );
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
        let report = run_capture(
            "https://example.com/app/",
            html,
            &CaptureConfig {
                capture_window_ms: 500,
                quiesce_ms: 20,
                max_intercepts: 8,
                stub_response_json: "{}".to_string(),
            },
            map_fetcher(resources),
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
        // Small, fixed-ceiling tick so op-completion latency stays low (the render
        // window is network-bound, ~60% idle). Floored at 3 ms, ceilinged at 10 ms.
        assert_eq!(quiesce_tick_ms(0), 3); // floor
        assert_eq!(quiesce_tick_ms(300), 6); // default quiesce
        assert_eq!(quiesce_tick_ms(500), 10); // render quiesce (was 50 ms)
        assert_eq!(quiesce_tick_ms(10_000), 10); // ceiling
    }

    #[test]
    fn current_script_parent_is_real_mount_node() {
        // A SvelteKit-style client bootstrap locates its mount target via
        // `document.currentScript.parentElement`. The inline <script> is parsed
        // inside <div id="app">, so currentScript.parentElement must resolve to
        // that div — id "app" — not <head> (the old synthetic-node-in-head bug,
        // which mounted the app into the wrong node) and not null. We prove it by
        // recording a fetch whose query encodes the resolved parent's id.
        let html = r#"<html><head></head><body>
            <div id="app"><script>
                var el = document.currentScript && document.currentScript.parentElement;
                fetch("/mounted?parent=" + (el ? el.id : "NONE"));
            </script></div>
        </body></html>"#;
        let report = run_capture(
            "https://sk.example.com/",
            html,
            &CaptureConfig {
                capture_window_ms: 500,
                quiesce_ms: 20,
                max_intercepts: 8,
                stub_response_json: "{}".to_string(),
            },
            null_fetcher(),
        );
        assert!(
            report
                .requests
                .iter()
                .any(|r| r.url == "https://sk.example.com/mounted?parent=app"),
            "currentScript.parentElement must be the real mount <div id=app>; got {:?}",
            report.requests.iter().map(|r| &r.url).collect::<Vec<_>>()
        );
    }

    #[test]
    fn render_mode_feeds_live_data_into_the_dom() {
        // A pure-CSR page: an inline script fetches data and writes it into the
        // mounted DOM. In RENDER mode the ApiFetcher answers with real JSON, so the
        // serialized DOM carries the fetched content. (In Observe mode the fetch is
        // stubbed with `[]` and the DOM stays empty — that is the thrill.com class
        // of failure this whole path fixes.)
        let html = r#"<html><body><div id="app"></div><script>
            fetch("/api/title")
                .then(function (r) { return r.json(); })
                .then(function (d) { document.getElementById("app").textContent = d.title; });
        </script></body></html>"#;
        let mut api = HashMap::new();
        api.insert(
            "https://csr.example.com/api/title".to_string(),
            (200u16, r#"{"title":"LIVE DATA"}"#.to_string()),
        );
        let report = run_capture_render(
            "https://csr.example.com/",
            html,
            &CaptureConfig {
                capture_window_ms: 500,
                quiesce_ms: 20,
                max_intercepts: 8,
                stub_response_json: "[]".to_string(),
            },
            null_fetcher(),
            api_fetcher(api),
        );
        let dom = report.rendered_html.unwrap_or_default();
        assert!(
            dom.contains("LIVE DATA"),
            "Render mode must feed the live fetch response into the DOM; dom={dom:?}, logs={:?}",
            report.logs
        );
        // The data request was still recorded (discover works in Render mode too).
        assert!(
            report
                .requests
                .iter()
                .any(|r| r.url == "https://csr.example.com/api/title"),
            "the live data request must still be recorded: {:?}",
            report.requests.iter().map(|r| &r.url).collect::<Vec<_>>()
        );
    }

    #[test]
    fn web_animations_shim_lets_transition_code_complete() {
        // Svelte 5 transitions call element.animate() inside the effect flush and
        // continue only when the animation finishes; the pre-shim TypeError
        // aborted the mount mid-tree (thrill.com rendered only its footer). The
        // shim must (a) exist, (b) fire onfinish (completion-biased), and
        // (c) answer getAnimations() — proven by fetches gated on each.
        let html = r#"<html><body><div id="app"></div><script>
            var el = document.getElementById("app");
            if (typeof el.getAnimations === "function" && el.getAnimations().length === 0) {
                fetch("/api/get-animations-ok");
            }
            var a = el.animate([{ opacity: 0 }, { opacity: 1 }], { duration: 200 });
            a.onfinish = function () { fetch("/api/after-animate"); };
        </script></body></html>"#;
        let report = run_capture(
            "https://anim.example.com/",
            html,
            &CaptureConfig {
                capture_window_ms: 800,
                quiesce_ms: 100,
                max_intercepts: 8,
                stub_response_json: "{}".to_string(),
            },
            null_fetcher(),
        );
        let urls: Vec<&str> = report.requests.iter().map(|r| r.url.as_str()).collect();
        assert!(
            urls.contains(&"https://anim.example.com/api/get-animations-ok"),
            "getAnimations() must exist and return []: {urls:?}, logs={:?}",
            report.logs
        );
        assert!(
            urls.contains(&"https://anim.example.com/api/after-animate"),
            "animate().onfinish must fire (completion-biased shim): {urls:?}, logs={:?}",
            report.logs
        );
    }

    #[test]
    fn injected_module_script_runs_via_import_not_happydom() {
        // A dynamically inserted <script type="module" src=…> (e.g. thrill.com's
        // game-loader) must be loaded + evaluated as an ES module through our
        // MapModuleLoader — not fall through to happy-dom's disabled module loader
        // (a NotSupportedError) or be classic-eval'd (breaks on import/export). We
        // prove the module ran by capturing a fetch its top-level body makes.
        let html = r#"<html><body><script>
            var s = document.createElement("script");
            s.type = "module";
            s.src = "/mod/game-loader.js";
            document.head.appendChild(s);
        </script></body></html>"#;
        let mut resources = HashMap::new();
        resources.insert(
            "https://ex.example.com/mod/game-loader.js".to_string(),
            // Module-only syntax (export) proves it's evaluated as a module, not eval'd.
            br#"export const loaded = true; fetch("/from-module");"#.to_vec(),
        );
        let report = run_capture(
            "https://ex.example.com/",
            html,
            &CaptureConfig {
                capture_window_ms: 500,
                quiesce_ms: 20,
                max_intercepts: 8,
                stub_response_json: "{}".to_string(),
            },
            map_fetcher(resources),
        );
        assert!(
            report
                .requests
                .iter()
                .any(|r| r.url == "https://ex.example.com/from-module"),
            "injected module script must load+evaluate via import(); got {:?}, logs={:?}",
            report.requests.iter().map(|r| &r.url).collect::<Vec<_>>(),
            report.logs
        );
    }

    #[test]
    fn intersection_observer_fires_so_lazy_content_loads() {
        // Modern SPAs lazy-load a section's data when it scrolls into view via
        // IntersectionObserver (thrill.com's game rows). A no-op observer never
        // fires its callback, so the section stays a skeleton and its data fetch
        // never happens. Our completion-biased observer reports the element
        // intersecting once — proven by capturing the fetch the callback makes.
        let html = r#"<html><body><div id="section"></div><script>
            var io = new IntersectionObserver(function (entries) {
                for (var i = 0; i < entries.length; i++) {
                    if (entries[i].isIntersecting) fetch("/api/lazy-section-data");
                }
            });
            io.observe(document.getElementById("section"));
        </script></body></html>"#;
        let report = run_capture(
            "https://lazy.example.com/",
            html,
            &CaptureConfig {
                capture_window_ms: 500,
                quiesce_ms: 20,
                max_intercepts: 8,
                stub_response_json: "[]".to_string(),
            },
            null_fetcher(),
        );
        assert!(
            report
                .requests
                .iter()
                .any(|r| r.url == "https://lazy.example.com/api/lazy-section-data"),
            "IntersectionObserver must fire so lazy-load-on-visible content triggers: {:?}, logs={:?}",
            report.requests.iter().map(|r| &r.url).collect::<Vec<_>>(),
            report.logs
        );
    }

    #[test]
    fn document_lifecycle_events_fire_after_script_eval() {
        // The classic boot gates: framework/data code runs immediately when the
        // document already finished loading, else waits for DOMContentLoaded /
        // load / readystatechange. happy-dom never runs the loading lifecycle for
        // our document.write path, so without glue §7 these gates stall forever
        // and the shell hydrates without its data (thrill.com's providers/
        // geolocation/license calls). All three canonical forms must fire, and
        // during script evaluation readyState must read "loading" (browser-
        // faithful: scripts execute during parse), flipping to "complete" after.
        let html = r#"<html><body><script>
            if (document.readyState === "loading") fetch("/api/was-loading");
            document.addEventListener("DOMContentLoaded", function () { fetch("/api/dcl"); });
            window.addEventListener("load", function () { fetch("/api/load"); });
            document.addEventListener("readystatechange", function () {
                if (document.readyState === "complete") fetch("/api/ready-complete");
            });
        </script></body></html>"#;
        let report = run_capture(
            "https://boot.example.com/",
            html,
            &CaptureConfig {
                capture_window_ms: 500,
                quiesce_ms: 20,
                max_intercepts: 8,
                stub_response_json: "{}".to_string(),
            },
            null_fetcher(),
        );
        let urls: Vec<&str> = report.requests.iter().map(|r| r.url.as_str()).collect();
        for want in [
            "https://boot.example.com/api/was-loading",
            "https://boot.example.com/api/dcl",
            "https://boot.example.com/api/load",
            "https://boot.example.com/api/ready-complete",
        ] {
            assert!(
                urls.contains(&want),
                "lifecycle gate {want} did not fire; got {urls:?}, logs={:?}",
                report.logs
            );
        }
    }

    #[test]
    fn push_log_dedupes_exact_repeats() {
        // A framework warn repeated per-component must not exhaust the log budget
        // and evict the one distinct error that explains a failure.
        let mut cs = CaptureState {
            requests: Vec::new(),
            max_intercepts: 8,
            stub_body: "{}".to_string(),
            fetcher: null_fetcher(),
            api_fetcher: None,
            inflight: Rc::new(Cell::new(0)),
            content_activity: Rc::new(Cell::new(0)),
            rendered_html: None,
            logs: Vec::new(),
            exec_result: None,
            started: Instant::now(),
        };
        for _ in 0..50 {
            cs.push_log("[console.warn] No --breakpoint-sm value found in CSS variables");
        }
        cs.push_log("[exception] TypeError: e.animate is not a function");
        assert_eq!(
            cs.logs.len(),
            2,
            "repeats deduped, distinct line retained: {:?}",
            cs.logs
        );
        assert!(cs.logs[1].contains("animate"), "{:?}", cs.logs);
    }

    #[test]
    fn finish_moves_capture_buffers_out_of_shared_state() {
        let cap = Rc::new(RefCell::new(CaptureState {
            requests: vec![CapturedRequest {
                method: "GET".to_string(),
                url: "https://example.test/api".to_string(),
                headers: Vec::new(),
                body: None,
                via: InterceptVia::Fetch,
            }],
            max_intercepts: 8,
            stub_body: "{}".to_string(),
            fetcher: null_fetcher(),
            api_fetcher: None,
            inflight: Rc::new(Cell::new(0)),
            content_activity: Rc::new(Cell::new(0)),
            rendered_html: Some("<html>unused on boot failure</html>".to_string()),
            logs: vec!["boot diagnostic".to_string()],
            exec_result: None,
            started: Instant::now(),
        }));

        let report = finish(cap.clone(), RuntimeOutcome::Threw, None);

        assert_eq!(report.requests.len(), 1);
        assert_eq!(report.logs, ["boot diagnostic"]);
        assert_eq!(report.rendered_html, None);
        let state = cap.borrow();
        assert!(state.requests.is_empty(), "requests must be moved out");
        assert!(
            state.rendered_html.is_none(),
            "rendered HTML must be released"
        );
        assert!(state.logs.is_empty(), "logs must be moved out");
    }

    #[test]
    fn memory_logs_survive_ordinary_log_saturation() {
        let mut cs = CaptureState {
            requests: Vec::new(),
            max_intercepts: 8,
            stub_body: "{}".to_string(),
            fetcher: null_fetcher(),
            api_fetcher: None,
            inflight: Rc::new(Cell::new(0)),
            content_activity: Rc::new(Cell::new(0)),
            rendered_html: None,
            logs: Vec::new(),
            exec_result: None,
            started: Instant::now(),
        };
        let phases = [
            "snapshot",
            "dom",
            "scripts-fetched",
            "scripts-run",
            "settled",
            "serialized",
        ];

        for (phase_index, phase) in phases.iter().enumerate() {
            for ordinary_index in 0..MAX_RUNTIME_LOGS {
                cs.push_log(&format!("[ordinary] {phase_index}:{ordinary_index}"));
            }
            cs.push_log(&format!(
                "[raze.memory] phase={phase} used_heap_size=0 total_heap_size=0 \
                 total_physical_size=0 external_memory=0 heap_size_limit=0 \
                 module_registry_bytes=0 retained_external_script_bytes=0"
            ));
        }

        let memory_phases: Vec<&str> = cs
            .logs
            .iter()
            .filter_map(|line| {
                line.strip_prefix("[raze.memory] phase=")
                    .and_then(|rest| rest.split_whitespace().next())
            })
            .collect();
        assert_eq!(memory_phases, phases);
        assert_eq!(cs.logs.len(), MAX_RUNTIME_LOGS);
        assert_eq!(
            cs.logs
                .iter()
                .filter(|line| !line.starts_with("[raze.memory] "))
                .count(),
            MAX_RUNTIME_LOGS - phases.len()
        );
    }

    #[test]
    fn is_tracker_flags_known_vendors_but_not_first_party_content() {
        // Non-content vendors observed pinning the capture window (the target.com
        // tail: FullStory, DoubleVerify, googlesyndication, Medallia, Attentive,
        // PerimeterX/px-cloud, zeronaught, amplitude) must be classified trackers.
        for u in [
            "https://edge.fullstory.com/s/settings/o-x/v1/web",
            "https://pub.doubleverify.com/dvtag/signals/bsc/pub.json",
            "https://pagead2.googlesyndication.com/pagead/ping?e=1",
            "https://resources.digital-cloud.medallia.com/wdcus/onsiteData.json",
            "https://target.attn.tv/unrenderedCreative?v=1",
            "https://ift.px-cloud.net/ns?c=x",
            "https://ponos.zeronaught.com/2?a=x",
            "https://api.eu.amplitude.com/2/httpapi",
        ] {
            assert!(is_tracker(u), "should be a tracker: {u}");
        }
        // First-party / content endpoints must NEVER be misclassified — these hold
        // the capture window open until the content has actually rendered.
        for u in [
            "https://thrill.com/api/v2/games/providers",
            "https://games-state.thrill.com/snapshots/09-07-2026.json",
            "https://api.bluff.com/promotions",
            "https://redoak.target.com/content-publish/pages/v1?url=/",
            "https://www.target.com/",
            "https://example.com/data.json",
        ] {
            assert!(!is_tracker(u), "must NOT be a tracker: {u}");
        }
    }
}
