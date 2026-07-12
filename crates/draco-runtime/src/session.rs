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
use futures::future::LocalBoxFuture;
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};

use crate::{
    draco_runtime_ext, eval_module, extract_scripts, json_string_literal, normalize_stub_body,
    poll_once, quiesce_tick_ms, resolve_script_url, serialize_dom, ApiFetcher, CaptureConfig,
    CaptureState, MapModuleLoader, ScriptFetcher, GLUE_JS, SNAPSHOT,
};

/// Fetch a top-level document for an in-session navigation, cookie-aware.
///
/// Distinct from [`ScriptFetcher`] (code loads) and [`ApiFetcher`] (the page's own
/// data requests): this fetches the *next page's* HTML when the session navigates,
/// through `draco-net` **with the session's shared cookie jar**, so a `Set-Cookie`
/// (login, CSRF, session id) from one page rides to the next — the browser-tab
/// behaviour that makes multi-page interact flows work. Returns the final URL after
/// redirects plus the HTML, or `None` if the fetch failed. `draco-core` implements
/// it; `None` on the session (no page fetcher supplied) disables navigation.
pub trait PageFetcher {
    fn fetch_page<'a>(&'a self, url: &'a str) -> LocalBoxFuture<'a, Option<(String, String)>>;
}

/// The `!Send` fetcher set a session runs on, built **on the session thread**.
///
/// `Rc<dyn …>` fetchers cannot cross the thread boundary, so the caller hands over
/// a `Send` [`FetcherFactory`] closure that constructs them in place. `draco-core`
/// closes over its pooled client + the session's shared cookie jar (all `Send`) and
/// returns the `Rc` wrappers. Built once at open and reused for every navigation
/// re-hydrate, so the cookie jar (captured inside them) persists for the session.
pub struct SessionFetchers {
    /// Script/module/chunk byte source (always present).
    pub scripts: Rc<dyn ScriptFetcher>,
    /// The page's own data requests: `Some` = Render mode (live), `None` = Observe
    /// (synthetic stubs).
    pub api: Option<Rc<dyn ApiFetcher>>,
    /// Top-level document fetch for navigation: `Some` enables `navigate`, `None`
    /// disables it (e.g. a single-page interact with no navigation).
    pub page: Option<Rc<dyn PageFetcher>>,
}

/// A `Send` closure that builds the [`SessionFetchers`] on the session thread.
pub type FetcherFactory = Box<dyn FnOnce() -> SessionFetchers + Send + 'static>;

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

/// What a turn produced. Slice 4 adds `navigated: Option<(String, String)>`.
#[derive(Debug, Clone, Default)]
pub struct ExecReport {
    /// `false` if the turn threw — at evaluation time (a compile/dispatch error)
    /// **or** inside the page JS (an async throw the wrapper caught). Either way
    /// the throw text is in `error`.
    pub ok: bool,
    /// The turn's throw, if any: evaluation-time errors and page-side caught
    /// throws (which arrive from the wrapper as an `{ "__error": … }` value) are
    /// both promoted here, so callers never have to fish an error out of `result`.
    pub error: Option<String>,
    /// The turn's completion value — whatever the JS `return`ed — serialized to
    /// JSON under the size budget (see [`ExecOptions`]). A turn that completes
    /// with `undefined`/nothing yields a `{ "__undefined": true }` descriptor, so
    /// it is distinguishable from an expression that *evaluates to* `null`.
    /// `None` only when the turn threw. DOM nodes/functions/cycles are
    /// *described* rather than dropped; an over-budget value becomes a
    /// `{ "__truncated": true, "bytes", "maxBytes", "preview" }` descriptor unless
    /// `full` was set. This is the devtools-console return value.
    pub result: Option<serde_json::Value>,
    /// Page-side diagnostic/console lines emitted *during this turn* (the delta of
    /// [`CaptureState::logs`] over the turn) — the "console output" half of the
    /// devtools console.
    pub logs: Vec<String>,
}

/// Per-turn `exec` knobs.
#[derive(Debug, Clone)]
pub struct ExecOptions {
    /// After the JS eval + microtask drain, pump the event loop to quiesce so DOM
    /// updates from triggered fetches land before the caller reads back. Default
    /// `true`; `false` returns right after the microtask drain.
    pub settle: bool,
    /// Return the completion value untruncated regardless of `max_bytes`.
    pub full: bool,
    /// Approximate size budget (JS string length) for the serialized result; over
    /// budget yields a truncation descriptor unless `full`. Default 256 KiB.
    pub max_bytes: usize,
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self {
            settle: true,
            full: false,
            max_bytes: 262_144,
        }
    }
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
    /// Evaluate `js` in page global scope under `opts` (settle + result budget).
    Exec {
        js: String,
        opts: ExecOptions,
        reply: oneshot::Sender<ExecReport>,
    },
    /// Serialize the live hydrated DOM (`document.documentElement.outerHTML`).
    Serialize {
        reply: oneshot::Sender<Option<String>>,
    },
    /// Navigate to `url`: fetch the next document (cookie-aware), tear down the
    /// current isolate, and re-hydrate in place.
    Navigate {
        url: String,
        reply: oneshot::Sender<NavReport>,
    },
    /// Run a batch of high-fidelity interactions (click/type/…), settling the DOM
    /// after each so a reactive render (modal, route swap) is captured.
    Act {
        actions: Vec<Action>,
        reply: oneshot::Sender<ActReport>,
    },
    /// Tear the isolate down and end the thread.
    Close { reply: oneshot::Sender<()> },
}

/// Outcome of a [`Session::navigate`].
#[derive(Debug, Clone, Default)]
pub struct NavReport {
    /// `true` if the new document was fetched and re-hydrated.
    pub ok: bool,
    /// The final URL after redirects (present on success).
    pub url: Option<String>,
    /// Why navigation failed (no page fetcher, fetch failed, or re-hydrate threw).
    pub error: Option<String>,
}

/// One high-fidelity interaction. Firecrawl-shaped (`{ "type": "...", ... }`),
/// deserialized directly from the request `actions[]`. Unknown fields ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Action {
    /// Click an element: focus + a faithful pointer/mouse event sequence.
    Click { selector: String },
    /// Focus `selector` (or the active element) and type `text`, dispatching
    /// `input`/`change` so framework bindings update.
    Type {
        #[serde(default)]
        selector: Option<String>,
        text: String,
        /// Clear the field first (default true).
        #[serde(default = "default_true")]
        clear: bool,
    },
    /// Dispatch a `keydown`/`keyup` for `key` on `selector` (or the document).
    Press {
        #[serde(default)]
        selector: Option<String>,
        key: String,
    },
    /// Scroll `selector` into view, or the window by `direction` (`up`/`down`).
    Scroll {
        #[serde(default)]
        selector: Option<String>,
        #[serde(default)]
        direction: Option<String>,
    },
    /// Set a `<select>`'s value and dispatch `change`.
    Select { selector: String, value: String },
    /// Hover: pointerover/mouseover/mouseenter/mousemove.
    Hover { selector: String },
    /// Wait until `selector` appears (up to `timeout_ms`), or a fixed
    /// `milliseconds` pause. Defaults: 5000 ms selector timeout.
    Wait {
        #[serde(default)]
        selector: Option<String>,
        #[serde(default)]
        milliseconds: Option<u64>,
    },
}

fn default_true() -> bool {
    true
}

/// Per-action outcome within an [`ActReport`].
#[derive(Debug, Clone)]
pub struct ActStep {
    /// A short label for the action (e.g. `click a.login`).
    pub action: String,
    pub ok: bool,
    /// Why this step failed (selector not found, JS throw, wait timeout).
    pub error: Option<String>,
}

/// Outcome of a [`Session::act`] batch. `ok` is true iff every step succeeded;
/// steps stop at the first failure (a later action usually depends on an earlier
/// one landing). The caller reads the resulting DOM via `serialize`/`scrape`.
#[derive(Debug, Clone, Default)]
pub struct ActReport {
    pub ok: bool,
    pub steps: Vec<ActStep>,
    /// Console lines emitted across the batch.
    pub logs: Vec<String>,
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

    /// Run `js` in page global scope (as an async body, so it may `await` and
    /// `return` a value). Returns the completion value (serialized under
    /// `opts`), the console lines produced this turn, and any evaluation throw.
    pub async fn exec(&self, js: String, opts: ExecOptions) -> Result<ExecReport, SessionError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Exec { js, opts, reply })
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

    /// Navigate the session to `url`: fetch the next document through the session's
    /// cookie-aware page fetcher, tear down the current isolate, and re-hydrate the
    /// new page in the same session (so cookies set so far ride along). Returns a
    /// [`NavReport`]; `ok = false` if no page fetcher was supplied, the fetch
    /// failed, or the new page failed to boot. The session stays usable either way
    /// (on failure the previous page remains loaded).
    pub async fn navigate(&self, url: String) -> Result<NavReport, SessionError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Navigate { url, reply })
            .map_err(|_| SessionError::Closed)?;
        rx.await.map_err(|_| SessionError::Closed)
    }

    /// Run a batch of interactions in order, dispatching a faithful event
    /// sequence per action and settling the DOM after each so a reactive render
    /// (a modal, a route swap) is captured. Stops at the first failed step.
    /// Read the resulting page with `serialize`/`scrape` afterwards.
    pub async fn act(&self, actions: Vec<Action>) -> Result<ActReport, SessionError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Act { actions, reply })
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
    fetchers_factory: FetcherFactory,
    ready_tx: oneshot::Sender<Result<(), String>>,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
) {
    // Build the `!Send` fetchers once, on this thread; reuse them for the initial
    // hydrate and every navigation re-hydrate (so the cookie jar inside them
    // persists across pages).
    let fetchers = fetchers_factory();

    let (mut runtime, mut cap) = match hydrate(&config, &fetchers).await {
        Ok(parts) => (Some(parts.0), Some(parts.1)),
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    // Initial settle: let the freshly-hydrated page's scheduled work land, exactly
    // as the one-shot path's capture window does, before we report ready.
    pump_to_quiesce(
        runtime.as_mut().unwrap(),
        cap.as_ref().unwrap(),
        &config.capture,
    )
    .await;
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
                    Some(Command::Exec { js, opts, reply }) => {
                        let report = do_exec(runtime.as_mut().unwrap(), cap.as_ref().unwrap(), &config.capture, &js, &opts).await;
                        let _ = reply.send(report);
                    }
                    Some(Command::Serialize { reply }) => {
                        serialize_dom(runtime.as_mut().unwrap());
                        let html = cap.as_ref().unwrap().borrow().rendered_html.clone();
                        let _ = reply.send(html);
                    }
                    Some(Command::Navigate { url, reply }) => {
                        let mut old_dropped = false;
                        let report = match &fetchers.page {
                            None => NavReport {
                                ok: false,
                                url: None,
                                error: Some(
                                    "navigation unavailable (no page fetcher)".to_string(),
                                ),
                            },
                            Some(page) => match page.fetch_page(&url).await {
                                None => NavReport {
                                    ok: false,
                                    url: None,
                                    error: Some(format!("failed to fetch {url}")),
                                },
                                Some((final_url, html)) => {
                                    // Drop the old V8 isolate *before* creating the
                                    // new one.  Two isolates on the same thread cause
                                    // a V8 HandleScope CHECK failure during the old
                                    // isolate's realm teardown (SetAlignedPointerIn-
                                    // EmbedderData creates a handle without a scope).
                                    drop(runtime.take());
                                    drop(cap.take());
                                    old_dropped = true;

                                    let nav_cfg = SessionConfig {
                                        url: final_url.clone(),
                                        html,
                                        capture: config.capture.clone(),
                                    };
                                    match hydrate(&nav_cfg, &fetchers).await {
                                        Ok((new_rt, new_cap)) => {
                                            runtime = Some(new_rt);
                                            cap = Some(new_cap);
                                            pump_to_quiesce(runtime.as_mut().unwrap(), cap.as_ref().unwrap(), &config.capture)
                                                .await;
                                            NavReport {
                                                ok: true,
                                                url: Some(final_url),
                                                error: None,
                                            }
                                        }
                                        Err(e) => NavReport {
                                            ok: false,
                                            url: None,
                                            error: Some(format!("re-hydrate failed: {e}")),
                                        },
                                    }
                                }
                            },
                        };
                        let ok = report.ok;
                        let _ = reply.send(report);
                        if old_dropped && !ok {
                            break;
                        }
                    }
                    Some(Command::Act { actions, reply }) => {
                        let report = do_act(
                            runtime.as_mut().unwrap(),
                            cap.as_ref().unwrap(),
                            &config.capture,
                            &actions,
                        )
                        .await;
                        let _ = reply.send(report);
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
                if let Some(rt) = runtime.as_mut() {
                    let _ = poll_once(rt);
                }
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
    fetchers: &SessionFetchers,
) -> Result<(JsRuntime, Rc<RefCell<CaptureState>>), String> {
    crate::ensure_v8_flags();

    // The `!Send` fetchers were built on this thread at open and are reused for
    // every navigation re-hydrate (so their cookie jar persists). Cheap Rc clones.
    let fetcher = fetchers.scripts.clone();
    let api_fetcher = fetchers.api.clone();

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
        exec_result: None,
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

    Ok((runtime, cap))
}

/// Evaluate one turn's JS in page global scope, capture its completion value, then
/// (optionally) settle.
///
/// The turn's `js` is the body of an async function, so it may `await` and must
/// `return` to yield a value (Firecrawl-`executeJavascript` semantics). The
/// wrapper awaits that body, serializes the value **page-side** under the size
/// budget (DOM nodes/functions/cycles described, not dropped; over-budget →
/// truncation descriptor unless `full`), and hands the JSON back through
/// `op_raze_exec_result`. Everything is in page-reachable scope only — no host
/// bindings — so an errant turn can throw or loop but never escape the isolate.
async fn do_exec(
    runtime: &mut JsRuntime,
    cap: &Rc<RefCell<CaptureState>>,
    cfg: &CaptureConfig,
    js: &str,
    opts: &ExecOptions,
) -> ExecReport {
    let log_start = cap.borrow().logs.len();
    // Clear any stale result so a turn that returns `undefined` reads back `None`.
    cap.borrow_mut().exec_result = None;

    // Budget the page-side serializer applies. `full` lifts it effectively to
    // infinity (a finite JS number larger than any real result length).
    let budget: f64 = if opts.full {
        f64::from(u32::MAX)
    } else {
        opts.max_bytes as f64
    };
    let wrapped = build_exec_wrapper(js, budget);
    let mut error = None;
    if let Err(e) = runtime.execute_script("draco:interact:exec", wrapped) {
        error = Some(e.to_string());
        cap.borrow_mut().push_log(&format!("exec threw: {e}"));
    }

    // Always drain microtasks at least once so a purely-synchronous turn's value
    // (and DOM effects) are captured immediately; settle drives async turns.
    let _ = poll_once(runtime);
    if opts.settle {
        pump_to_quiesce(runtime, cap, cfg).await;
    }

    let (logs, result) = {
        let mut cs = cap.borrow_mut();
        let logs = cs
            .logs
            .get(log_start..)
            .map(|s| s.to_vec())
            .unwrap_or_default();
        let result = cs.exec_result.take().map(|s| {
            serde_json::from_str::<serde_json::Value>(&s)
                .unwrap_or_else(|_| serde_json::Value::String(s.clone()))
        });
        (logs, result)
    };
    // Normalize the page-side channel into first-class report fields:
    // - a throw the wrapper caught arrives as `{ "__error": <stack> }` — promote
    //   it to `error` (=> ok:false) instead of leaving it buried in `result`
    //   while `error` reads null;
    // - `undefined` calls no op — surface it as a `{ "__undefined": true }`
    //   descriptor so it is distinguishable from a turn that evaluated to `null`.
    let result = match result {
        Some(serde_json::Value::Object(map)) if map.contains_key("__error") => {
            if error.is_none() {
                error = map
                    .get("__error")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .or_else(|| Some("exec threw".to_string()));
            }
            None
        }
        Some(v) => Some(v),
        None if error.is_none() => Some(serde_json::json!({ "__undefined": true })),
        None => None,
    };
    ExecReport {
        ok: error.is_none(),
        error,
        result,
        logs,
    }
}

/// Build the page-side exec wrapper: run the turn as an async body, then serialize
/// its return value to JSON under `budget` and hand it to `op_raze_exec_result`.
/// An `undefined` return calls no op; a throw is reported through the same channel
/// as an `{ "__error": ... }` value. `do_exec` normalizes both after the turn —
/// no-op → a `{ "__undefined": true }` result, `__error` → the report's `error`
/// field (ok:false) — so surfaces never have to interpret the raw channel.
fn build_exec_wrapper(js: &str, budget: f64) -> String {
    // The user source is embedded as a STRING and evaluated for its completion
    // value (devtools-console / REPL semantics) rather than spliced as a function
    // body — so a bare last expression (`document.title`, `els.length`) is
    // captured WITHOUT an explicit `return`. `MAXB` and that string literal are the
    // only interpolated parts; the serializer below is fixed.
    let src_lit = json_string_literal(js);
    format!(
        r#"(async () => {{
  const MAXB = {budget};
  const __src = {src_lit};
  let __v;
  try {{
    // Indirect eval runs in global scope (sees `document`/`window`, not our
    // wrapper locals); its completion value is the last expression's value.
    // `await` resolves a trailing promise (e.g. a bare `fetch(...)`).
    __v = await (0, eval)(__src);
  }} catch (e) {{
    if (e instanceof SyntaxError) {{
      // Not a bare expression (top-level `await`/`return`/`import`): run it as an
      // async body — a value still comes back via an explicit `return`.
      try {{
        __v = await (0, eval)("(async () => {{" + __src + "\n}})()");
      }} catch (e2) {{
        try {{ Deno.core.ops.op_raze_exec_result(JSON.stringify({{ __error: (e2 && e2.stack) ? String(e2.stack) : String(e2) }})); }} catch (_e) {{}}
        return;
      }}
    }} else {{
      try {{ Deno.core.ops.op_raze_exec_result(JSON.stringify({{ __error: (e && e.stack) ? String(e.stack) : String(e) }})); }} catch (_e) {{}}
      return;
    }}
  }}
  if (__v === undefined) return;
  const seen = new WeakSet();
  function desc(x, depth) {{
    if (x === null) return null;
    const t = typeof x;
    if (t === "number" || t === "boolean" || t === "string") return x;
    if (t === "bigint") return String(x);
    if (t === "function") return {{ __fn: x.name || "anonymous" }};
    if (t === "symbol") return String(x);
    if (t !== "object") return String(x);
    if (typeof x.nodeType === "number" && (x.nodeType === 1 || x.nodeType === 3 || x.nodeType === 9)) {{
      const tag = String(x.tagName || x.nodeName || "").toLowerCase();
      const o = {{ __node: tag }};
      if (x.id) o.id = x.id;
      let cls = (x.getAttribute && x.getAttribute("class")) || x.className;
      if (cls && typeof cls === "string") o.class = cls;
      const txt = String(x.textContent || "");
      o.text = txt.length > 120 ? txt.slice(0, 120) : txt;
      if (x.getAttribute) {{ const h = x.getAttribute("href"); if (h) o.href = h; }}
      return o;
    }}
    if (seen.has(x)) return {{ __cycle: true }};
    seen.add(x);
    if (depth > 6) return {{ __truncated: "depth" }};
    if (Array.isArray(x)) {{
      const n = Math.min(x.length, 1000);
      const a = [];
      for (let i = 0; i < n; i++) a.push(desc(x[i], depth + 1));
      if (x.length > n) a.push({{ __truncated: "length", total: x.length }});
      return a;
    }}
    const o = {{}};
    let c = 0;
    for (const k in x) {{
      if (!Object.prototype.hasOwnProperty.call(x, k)) continue;
      if (c++ > 200) {{ o.__truncated = "keys"; break; }}
      try {{ o[k] = desc(x[k], depth + 1); }} catch (_e) {{ o[k] = {{ __error: true }}; }}
    }}
    return o;
  }}
  let json;
  try {{ json = JSON.stringify(desc(__v, 0)); }} catch (e) {{ json = JSON.stringify({{ __error: String(e) }}); }}
  if (json === undefined) return;
  if (json.length > MAXB) {{
    json = JSON.stringify({{ __truncated: true, bytes: json.length, maxBytes: MAXB, preview: json.slice(0, MAXB) }});
  }}
  try {{ Deno.core.ops.op_raze_exec_result(json); }} catch (_e) {{}}
}})();"#
    )
}

// ===================================================================
// act — high-fidelity interaction primitives
// ===================================================================

/// Run a batch of interactions in order. Each action dispatches a faithful event
/// sequence in page scope, then the DOM is settled (see [`pump_to_dom_settled`])
/// so a reactive render triggered by the action is captured. Stops at the first
/// failed step. Reads back nothing itself — the caller `serialize`s afterwards.
async fn do_act(
    runtime: &mut JsRuntime,
    cap: &Rc<RefCell<CaptureState>>,
    cfg: &CaptureConfig,
    actions: &[Action],
) -> ActReport {
    let log_start = cap.borrow().logs.len();
    let mut steps: Vec<ActStep> = Vec::with_capacity(actions.len());
    let mut all_ok = true;

    for action in actions {
        let label = action_label(action);
        // `wait` is driven Rust-side (poll for the selector, or a bounded sleep);
        // every other action runs a page-scope event snippet that reports
        // `{ok,error}` back through `op_raze_exec_result`.
        let outcome = match action {
            Action::Wait {
                selector,
                milliseconds,
            } => wait_action(runtime, cap, cfg, selector.as_deref(), *milliseconds).await,
            _ => run_action_snippet(runtime, cap, &build_action_js(action)),
        };
        let ok = outcome.is_ok();
        steps.push(ActStep {
            action: label,
            ok,
            error: outcome.err(),
        });
        if !ok {
            all_ok = false;
            break;
        }
        // Let the SPA react (modal mount, route render) before the next action.
        pump_to_dom_settled(runtime, cap, cfg).await;
    }

    let logs = {
        let cs = cap.borrow();
        cs.logs
            .get(log_start..)
            .map(|s| s.to_vec())
            .unwrap_or_default()
    };
    ActReport {
        ok: all_ok,
        steps,
        logs,
    }
}

/// A short human label for an action, for the [`ActStep`] trace.
fn action_label(a: &Action) -> String {
    match a {
        Action::Click { selector } => format!("click {selector}"),
        Action::Type { selector, .. } => {
            format!("type {}", selector.as_deref().unwrap_or("<focused>"))
        }
        Action::Press { key, .. } => format!("press {key}"),
        Action::Scroll {
            selector,
            direction,
        } => format!(
            "scroll {}",
            selector
                .as_deref()
                .or(direction.as_deref())
                .unwrap_or("down")
        ),
        Action::Select { selector, .. } => format!("select {selector}"),
        Action::Hover { selector } => format!("hover {selector}"),
        Action::Wait {
            selector,
            milliseconds,
        } => match (selector, milliseconds) {
            (Some(s), _) => format!("wait {s}"),
            (None, Some(ms)) => format!("wait {ms}ms"),
            _ => "wait".to_string(),
        },
    }
}

/// Run one action's event snippet and read back its `{ok,error}` result.
fn run_action_snippet(
    runtime: &mut JsRuntime,
    cap: &Rc<RefCell<CaptureState>>,
    js: &str,
) -> Result<(), String> {
    cap.borrow_mut().exec_result = None;
    if let Err(e) = runtime.execute_script("draco:interact:act", js.to_string()) {
        return Err(format!("act snippet threw: {e}"));
    }
    let _ = poll_once(runtime);
    let raw = cap.borrow_mut().exec_result.take();
    match raw {
        Some(s) => {
            let v: serde_json::Value = serde_json::from_str(&s).unwrap_or(serde_json::Value::Null);
            if v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false) {
                Ok(())
            } else {
                Err(v
                    .get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("action failed")
                    .to_string())
            }
        }
        None => Err("action produced no result".to_string()),
    }
}

/// `wait`: a fixed bounded sleep (pumping the loop) or poll until `selector`
/// appears, ceilinged by the capture window.
async fn wait_action(
    runtime: &mut JsRuntime,
    cap: &Rc<RefCell<CaptureState>>,
    cfg: &CaptureConfig,
    selector: Option<&str>,
    milliseconds: Option<u64>,
) -> Result<(), String> {
    let tick = Duration::from_millis(30);
    if let Some(ms) = milliseconds {
        let ms = ms.min(cfg.capture_window_ms.max(1));
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(ms) {
            let _ = poll_once(runtime);
            tokio::time::sleep(tick).await;
        }
        return Ok(());
    }
    let sel = match selector {
        Some(s) => s,
        None => return Err("wait requires `selector` or `milliseconds`".to_string()),
    };
    let check_js = SEL_PRESENT_JS.replace("__SEL__", &json_string_literal(sel));
    let ceiling = Duration::from_millis(cfg.capture_window_ms);
    let start = Instant::now();
    loop {
        let _ = poll_once(runtime);
        cap.borrow_mut().exec_result = None;
        let _ = runtime.execute_script("draco:interact:wait", check_js.clone());
        let _ = poll_once(runtime);
        if cap.borrow().exec_result.as_deref() == Some("1") {
            return Ok(());
        }
        if start.elapsed() >= ceiling {
            return Err(format!("wait timed out for selector: {sel}"));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Build the page-scope event snippet for a (non-`wait`) action. Placeholder
/// substitution (not `format!`) keeps the embedded JS free of brace-escaping.
fn build_action_js(action: &Action) -> String {
    let body = match action {
        Action::Click { selector } => CLICK_JS.replace("__SEL__", &json_string_literal(selector)),
        // Free-form user values (`__TEXT__`, `__VALUE__`, `__KEY__`) are always
        // substituted LAST so a later `.replace` can never rescan a placeholder
        // token that happened to appear inside them.
        Action::Type {
            selector,
            text,
            clear,
        } => TYPE_JS
            .replace("__CLEAR__", if *clear { "true" } else { "false" })
            .replace("__SEL__", &opt_lit(selector))
            .replace("__TEXT__", &json_string_literal(text)),
        Action::Press { selector, key } => PRESS_JS
            .replace("__SEL__", &opt_lit(selector))
            .replace("__KEY__", &json_string_literal(key)),
        Action::Scroll {
            selector,
            direction,
        } => SCROLL_JS
            .replace(
                "__DIR__",
                &json_string_literal(direction.as_deref().unwrap_or("down")),
            )
            .replace("__SEL__", &opt_lit(selector)),
        Action::Select { selector, value } => SELECT_JS
            .replace("__SEL__", &json_string_literal(selector))
            .replace("__VALUE__", &json_string_literal(value)),
        Action::Hover { selector } => HOVER_JS.replace("__SEL__", &json_string_literal(selector)),
        // `wait` never reaches here (handled Rust-side in `do_act`).
        Action::Wait { .. } => {
            "Deno.core.ops.op_raze_exec_result(JSON.stringify({ok:true}));".to_string()
        }
    };
    WRAP_JS.replace("__BODY__", &body)
}

/// `None` selector → the JS literal `null`; `Some` → a quoted string literal.
fn opt_lit(s: &Option<String>) -> String {
    match s {
        Some(v) => json_string_literal(v),
        None => "null".to_string(),
    }
}

const WRAP_JS: &str = r#"(() => { try { __BODY__
  Deno.core.ops.op_raze_exec_result(JSON.stringify({ ok: true })); } catch (e) { try { Deno.core.ops.op_raze_exec_result(JSON.stringify({ ok: false, error: String((e && e.stack) || e) })); } catch (_e) {} } })();"#;

const CLICK_JS: &str = r#"const el = document.querySelector(__SEL__);
  if (!el) { Deno.core.ops.op_raze_exec_result(JSON.stringify({ ok: false, error: "selector not found: " + __SEL__ })); return; }
  try { if (el.scrollIntoView) el.scrollIntoView(); } catch (_e) {}
  try { if (el.focus) el.focus(); } catch (_e) {}
  const mk = (t) => { try { return new MouseEvent(t, { bubbles: true, cancelable: true, composed: true, view: (typeof window !== "undefined" ? window : null) }); } catch (_e) { return new Event(t, { bubbles: true, cancelable: true }); } };
  ["pointerover","pointerenter","pointerdown","mousedown","pointerup","mouseup","click"].forEach((t) => { try { el.dispatchEvent(mk(t)); } catch (_e) {} });"#;

const TYPE_JS: &str = r#"const el = __SEL__ ? document.querySelector(__SEL__) : (document.activeElement || null);
  if (!el) { Deno.core.ops.op_raze_exec_result(JSON.stringify({ ok: false, error: "no element to type into" })); return; }
  try { if (el.focus) el.focus(); } catch (_e) {}
  const hasValue = ("value" in el);
  if (__CLEAR__) { try { if (hasValue) { el.value = ""; } else { el.textContent = ""; } el.dispatchEvent(new Event("input", { bubbles: true })); } catch (_e) {} }
  try { if (hasValue) { el.value = (el.value || "") + __TEXT__; } else { el.textContent = (el.textContent || "") + __TEXT__; } } catch (_e) {}
  try { el.dispatchEvent(new Event("input", { bubbles: true })); } catch (_e) {}
  try { el.dispatchEvent(new Event("change", { bubbles: true })); } catch (_e) {}"#;

const PRESS_JS: &str = r#"const el = __SEL__ ? document.querySelector(__SEL__) : (document.activeElement || document.body || document.documentElement);
  if (!el) { Deno.core.ops.op_raze_exec_result(JSON.stringify({ ok: false, error: "no element for keypress" })); return; }
  const mk = (t) => { try { return new KeyboardEvent(t, { key: __KEY__, bubbles: true, cancelable: true }); } catch (_e) { return new Event(t, { bubbles: true, cancelable: true }); } };
  ["keydown","keyup"].forEach((t) => { try { el.dispatchEvent(mk(t)); } catch (_e) {} });"#;

const SCROLL_JS: &str = r#"if (__SEL__) { const el = document.querySelector(__SEL__); if (!el) { Deno.core.ops.op_raze_exec_result(JSON.stringify({ ok: false, error: "selector not found: " + __SEL__ })); return; } try { if (el.scrollIntoView) el.scrollIntoView(); } catch (_e) {} }
  else { const dy = (__DIR__ === "up") ? -1000 : 1000; try { if (typeof window !== "undefined" && window.scrollBy) window.scrollBy(0, dy); } catch (_e) {} }"#;

const SELECT_JS: &str = r#"const el = document.querySelector(__SEL__);
  if (!el) { Deno.core.ops.op_raze_exec_result(JSON.stringify({ ok: false, error: "selector not found: " + __SEL__ })); return; }
  try { el.value = __VALUE__; } catch (_e) {}
  try { el.dispatchEvent(new Event("input", { bubbles: true })); } catch (_e) {}
  try { el.dispatchEvent(new Event("change", { bubbles: true })); } catch (_e) {}"#;

const HOVER_JS: &str = r#"const el = document.querySelector(__SEL__);
  if (!el) { Deno.core.ops.op_raze_exec_result(JSON.stringify({ ok: false, error: "selector not found: " + __SEL__ })); return; }
  const mk = (t) => { try { return new MouseEvent(t, { bubbles: true, cancelable: true, composed: true }); } catch (_e) { return new Event(t, { bubbles: true }); } };
  ["pointerover","mouseover","mouseenter","mousemove"].forEach((t) => { try { el.dispatchEvent(mk(t)); } catch (_e) {} });"#;

const SEL_PRESENT_JS: &str = r#"try { Deno.core.ops.op_raze_exec_result(document.querySelector(__SEL__) ? "1" : "0"); } catch (_e) { try { Deno.core.ops.op_raze_exec_result("0"); } catch (_e2) {} }"#;

/// After an action, pump the event loop until the DOM stops changing for a
/// stability window — a reactive modal/route render lands with no network, which
/// `pump_to_quiesce` (fetch-activity based) would miss. Bounded by the capture
/// window; also exits once loads are done and the DOM is stable.
async fn pump_to_dom_settled(
    runtime: &mut JsRuntime,
    cap: &Rc<RefCell<CaptureState>>,
    cfg: &CaptureConfig,
) {
    let start = Instant::now();
    let hard_cap = Duration::from_millis(cfg.capture_window_ms);
    let stability = Duration::from_millis(cfg.quiesce_ms.max(120));
    let tick = Duration::from_millis(30);
    let inflight = cap.borrow().inflight.clone();
    let mut last_size: i64 = -1;
    let mut stable_since = Instant::now();
    loop {
        // A drained loop (no pending timers/promises/loads) can never mutate the
        // DOM further — microtasks are already flushed — so stop immediately once
        // any in-flight loads are done. Pages with a live interval stay `Pending`
        // and fall through to the stability-window / hard-cap checks below.
        let drained = matches!(poll_once(runtime), std::task::Poll::Ready(_));
        let size = probe_dom_size(runtime, cap);
        if size != last_size {
            last_size = size;
            stable_since = Instant::now();
        }
        if start.elapsed() >= hard_cap {
            break;
        }
        if drained && inflight.get() == 0 {
            break;
        }
        if stable_since.elapsed() >= stability && inflight.get() == 0 {
            break;
        }
        tokio::time::sleep(tick).await;
    }
}

/// Cheap DOM-size probe (element count) via a one-shot page-scope script that
/// hands the number back through `op_raze_exec_result`; `-1` if unavailable.
fn probe_dom_size(runtime: &mut JsRuntime, cap: &Rc<RefCell<CaptureState>>) -> i64 {
    cap.borrow_mut().exec_result = None;
    let _ = runtime.execute_script(
        "draco:interact:probe",
        r#"try { Deno.core.ops.op_raze_exec_result(String((document.getElementsByTagName("*") || []).length)); } catch (_e) {}"#
            .to_string(),
    );
    let _ = poll_once(runtime);
    let n = cap
        .borrow()
        .exec_result
        .as_deref()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(-1);
    cap.borrow_mut().exec_result = None;
    n
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
