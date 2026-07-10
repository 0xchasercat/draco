//! # Interact sessions — a resumable Tier 2 isolate (v0.17.0, slice 2)
//!
//! [`run_capture`](crate::run_capture) runs the isolate **once**: build → hydrate
//! → drive the capture window → serialize → tear down, all inside a single
//! `block_on`. Interact needs the same isolate to stay **alive across turns** so an
//! LLM can open a page, then repeatedly run JS in page scope, read the result +
//! console, and (slice 4) click/navigate — with the network session (cookies)
//! persisted for the whole job.
//!
//! ## The actor
//!
//! A `JsRuntime` is `!Send` and thread-bound, so a session **owns a dedicated OS
//! thread** running a current-thread tokio runtime (the same driver set
//! [`run_capture`] uses). That thread hydrates the page once, then services
//! commands off an `mpsc` channel, replying over per-command `oneshot`s. Between
//! commands the loop keeps **pumping the event loop** on a short tick, so timers
//! and in-flight fetches armed in one turn keep resolving before the next — the
//! resumable analogue of `drive_capture_window`'s pump.
//!
//! The [`Session`] handle is `Send` (it holds only channels + the join handle);
//! the isolate never leaves its thread. Because `Rc<dyn ScriptFetcher>` is `!Send`
//! it **cannot** be passed in from another thread — the caller instead supplies a
//! `Send` [`FetcherFactory`] closure that the session thread invokes locally to
//! build the `!Send` fetchers in place. `draco-core` implements that factory over
//! its pooled `draco-net` client + shared cookie jar (all `Send`), constructing the
//! `Rc` wrappers on the session thread.
//!
//! ## Scope (slice 2)
//!
//! This module is the **session actor primitive**: open (hydrate + hold), `exec`
//! (run JS in page scope, settle, return the console lines produced this turn),
//! `serialize` (snapshot the live DOM), and clean `close`/teardown. The
//! devtools-console *return-value* serialization (`full`/`maxBytes`) is slice 3 and
//! slots into [`Command::Exec`] behind a new op; navigation (SPA vs full-document
//! refetch through the session cookie jar) is slice 4. Both are called out at their
//! extension points below.
//!
//! Containment is unchanged: the isolate has no host-capability bindings, so
//! `exec`'s JS can only cause the fetches the engine brokers — exactly as in a
//! one-shot capture. Making the isolate resumable does not widen the boundary.
//!
//! > **Note (no-fork intent).** `hydrate` repeats the *linear open sequence* of
//! > `run_capture_inner` (inputs → glue → document-order script eval →
//! > lifecycle) but reuses every non-trivial primitive verbatim (`CaptureState`,
//! > the ops extension, `MapModuleLoader`, `extract_scripts`,
//! > `eval_module`, `serialize_dom`, `poll_once`). Once slice 2 is green on the
//! > macOS gate, that shared sequence should be extracted into a single helper both
//! > entry points call; it is duplicated here (not refactored into the shipped
//! > one-shot path) only to keep this change from touching `scrape`/`render` while
//! > it is authored without a local compiler.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use deno_core::{JsRuntime, RuntimeOptions};
use tokio::sync::{mpsc, oneshot};

use crate::{
    draco_runtime_ext, eval_module, extract_scripts, json_string_literal, normalize_stub_body,
    poll_once, quiesce_tick_ms, resolve_script_url, serialize_dom, ApiFetcher, CaptureConfig,
    CaptureState, MapModuleLoader, ScriptFetcher, GLUE_JS, SNAPSHOT,
};

/// Build the isolate's `!Send` fetchers **on the session thread**.
///
/// `Rc<dyn ScriptFetcher>` / `Rc<dyn ApiFetcher>` cannot cross the thread
/// boundary, so the caller hands over a `Send` closure that constructs them in
/// place. `draco-core` closes over its pooled client + the session's shared cookie
/// jar (all `Send`) and returns the `Rc` wrappers. `None` for the [`ApiFetcher`] =
/// Observe mode (synthetic stubs); `Some` = Render mode (live data).
pub type FetcherFactory =
    Box<dyn FnOnce() -> (Rc<dyn ScriptFetcher>, Option<Rc<dyn ApiFetcher>>) + Send + 'static>;

/// Inputs to open a session. All fields are `Send` (the `!Send` fetchers arrive via
/// the [`FetcherFactory`], not here).
pub struct SessionConfig {
    /// Document URL the initial HTML is evaluated under.
    pub url: String,
    /// Initial page HTML (as fetched by `draco-net`).
    pub html: String,
    /// Capture-window knobs, reused for the initial hydrate settle and each
    /// `exec` settle.
    pub capture: CaptureConfig,
}

/// What a turn produced. Slice 3 adds `result: serde_json::Value` (the serialized
/// completion value, honoring `full`/`maxBytes`); slice 4 adds
/// `navigated: Option<(String, String)>`.
#[derive(Debug, Clone, Default)]
pub struct ExecReport {
    /// `false` if the script threw at evaluation time (the throw text is in
    /// `error` and appended to `logs`).
    pub ok: bool,
    /// Evaluation-time throw, if any.
    pub error: Option<String>,
    /// Page-side diagnostic/console lines emitted *during this turn* (the delta of
    /// [`CaptureState::logs`] over the turn) — the "console output" half of the
    /// devtools console.
    pub logs: Vec<String>,
}

/// Why a session call failed at the plumbing level (distinct from a page-JS throw,
/// which is a successful call returning `ExecReport { ok: false, .. }`).
#[derive(Debug, Clone)]
pub enum SessionError {
    /// The isolate failed to boot / hydrate; carries the reason.
    Hydrate(String),
    /// The session thread is gone (closed or panicked) — the command could not be
    /// delivered or its reply never arrived.
    Closed,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Hydrate(e) => write!(f, "interact session failed to hydrate: {e}"),
            SessionError::Closed => write!(f, "interact session is closed"),
        }
    }
}

impl std::error::Error for SessionError {}

/// One instruction for the session thread. Each carries its own reply channel.
enum Command {
    /// Evaluate `js` in page global scope; if `settle`, pump to quiesce afterwards
    /// so DOM updates from triggered fetches land before the caller reads back.
    Exec {
        js: String,
        settle: bool,
        reply: oneshot::Sender<ExecReport>,
    },
    /// Serialize the live hydrated DOM (`document.documentElement.outerHTML`).
    Serialize {
        reply: oneshot::Sender<Option<String>>,
    },
    /// Tear the isolate down and end the thread.
    Close { reply: oneshot::Sender<()> },
}

/// A live interact session: a `Send` handle to the isolate running on its own
/// thread. Dropping it (without [`close`](Session::close)) signals the thread to
/// tear down and detaches it.
pub struct Session {
    cmd_tx: mpsc::UnboundedSender<Command>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    /// Open a session: spawn the isolate thread, hydrate `config.html` under
    /// `config.url` (Observe or Render per the factory), settle once, and return a
    /// handle ready for `exec`/`serialize`. Errors if the isolate can't boot.
    pub async fn open(
        config: SessionConfig,
        fetchers: FetcherFactory,
    ) -> Result<Session, SessionError> {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();

        let join = std::thread::Builder::new()
            .name("draco-interact".to_string())
            .spawn(move || {
                // Current-thread runtime with the full driver set — identical to the
                // one-shot capture path; `JsRuntime` is `!Send` so it must be
                // current-thread, and `draco-net` sockets + `op_sleep` need I/O+time.
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("build tokio runtime: {e}")));
                        return;
                    }
                };
                rt.block_on(actor_main(config, fetchers, ready_tx, cmd_rx));
            })
            .map_err(|e| SessionError::Hydrate(format!("spawn session thread: {e}")))?;

        match ready_rx.await {
            Ok(Ok(())) => Ok(Session {
                cmd_tx,
                join: Some(join),
            }),
            Ok(Err(e)) => {
                let _ = join.join();
                Err(SessionError::Hydrate(e))
            }
            // The thread dropped the sender without reporting readiness (panicked).
            Err(_) => {
                let _ = join.join();
                Err(SessionError::Hydrate(
                    "session thread exited during boot".to_string(),
                ))
            }
        }
    }

    /// Run `js` in page global scope. `settle = true` pumps the event loop to
    /// quiesce after the microtask drain (the default the daemon uses); `false`
    /// returns right after the drain. Returns the console lines produced this turn.
    pub async fn exec(&self, js: String, settle: bool) -> Result<ExecReport, SessionError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Exec { js, settle, reply })
            .map_err(|_| SessionError::Closed)?;
        rx.await.map_err(|_| SessionError::Closed)
    }

    /// Snapshot the live DOM as serialized HTML (the raw material for a
    /// render-then-Markdown pass), or `None` if nothing usable serialized.
    pub async fn serialize(&self) -> Result<Option<String>, SessionError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Serialize { reply })
            .map_err(|_| SessionError::Closed)?;
        rx.await.map_err(|_| SessionError::Closed)
    }

    /// Tear the isolate down and join the thread. Best-effort: if the thread is
    /// already gone this still resolves.
    pub async fn close(mut self) -> Result<(), SessionError> {
        let (reply, rx) = oneshot::channel();
        // If the send fails the thread is already gone — treat close as done.
        if self.cmd_tx.send(Command::Close { reply }).is_ok() {
            let _ = rx.await;
        }
        if let Some(join) = self.join.take() {
            // The thread is exiting (or exited); the join returns promptly.
            let _ = join.join();
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Dropping the sender ends the command loop (recv -> None); detach the
        // thread so a dropped handle never blocks. Explicit `close` is preferred
        // for a synchronous teardown.
        if let Some(join) = self.join.take() {
            drop(join);
        }
    }
}

/// The session thread's entry: hydrate once, then service commands until closed.
async fn actor_main(
    config: SessionConfig,
    fetchers: FetcherFactory,
    ready_tx: oneshot::Sender<Result<(), String>>,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
) {
    let (mut runtime, cap, _modules, _inflight) = match hydrate(&config, fetchers).await {
        Ok(parts) => parts,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    // Initial settle: let the freshly-hydrated page's scheduled work land, exactly
    // as the one-shot path's capture window does, before we report ready.
    pump_to_quiesce(&mut runtime, &cap, &config.capture).await;
    if ready_tx.send(Ok(())).is_err() {
        // Opener gave up; nothing to serve.
        return;
    }

    // Idle pump cadence between commands — reuse the capture window's tick so an
    // async op landing between turns is noticed within a tick, never lost.
    let tick = Duration::from_millis(quiesce_tick_ms(config.capture.quiesce_ms));

    loop {
        tokio::select! {
            biased;
            maybe_cmd = cmd_rx.recv() => {
                match maybe_cmd {
                    None => break, // all handles dropped -> tear down
                    Some(Command::Exec { js, settle, reply }) => {
                        let report = do_exec(&mut runtime, &cap, &config.capture, &js, settle).await;
                        let _ = reply.send(report);
                    }
                    Some(Command::Serialize { reply }) => {
                        serialize_dom(&mut runtime);
                        let html = cap.borrow().rendered_html.clone();
                        let _ = reply.send(html);
                    }
                    Some(Command::Close { reply }) => {
                        let _ = reply.send(());
                        break;
                    }
                }
            }
            _ = tokio::time::sleep(tick) => {
                // Keep background work (timers, in-flight fetches) progressing while
                // idle-waiting for the next command. A drained loop returns fast, so
                // this is not a busy-spin.
                let _ = poll_once(&mut runtime);
            }
        }
    }
    // `runtime` (and the isolate) drops here — clean teardown.
}

/// Build the isolate and evaluate the page to the parsing-finished + lifecycle
/// point, returning the live runtime and shared state so the caller can keep
/// driving it. Mirrors `run_capture_inner`'s open sequence (see module note);
/// reuses all of its primitives.
async fn hydrate(
    config: &SessionConfig,
    fetchers: FetcherFactory,
) -> Result<
    (
        JsRuntime,
        Rc<RefCell<CaptureState>>,
        Rc<RefCell<HashMap<String, Vec<u8>>>>,
        Rc<Cell<u32>>,
    ),
    String,
> {
    crate::ensure_v8_flags();

    // Build the `!Send` fetchers ON this thread.
    let (fetcher, api_fetcher) = fetchers();

    let stub_body = normalize_stub_body(&config.capture.stub_response_json);
    let modules: Rc<RefCell<HashMap<String, Vec<u8>>>> = Rc::new(RefCell::new(HashMap::new()));
    let inflight: Rc<Cell<u32>> = Rc::new(Cell::new(0));

    let cap = Rc::new(RefCell::new(CaptureState {
        requests: Vec::new(),
        max_intercepts: config.capture.max_intercepts,
        stub_body: stub_body.clone(),
        fetcher: fetcher.clone(),
        api_fetcher,
        inflight: inflight.clone(),
        content_activity: Rc::new(Cell::new(0)),
        rendered_html: None,
        logs: Vec::new(),
        started: Instant::now(),
    }));

    let mut runtime = JsRuntime::new(RuntimeOptions {
        startup_snapshot: Some(SNAPSHOT),
        extensions: vec![draco_runtime_ext::init(cap.clone())],
        module_loader: Some(Rc::new(MapModuleLoader {
            modules: modules.clone(),
            fetcher: fetcher.clone(),
            inflight: inflight.clone(),
            cap: cap.clone(),
        })),
        ..Default::default()
    });

    // Inputs + glue (build the happy-dom Window, install the fetch/XHR interceptor,
    // load the HTML).
    let url_lit = json_string_literal(&config.url);
    let html_lit = json_string_literal(&config.html);
    let stub_lit = json_string_literal(&stub_body);
    runtime
        .execute_script(
            "draco:inputs",
            format!(
                "globalThis.__DRACO_URL__={url_lit}; globalThis.__DRACO_HTML__={html_lit}; \
                 globalThis.__DRACO_STUB__={stub_lit};"
            ),
        )
        .map_err(|e| format!("inputs: {e}"))?;
    runtime
        .execute_script("draco:glue", GLUE_JS)
        .map_err(|e| format!("glue: {e}"))?;

    // Evaluate the page's scripts in document order (external fetched concurrently),
    // pointing document.currentScript at the real parsed node per block.
    let scripts = extract_scripts(&config.html);
    let external: Vec<(usize, String)> = scripts
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.inline)
        .map(|(i, s)| (i, resolve_script_url(&config.url, &s.payload)))
        .collect();
    let fetched = futures::future::join_all(external.iter().map(|(_, u)| fetcher.fetch(u))).await;
    let mut ext_bytes: HashMap<usize, Vec<u8>> = HashMap::new();
    for ((i, _), bytes) in external.iter().zip(fetched) {
        if let Some(b) = bytes {
            ext_bytes.insert(*i, b);
        }
    }

    for (i, script) in scripts.into_iter().enumerate() {
        let (source, spec_str) = if script.inline {
            let base = config.url.split('#').next().unwrap_or(&config.url);
            (script.payload.clone(), format!("{base}#draco-inline-{i}"))
        } else {
            let resolved = resolve_script_url(&config.url, &script.payload);
            match ext_bytes.remove(&i) {
                Some(bytes) => (String::from_utf8_lossy(&bytes).into_owned(), resolved),
                None => continue,
            }
        };

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
        let _ = runtime.execute_script("draco:currentScript", set_cs);

        if script.module {
            match deno_core::url::Url::parse(&spec_str) {
                Ok(spec_url) => {
                    modules
                        .borrow_mut()
                        .insert(spec_url.as_str().to_string(), source.into_bytes());
                    if let Err(e) = eval_module(&mut runtime, &spec_url).await {
                        cap.borrow_mut()
                            .push_log(&format!("module script {i} threw: {e}"));
                    }
                }
                Err(e) => {
                    cap.borrow_mut()
                        .push_log(&format!("bad module specifier for script {i}: {e}"));
                }
            }
        } else {
            let name = if script.inline {
                spec_str.clone()
            } else {
                format!("draco:page[{i}]")
            };
            if let Err(e) = runtime.execute_script(name, source) {
                cap.borrow_mut()
                    .push_log(&format!("page script {i} threw: {e}"));
            }
        }
    }
    let _ = runtime.execute_script(
        "draco:currentScript:clear",
        "try { globalThis.__dracoClearCurrentScript(); } catch (_) {}",
    );
    // Parsing-finished: fire readyState/DOMContentLoaded/load so gated boot code runs.
    let _ = runtime.execute_script(
        "draco:lifecycle",
        "try { globalThis.__dracoFireLifecycle(); } catch (_) {}",
    );

    Ok((runtime, cap, modules, inflight))
}

/// Evaluate one turn's JS in page global scope, then (optionally) settle.
///
/// Slice 3 will replace the wrapper with one that captures the completion value
/// through a new op and returns it as `ExecReport.result` under the `full`/
/// `maxBytes` budget; for now the turn's observable output is its console lines.
async fn do_exec(
    runtime: &mut JsRuntime,
    cap: &Rc<RefCell<CaptureState>>,
    cfg: &CaptureConfig,
    js: &str,
    settle: bool,
) -> ExecReport {
    let log_start = cap.borrow().logs.len();

    // Wrap as an async IIFE so a turn can `await` (fetch, dynamic import, timers).
    // The completion value resolves on the event loop; the settle pump below drives
    // it. Errors are reported, never fatal to the session.
    let wrapped = format!("(async () => {{\n{js}\n}})();");
    let mut error = None;
    if let Err(e) = runtime.execute_script("draco:interact:exec", wrapped) {
        error = Some(e.to_string());
        cap.borrow_mut().push_log(&format!("exec threw: {e}"));
    }

    // Always drain microtasks at least once so a purely-synchronous turn's effects
    // (and a resolved async IIFE with no pending I/O) are visible immediately.
    let _ = poll_once(runtime);
    if settle {
        pump_to_quiesce(runtime, cap, cfg).await;
    }

    let logs = {
        let cs = cap.borrow();
        cs.logs
            .get(log_start..)
            .map(|s| s.to_vec())
            .unwrap_or_default()
    };
    ExecReport {
        ok: error.is_none(),
        error,
        logs,
    }
}

/// Pump the event loop until it quiesces (no new content activity for `quiesce_ms`
/// and no loads in flight) or the hard cap elapses. A bounded, self-contained
/// mirror of `drive_capture_window`'s loop, used for the initial hydrate settle and
/// each `exec(settle=true)`.
async fn pump_to_quiesce(
    runtime: &mut JsRuntime,
    cap: &Rc<RefCell<CaptureState>>,
    cfg: &CaptureConfig,
) {
    let start = Instant::now();
    let hard_cap = Duration::from_millis(cfg.capture_window_ms);
    let quiesce = Duration::from_millis(cfg.quiesce_ms);
    let tick = Duration::from_millis(quiesce_tick_ms(cfg.quiesce_ms));

    let content_activity = cap.borrow().content_activity.clone();
    let inflight = cap.borrow().inflight.clone();
    let mut last_count = content_activity.get();
    let mut last_activity = Instant::now();

    loop {
        match poll_once(runtime) {
            std::task::Poll::Ready(_) => break, // drained (or loop error) — done
            std::task::Poll::Pending => {}
        }

        let now_count = content_activity.get();
        if now_count != last_count {
            last_count = now_count;
            last_activity = Instant::now();
        }
        if inflight.get() > 0 {
            last_activity = Instant::now();
        }

        if start.elapsed() >= hard_cap {
            break;
        }
        if last_activity.elapsed() >= quiesce {
            break;
        }
        tokio::time::sleep(tick).await;
    }
}
