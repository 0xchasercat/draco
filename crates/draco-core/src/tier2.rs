//! Tier 2 wiring (Slice 4): jail-hosted V8 capture → ranked replay.
//!
//! This is the supervisor half of the Tier 2 flow (the jailed-child half lives in
//! `draco-jail`). When the ladder reaches Tier 2 (Tiers 0/1 missed, it is not a
//! challenge, and `tier_max >= 2`), it drives a [`Tier2Capture`] seam to obtain a
//! [`CaptureResult`], then [`rank_and_replay`]s it:
//!
//! 1. **Capture.** The production seam ([`ProdTier2Capture`], `tier2` feature on)
//!    spawns the jail child via [`draco_jail::spawn_jail`] (or, when
//!    `config.no_jail`, an un-jailed dev fork of `draco __jail`), drives the IPC
//!    exchange (read `Ready`, write `Hydrate` with the Tier-0 HTML as the frame
//!    body, collect `Intercept`s until the terminal `Result`), and returns the
//!    captured requests + outcome. With the feature OFF, the seam is
//!    [`DisabledCapture`], which reports "built without tier2".
//! 2. **Rank + replay.** [`best_replayable`](crate::ranking::best_replayable)
//!    picks the most data-endpoint-like intercept that is *also* safe to replay;
//!    if it clears [`MIN_VIABLE_SCORE`](crate::ranking::MIN_VIABLE_SCORE) it is
//!    replayed through the [`PageFetcher`] seam. A JSON body finalizes `Success`
//!    / `SourceTier::RuntimeInterception`; otherwise the run is `Unsupported`.
//!    A state-changing request (unsafe method, not a GraphQL/JSON-RPC read) is
//!    withheld from replay unless `Config::allow_unsafe_replay` is set.
//!
//! ## Why a capture *seam*
//!
//! Splitting capture behind [`Tier2Capture`] keeps the ladder offline-testable:
//! unit tests drive `run_ladder` (and [`rank_and_replay`]) with a **mock** capture
//! that returns a canned `Vec` of intercepts, so no real child is forked. The
//! real-child path is exercised only by an `#[ignore]`d e2e test. The rank/replay
//! logic and the `CaptureResult` shape are V8-free and always compiled; only the
//! jail-spawning production seam is behind `#[cfg(feature = "tier2")]`.
//!
//! ## Blocking IPC inside an async ladder
//!
//! `spawn_jail` forks and the IPC uses blocking `UnixStream` reads/writes, so the
//! spawn+capture phase is synchronous. The ladder runs under a multi-thread tokio
//! runtime, so [`ProdTier2Capture`] pushes that phase onto
//! [`tokio::task::spawn_blocking`] rather than stalling a worker. The V8 isolate
//! (and its own current-thread tokio runtime) lives entirely in the **child**
//! process, so there is no nested-runtime hazard on the supervisor side. Only the
//! *replay* is async — it goes back through the normal `PageFetcher`.

use async_trait::async_trait;
use draco_types::{DiscoveredEndpoint, DracoError, JailKind, RuntimeOutcome};

use crate::fetcher::PageFetcher;
use crate::ranking::{best_candidate, best_replayable, Candidate};
use crate::Config;

// ===========================================================================
// Always-on: capture result shape, the seam, and rank+replay.
// (No dependency on draco-jail / V8; compiled in the lean build too.)
// ===========================================================================

/// The outcome of the capture phase: what the child intercepted, plus the
/// terminal runtime outcome it reported. Produced by a [`Tier2Capture`] seam and
/// consumed by [`rank_and_replay`].
#[derive(Debug, Clone)]
pub(crate) struct CaptureResult {
    /// Ranking views of every intercepted request, in capture order.
    pub candidates: Vec<Candidate>,
    /// The raw request body captured for each intercept (parallel to
    /// `candidates`; `None` when the request had no body). Kept out of
    /// [`Candidate`] so the ranking policy stays body-agnostic, but carried here
    /// so a POST winner (e.g. GraphQL) can be replayed faithfully.
    pub bodies: Vec<Option<Vec<u8>>>,
    /// The child's terminal runtime outcome.
    pub outcome: RuntimeOutcome,
    /// The achieved sandbox level the child reported (e.g.
    /// `"hardened: seccomp+netns+landlock"` or `"isolate: v8 no host bindings
    /// (macos)"`), surfaced as the `runtime.sandbox` trace step. `None` if the
    /// child did not report one (e.g. the offline mock capture).
    pub sandbox_level: Option<String>,
    /// The hydrated DOM the runtime serialized (`document.documentElement.
    /// outerHTML`), carried on the terminal `Result` frame body. `Some` when the
    /// isolate produced usable markup — the input to the render-then-Markdown
    /// escalation ([`crate::machine`]); `None` otherwise (empty body, or the
    /// offline mock capture).
    pub rendered_html: Option<String>,
}

/// The Tier 2 capture seam: given the page URL + Tier-0 HTML, produce a
/// [`CaptureResult`] (or a [`DracoError::Jail`]). Behind a trait so the ladder is
/// drivable offline with a mock that fabricates intercepts, no child forked.
#[async_trait]
pub(crate) trait Tier2Capture: Send + Sync {
    async fn capture(
        &self,
        url: &str,
        html: &[u8],
        resources: &[ScriptResource],
        config: &Config,
    ) -> Result<CaptureResult, DracoError>;
}

/// A supervisor-prefetched script subresource handed to the jailed child so the
/// (air-gapped) isolate can run external `<script src>` and resolve
/// `import`/`import()` for `type="module"` apps. `url` is the absolute resource
/// URL (the module-loader key); `source` is its raw bytes.
#[derive(Debug, Clone)]
pub(crate) struct ScriptResource {
    pub url: String,
    pub source: Vec<u8>,
}

/// A structured Tier 2 error, mapped to [`DracoError::Jail`] for the trace/result.
pub(crate) fn jail_error(reason: JailKind, detail: impl Into<String>) -> DracoError {
    DracoError::Jail {
        reason,
        detail: detail.into(),
    }
}

/// Why a rank+replay produced no data — used to make an `Ok(None)` outcome
/// observable in the ladder trace. Ranking-safety in particular must be visible:
/// the operator needs to know a viable endpoint was *withheld* for safety (and
/// that `--allow-unsafe-replay` would have replayed it), not merely that nothing
/// looked like data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoReplayReason {
    /// No intercept cleared [`MIN_VIABLE_SCORE`](crate::ranking::MIN_VIABLE_SCORE).
    NoViableCandidate,
    /// A viable top candidate existed but every viable candidate was an unsafe
    /// state-changing request skipped by the mutation-safety policy. Nothing was
    /// replayed; `--allow-unsafe-replay` would override this.
    UnsafeSkipped,
    /// The chosen winner replayed but the response was non-2xx or not JSON.
    ReplayNotJson,
}

impl NoReplayReason {
    /// A short, log-safe note for the `runtime.rank` trace step.
    pub(crate) fn note(self) -> &'static str {
        match self {
            NoReplayReason::NoViableCandidate => "no viable JSON endpoint among intercepts",
            NoReplayReason::UnsafeSkipped => {
                "viable endpoint withheld: unsafe state-changing method \
                 (use --allow-unsafe-replay to replay it)"
            }
            NoReplayReason::ReplayNotJson => "winner replay was non-2xx or not JSON",
        }
    }
}

/// Classify *why* [`rank_and_replay`] found nothing to replay, so the ladder can
/// record a precise trace note. Distinguishes "nothing viable" from "a viable
/// candidate was withheld purely for mutation-safety" (the latter only when
/// `allow_unsafe` is off). Pure + cheap — re-derived from the same ranking fns.
pub(crate) fn no_replay_reason(capture: &CaptureResult, target_url: &str) -> NoReplayReason {
    // If a viable candidate exists at all but none is replay-eligible under the
    // safe policy, the miss is a safety skip; otherwise nothing was viable.
    let has_viable = best_candidate(&capture.candidates, Some(target_url)).is_some();
    let has_safe_eligible = best_replayable(&capture.candidates, Some(target_url), false).is_some();
    if has_viable && !has_safe_eligible {
        NoReplayReason::UnsafeSkipped
    } else {
        NoReplayReason::NoViableCandidate
    }
}

/// Rank a capture result and replay the winner, producing the finalized
/// `(data, detail)` on success or `None` when nothing viable/eligible was found.
///
/// Replay-selection applies the **mutation-safety policy**
/// ([`best_replayable`]): a state-changing request (e.g. `POST /api/cart/add`)
/// is never blind-replayed. When the top-ranked candidate is unsafe it is
/// skipped in favor of the next eligible one; if nothing eligible remains this
/// returns `Ok(None)` and the ladder finalizes `Unsupported`. `allow_unsafe`
/// (from `Config::allow_unsafe_replay`) disables the screen so anything the
/// ranker picks is replayed.
///
/// Async because replay goes through the [`PageFetcher`] seam. Returns
/// `Ok(Some((json, detail)))` on a JSON-bodied winner, `Ok(None)` when no
/// candidate is viable+eligible, the replay was non-2xx, or the body is not
/// JSON, and `Err(..)` only on a replay transport failure. This is the
/// offline-unit-tested core (mock `PageFetcher` + a hand-built `CaptureResult`).
pub(crate) async fn rank_and_replay<F>(
    capture: &CaptureResult,
    target_url: &str,
    opts: &draco_net::SessionOpts,
    fetcher: &F,
    allow_unsafe: bool,
) -> Result<Option<(serde_json::Value, String)>, DracoError>
where
    F: PageFetcher + ?Sized,
{
    let Some((idx, score)) = best_replayable(&capture.candidates, Some(target_url), allow_unsafe)
    else {
        // Surface *why* on stderr so the reason is visible even outside the
        // trace (machine also records a precise note via `no_replay_reason`).
        let reason = no_replay_reason(capture, target_url);
        if reason == NoReplayReason::UnsafeSkipped {
            eprintln!("draco-core: Tier 2 {}", reason.note());
        }
        return Ok(None);
    };
    let winner = &capture.candidates[idx];

    // Build the replay spec, attaching the captured request body (base64) so a
    // POST winner replays faithfully. Header order is preserved verbatim.
    let mut spec = winner.to_request_spec();
    if let Some(body) = capture.bodies.get(idx).and_then(|b| b.as_ref()) {
        spec.body_b64 = Some(base64_encode(body));
    }

    let resp = fetcher.replay(&spec, opts).await?;

    if !is_2xx(resp.meta.status) {
        return Ok(None);
    }

    // Accept the body only if it actually parses as JSON — Tier 2's contract is a
    // JSON data endpoint.
    match serde_json::from_slice::<serde_json::Value>(&resp.body) {
        Ok(value) => Ok(Some((
            value,
            format!("{} {} (score {})", winner.method, winner.url, score),
        ))),
        Err(_) => Ok(None),
    }
}

/// Build the ranked endpoint catalog from a capture — the API-discovery surface.
///
/// Pure over the intercepted requests (no network): scores every captured
/// `fetch`/XHR with [`score_request`], marks each `replayable` (clears the
/// viability bar and is replay-safe, honoring `allow_unsafe`), and returns them
/// sorted by descending score (best-guess data API first). This is exactly the
/// information [`rank_and_replay`] uses to pick a winner, surfaced in full so a
/// caller can see — and choose to replay — every JSON endpoint behind a
/// client-rendered page.
pub(crate) fn discover_endpoints(
    capture: &CaptureResult,
    target_url: &str,
    allow_unsafe: bool,
) -> Vec<DiscoveredEndpoint> {
    use crate::ranking::{is_safe_method, score_request, MIN_VIABLE_SCORE};

    let mut out: Vec<DiscoveredEndpoint> = capture
        .candidates
        .iter()
        .map(|c| {
            let score = score_request(c, Some(target_url));
            let replayable =
                score >= MIN_VIABLE_SCORE && (is_safe_method(&c.method) || allow_unsafe);
            DiscoveredEndpoint {
                method: c.method.clone(),
                url: c.url.clone(),
                via: c.via,
                score,
                replayable,
                headers: c.headers.clone(),
            }
        })
        .collect();
    // Best-guess data API first; stable so equal scores keep capture order.
    out.sort_by_key(|e| std::cmp::Reverse(e.score));
    out
}

/// Capture seam used when the crate is built **without** the `tier2` feature:
/// there is no jail/runtime linked, so Tier 2 cannot run. The ladder records a
/// "built without tier2" note and finalizes `Unsupported`.
#[cfg(not(feature = "tier2"))]
pub(crate) struct DisabledCapture;

#[cfg(not(feature = "tier2"))]
#[async_trait]
impl Tier2Capture for DisabledCapture {
    async fn capture(
        &self,
        _url: &str,
        _html: &[u8],
        _resources: &[ScriptResource],
        _config: &Config,
    ) -> Result<CaptureResult, DracoError> {
        Err(jail_error(
            JailKind::Spawn,
            "built without tier2: Tier 2 (jail-hosted V8 capture) is not compiled in",
        ))
    }
}

// ===========================================================================
// Tunables + small helpers (always on — used by rank+replay and the prod seam).
// ===========================================================================

/// Safety cap on captured requests per Hydrate (mirrors the runtime default).
#[cfg(feature = "tier2")]
const MAX_INTERCEPTS: u32 = 64;

/// Derive the quiesce window from the capture window: ~1/6th, clamped to a
/// sensible `[150, 500]` ms band so a short capture window still gets a chance to
/// idle-detect while a long one does not wait excessively.
#[cfg(feature = "tier2")]
fn default_quiesce_ms(capture_window_ms: u64) -> u64 {
    (capture_window_ms / 6).clamp(150, 500)
}

/// Is this an HTTP 2xx status?
fn is_2xx(status: u16) -> bool {
    (200..300).contains(&status)
}

/// Standard-alphabet base64 encoder (RFC 4648, with `=` padding) for request
/// bodies handed to replay. Kept local so draco-core does not take a base64 dep;
/// draco-net decodes with the `base64` crate's `STANDARD` engine, which matches.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((triple >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

// ===========================================================================
// tier2-gated: the production capture seam (spawns the jail, drives IPC).
// ===========================================================================

#[cfg(feature = "tier2")]
mod prod {
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use draco_jail::frame::{self, FrameError};
    use draco_types::{DracoError, JailKind, JailToSupervisor, RuntimeOutcome, SupervisorToJail};

    use super::{
        default_quiesce_ms, jail_error, CaptureResult, Config, ScriptResource, Tier2Capture,
        MAX_INTERCEPTS,
    };
    use crate::ranking::Candidate;

    /// Production [`Tier2Capture`]: spawns the jail child and drives one Hydrate.
    pub(crate) struct ProdTier2Capture;

    #[async_trait]
    impl Tier2Capture for ProdTier2Capture {
        async fn capture(
            &self,
            url: &str,
            html: &[u8],
            resources: &[ScriptResource],
            config: &Config,
        ) -> Result<CaptureResult, DracoError> {
            // The spawn + blocking IPC exchange runs off the async worker pool.
            let url = url.to_string();
            let html = html.to_vec();
            let resources = resources.to_vec();
            let config = config.clone();
            tokio::task::spawn_blocking(move || capture_blocking(&url, &html, &resources, &config))
                .await
                .map_err(|e| {
                    jail_error(
                        JailKind::Spawn,
                        format!("capture task panicked/cancelled: {e}"),
                    )
                })?
        }
    }

    /// Spawn the jail child, drive one `Hydrate`, and collect the capture result.
    /// **Blocking** — always called from `spawn_blocking`. This is the one-shot
    /// path (CLI, and posture-overriding daemon requests): spawn a worker, run a
    /// single job, shut it down. The warm-pool path reuses a [`Worker`] across
    /// many jobs instead (see [`Tier2Pool`]).
    fn capture_blocking(
        url: &str,
        html: &[u8],
        resources: &[ScriptResource],
        config: &Config,
    ) -> Result<CaptureResult, DracoError> {
        let mut worker = Worker::spawn(config.no_jail, config.strict_sandbox)?;
        let result = worker.run_job(url, html, resources, config);
        // Always shut the one-shot worker down, whether the job succeeded or not,
        // so we never leak a child or a zombie.
        worker.shutdown();
        result
    }

    /// A warm, reusable jailed capture worker: a child process that has already
    /// paid the fork+exec + sandbox-arming + first snapshot cost and announced
    /// `Ready`, and can service many `Hydrate` jobs over its lifetime (each with
    /// a *fresh* isolate — see [`crate::tier2`] and the child loop in
    /// `draco_jail::runtime_payload`). Held idle in a [`Tier2Pool`] between jobs.
    ///
    /// The worker is `Send`: the V8 isolate lives in the *child* process; this
    /// struct only owns the supervisor's IPC endpoint (an fd) + the child pid, so
    /// it can move between blocking threads freely.
    struct Worker {
        handle: Handle,
        /// Jobs serviced so far — used by the pool to recycle after a bound.
        jobs_done: u32,
        /// The achieved sandbox posture, reported by the child as a prefixed
        /// `Log` right after `Ready` (at boot, once). Stashed here so it can be
        /// attached to *every* job's [`CaptureResult`], not just the first —
        /// a reused worker never re-reports it.
        sandbox_level: Option<String>,
    }

    impl Worker {
        /// Spawn a child with the given posture and consume its `Ready`
        /// handshake, leaving the worker ready to accept jobs. A hard EOF /
        /// protocol error here usually means the child died during sandbox setup
        /// (seccomp kill, or namespaces refused) — surfaced as a jail error.
        fn spawn(no_jail: bool, strict_sandbox: bool) -> Result<Worker, DracoError> {
            let mut handle = spawn_posture(no_jail, strict_sandbox)?;
            let ipc = handle.ipc_stream();
            match frame::read_jail_frame(ipc) {
                Ok(f) => match f.header {
                    JailToSupervisor::Ready { .. } => {}
                    JailToSupervisor::Error { reason, detail } => {
                        return Err(jail_error(
                            reason,
                            format!("child error before ready: {detail}"),
                        ));
                    }
                    other => {
                        return Err(jail_error(
                            JailKind::Protocol,
                            format!("expected Ready, got {other:?}"),
                        ));
                    }
                },
                Err(e) => return Err(map_frame_err(e, "reading Ready")),
            }
            Ok(Worker {
                handle,
                jobs_done: 0,
                sandbox_level: None,
            })
        }

        /// Drive one `Hydrate` job over this worker: stream the prefetched
        /// subresources, send the page, and collect intercepts until the terminal
        /// `Result`. Does **not** shut the worker down — the caller decides
        /// whether to reuse or retire it. On any IPC error the worker is
        /// considered poisoned and must not be reused (the pool drops it).
        fn run_job(
            &mut self,
            url: &str,
            html: &[u8],
            resources: &[ScriptResource],
            config: &Config,
        ) -> Result<CaptureResult, DracoError> {
            let ipc = self.handle.ipc_stream();

            // 1. Stream the pre-fetched script subresources (each source rides its
            //    frame body) so the isolate's module loader can serve
            //    `<script src>` and `import`/`import()` without the air-gapped
            //    child fetching.
            for res in resources {
                let frame = SupervisorToJail::Resource {
                    url: res.url.clone(),
                };
                frame::write_supervisor_frame(ipc, &frame, &res.source)
                    .map_err(|e| map_frame_err(e, "sending Resource"))?;
            }

            // 2. Send Hydrate with the page HTML as the frame body.
            let hydrate = SupervisorToJail::Hydrate {
                url: url.to_string(),
                capture_window_ms: config.capture_window_ms,
                quiesce_ms: default_quiesce_ms(config.capture_window_ms),
                max_intercepts: MAX_INTERCEPTS,
                stub_response_json: "{}".to_string(),
            };
            frame::write_supervisor_frame(ipc, &hydrate, html)
                .map_err(|e| map_frame_err(e, "sending Hydrate"))?;

            // 3. Collect Intercept frames until the terminal Result. On the FIRST
            //    job the child's boot-time sandbox-level Log is still queued ahead
            //    of the Result; stash it on the worker so later jobs inherit it.
            let mut candidates = Vec::new();
            let mut bodies = Vec::new();
            let mut rendered_html: Option<String> = None;
            let outcome: RuntimeOutcome = loop {
                let f = match frame::read_jail_frame(ipc) {
                    Ok(f) => f,
                    Err(FrameError::Eof) => {
                        let status = self
                            .handle
                            .try_reap_status()
                            .unwrap_or_else(|| "status unavailable".to_string());
                        return Err(jail_error(
                            JailKind::Protocol,
                            format!("child closed IPC before sending a Result ({status})"),
                        ));
                    }
                    Err(e) => return Err(map_frame_err(e, "collecting intercepts")),
                };
                match f.header {
                    JailToSupervisor::Intercept {
                        method,
                        url,
                        headers,
                        has_body,
                        via,
                        ..
                    } => {
                        candidates.push(Candidate {
                            method,
                            url,
                            headers,
                            via,
                        });
                        bodies.push(if has_body { Some(f.body) } else { None });
                    }
                    JailToSupervisor::Result { outcome, .. } => {
                        // The Result frame's body carries the hydrated DOM the
                        // runtime serialized (empty when there was none) — the raw
                        // material for the render-then-Markdown escalation.
                        if !f.body.is_empty() {
                            rendered_html = Some(String::from_utf8_lossy(&f.body).into_owned());
                        }
                        break outcome;
                    }
                    // A Log prefixed with the sandbox-level marker carries the
                    // achieved posture (boot-time, once); other Logs are
                    // diagnostic only.
                    JailToSupervisor::Log { msg, .. } => {
                        if let Some(level) = msg.strip_prefix(draco_jail::level::LEVEL_LOG_PREFIX) {
                            self.sandbox_level = Some(level.to_string());
                        }
                    }
                    JailToSupervisor::Error { reason, detail } => {
                        return Err(jail_error(reason, detail));
                    }
                    JailToSupervisor::Ready { .. } => {
                        return Err(jail_error(
                            JailKind::Protocol,
                            "unexpected second Ready frame",
                        ));
                    }
                }
            };

            self.jobs_done += 1;
            Ok(CaptureResult {
                candidates,
                bodies,
                outcome,
                sandbox_level: self.sandbox_level.clone(),
                rendered_html,
            })
        }

        /// Tell the child to shut down (best-effort — it may already be waiting on
        /// EOF) and reap it so we leave no zombie. Consumes the worker.
        fn shutdown(mut self) {
            let ipc = self.handle.ipc_stream();
            let _ = frame::write_supervisor_frame(ipc, &SupervisorToJail::Shutdown, &[]);
            self.handle.finish();
        }
    }

    /// A pool of warm [`Worker`]s for the daemon. Keeps children alive and idle
    /// between scrapes so each Tier 2 request skips the fork+exec + sandbox-arming
    /// + first snapshot cost (~130+ ms measured) and pays only the actual capture.
    ///
    /// Each job still runs in a **fresh isolate** inside a reused worker process
    /// (the child loops, building a new snapshot-restored `JsRuntime` per job), so
    /// there is no cross-scrape state, cookie, or DOM bleed — the reuse is of the
    /// expensive *process + sandbox*, never of the isolate. Workers are recycled
    /// after `max_jobs` (leak hygiene) and dropped (not reused) on any IPC error.
    ///
    /// Cloneable and `Send + Sync` (an `Arc` inner), so the daemon holds one and
    /// shares it across request handlers. Concurrency is expected to be bounded by
    /// the daemon's own permit gate; the pool spawns a fresh worker on a checkout
    /// miss rather than blocking.
    #[derive(Clone)]
    pub struct Tier2Pool {
        inner: Arc<PoolInner>,
    }

    struct PoolInner {
        /// Idle warm workers available for checkout (LIFO — reuse the hottest).
        idle: Mutex<Vec<Worker>>,
        /// Max workers to retain idle; extras returned over this are retired.
        max_idle: usize,
        /// Retire (and let the next demand respawn) a worker after this many jobs.
        max_jobs: u32,
        /// The sandbox posture the pooled workers were spawned with. A request
        /// whose posture differs falls back to a one-shot spawn.
        no_jail: bool,
        strict_sandbox: bool,
    }

    impl Tier2Pool {
        /// Create a pool. `size` is the number of workers kept warm/idle (a good
        /// default is the CPU count, which also caps concurrent isolates);
        /// `max_jobs` recycles a worker after that many captures. `no_jail` /
        /// `strict_sandbox` fix the workers' sandbox posture.
        pub fn new(size: usize, max_jobs: u32, no_jail: bool, strict_sandbox: bool) -> Self {
            Tier2Pool {
                inner: Arc::new(PoolInner {
                    idle: Mutex::new(Vec::new()),
                    max_idle: size.max(1),
                    max_jobs: max_jobs.max(1),
                    no_jail,
                    strict_sandbox,
                }),
            }
        }

        /// Retire all idle workers (best-effort). Call on daemon shutdown so
        /// children exit promptly instead of on EOF when the socket drops.
        pub fn shutdown(&self) {
            let workers = std::mem::take(&mut *self.inner.idle.lock().unwrap());
            for w in workers {
                w.shutdown();
            }
        }
    }

    impl PoolInner {
        /// Blocking: run one job on a pooled (or freshly spawned) worker, then
        /// return it to the pool or recycle it.
        fn run_pooled(
            &self,
            url: &str,
            html: &[u8],
            resources: &[ScriptResource],
            config: &Config,
        ) -> Result<CaptureResult, DracoError> {
            let mut worker = match self.idle.lock().unwrap().pop() {
                Some(w) => w,
                None => Worker::spawn(self.no_jail, self.strict_sandbox)?,
            };
            match worker.run_job(url, html, resources, config) {
                Ok(result) => {
                    self.return_or_recycle(worker);
                    Ok(result)
                }
                // A poisoned worker (IPC error / child died mid-job) must never
                // be reused — drop it; the next demand spawns a fresh one.
                Err(e) => {
                    worker.shutdown();
                    Err(e)
                }
            }
        }

        /// Return a healthy worker to the idle set, or retire it if it has hit the
        /// recycle bound or the idle set is already full.
        fn return_or_recycle(&self, worker: Worker) {
            if worker.jobs_done >= self.max_jobs {
                worker.shutdown();
                return;
            }
            let mut idle = self.idle.lock().unwrap();
            if idle.len() < self.max_idle {
                idle.push(worker);
            } else {
                drop(idle);
                worker.shutdown();
            }
        }
    }

    #[async_trait]
    impl Tier2Capture for Tier2Pool {
        async fn capture(
            &self,
            url: &str,
            html: &[u8],
            resources: &[ScriptResource],
            config: &Config,
        ) -> Result<CaptureResult, DracoError> {
            // A request that overrides the pool's sandbox posture can't use a
            // pooled worker (workers are spawned with a fixed posture) — fall back
            // to a dedicated one-shot capture for it.
            if config.no_jail != self.inner.no_jail
                || config.strict_sandbox != self.inner.strict_sandbox
            {
                return ProdTier2Capture.capture(url, html, resources, config).await;
            }

            let inner = self.inner.clone();
            let url = url.to_string();
            let html = html.to_vec();
            let resources = resources.to_vec();
            let config = config.clone();
            tokio::task::spawn_blocking(move || inner.run_pooled(&url, &html, &resources, &config))
                .await
                .map_err(|e| {
                    jail_error(
                        JailKind::Spawn,
                        format!("pooled capture task panicked/cancelled: {e}"),
                    )
                })?
        }
    }

    /// Owns the supervisor side of the child so `capture_blocking` is agnostic to
    /// whether it was jailed (`spawn_jail`) or un-jailed (dev `no_jail`).
    enum Handle {
        Jailed(draco_jail::JailHandle),
        #[cfg(target_os = "linux")]
        Unjailed(unjailed::UnjailedChild),
    }

    impl Handle {
        fn ipc_stream(&mut self) -> &mut UnixStream {
            match self {
                Handle::Jailed(h) => h.ipc(),
                #[cfg(target_os = "linux")]
                Handle::Unjailed(h) => h.ipc(),
            }
        }

        fn finish(self) {
            match self {
                Handle::Jailed(h) => {
                    reap_pid(h.pid());
                    drop(h);
                }
                #[cfg(target_os = "linux")]
                Handle::Unjailed(h) => h.finish(),
            }
        }

        #[cfg(target_os = "linux")]
        fn try_reap_status(&mut self) -> Option<String> {
            match self {
                Handle::Jailed(h) => reap_pid_status(h.pid()),
                Handle::Unjailed(_) => None,
            }
        }

        #[cfg(not(target_os = "linux"))]
        fn try_reap_status(&mut self) -> Option<String> {
            None
        }
    }

    /// Spawn the child with an explicit sandbox posture. Default: the
    /// OS-sandboxed (`hardened`) child via [`draco_jail::spawn_jail_with`],
    /// selecting the strict seccomp model when `strict_sandbox`. With `no_jail`,
    /// the isolate-only child (OS sandbox skipped; V8 still has no host bindings).
    fn spawn_posture(no_jail: bool, strict_sandbox: bool) -> Result<Handle, DracoError> {
        if no_jail {
            #[cfg(target_os = "linux")]
            {
                return unjailed::spawn().map(Handle::Unjailed);
            }
            #[cfg(not(target_os = "linux"))]
            {
                // On non-Linux there is no OS sandbox to skip: spawn_jail() is
                // already the isolate path, so `no_jail` is a no-op distinction.
                return draco_jail::spawn_jail()
                    .map(Handle::Jailed)
                    .map_err(|e| jail_error(e.reason, e.detail));
            }
        }
        draco_jail::spawn_jail_with(strict_sandbox)
            .map(Handle::Jailed)
            .map_err(|e| jail_error(e.reason, e.detail))
    }

    /// Best-effort reap of a child pid so a completed jail run leaves no zombie.
    #[cfg(target_os = "linux")]
    fn reap_pid(pid: i32) {
        let _ = reap_pid_status(pid);
    }

    #[cfg(target_os = "linux")]
    fn reap_pid_status(pid: i32) -> Option<String> {
        // SAFETY: waitpid on our own child pid; the child exits promptly after
        // emitting its Result and seeing Shutdown/EOF.
        unsafe {
            let mut status: libc::c_int = 0;
            let r = libc::waitpid(pid, &mut status, 0);
            if r < 0 {
                return Some(format!(
                    "waitpid failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if libc::WIFSIGNALED(status) {
                return Some(format!("child signaled {}", libc::WTERMSIG(status)));
            }
            if libc::WIFEXITED(status) {
                return Some(format!("child exited {}", libc::WEXITSTATUS(status)));
            }
            Some(format!("wait status 0x{status:x}"))
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn reap_pid(_pid: i32) {
        // Non-Linux degraded spawn manages child lifetime itself.
    }

    /// Map a frame-codec error to the most specific [`DracoError::Jail`].
    fn map_frame_err(e: FrameError, ctx: &str) -> DracoError {
        match e {
            FrameError::Eof => jail_error(JailKind::Protocol, format!("{ctx}: unexpected EOF")),
            FrameError::Io(io) => jail_error(
                JailKind::Killed,
                format!("{ctx}: IPC I/O error (child likely killed): {io}"),
            ),
            other => jail_error(JailKind::Protocol, format!("{ctx}: {other}")),
        }
    }

    // -----------------------------------------------------------------------
    // Isolate-mode path (`--no-jail` on Linux): fork + re-exec `draco __jail`
    // WITHOUT the OS sandbox (netns/seccomp/Landlock). Tier 2 still hosts V8 with
    // no host-capability bindings, so page JS stays contained; this only skips
    // the defense-in-depth OS layer. Linux-only (elsewhere the isolate path is the
    // normal spawn).
    // -----------------------------------------------------------------------
    #[cfg(target_os = "linux")]
    mod unjailed {
        use std::ffi::CString;
        use std::os::fd::{IntoRawFd, OwnedFd};
        use std::os::unix::net::UnixStream;

        use draco_types::JailKind;

        use super::super::jail_error;
        use draco_types::DracoError;

        /// Fd the child inherits its IPC socket on — matches draco-jail's contract.
        const JAIL_IPC_FD: i32 = 3;

        /// A forked isolate-mode `draco __jail` child + the supervisor IPC end.
        pub(super) struct UnjailedChild {
            pid: i32,
            ipc: UnixStream,
        }

        impl UnjailedChild {
            pub(super) fn ipc(&mut self) -> &mut UnixStream {
                &mut self.ipc
            }

            pub(super) fn finish(self) {
                // SAFETY: reap our own child; it exits after Shutdown/EOF.
                unsafe {
                    let mut status: libc::c_int = 0;
                    libc::waitpid(self.pid, &mut status, 0);
                }
                drop(self.ipc);
            }
        }

        /// Spawn `draco __jail` in isolate mode: socketpair → fork → (child) dup
        /// socket onto fd 3 and exec self; (parent) keep the supervisor end.
        ///
        /// Intentionally skips the OS sandbox (netns/seccomp/Landlock) — the
        /// `--no-jail` path. The re-exec target is the running executable, whose
        /// `__jail` hook routes into `draco_jail::run_jail_child`. We set
        /// [`draco_jail::JAIL_NO_SANDBOX_ENV`] in the child before exec so that
        /// entry skips arming the OS sandbox and runs the capture payload directly
        /// (V8 still has no host-capability bindings).
        pub(super) fn spawn() -> Result<UnjailedChild, DracoError> {
            // The user explicitly opted out of OS-level hardening on a platform
            // where it was available. One concise, non-alarming line noting the
            // achieved posture — Tier 2 still runs V8 with no host bindings.
            eprintln!(
                "draco-core: --no-jail set; running Tier 2 in isolate mode (V8, no host \
                 bindings) without the OS sandbox (seccomp/netns/Landlock)."
            );

            let (sup, child) = UnixStream::pair()
                .map_err(|e| jail_error(JailKind::Spawn, format!("socketpair: {e}")))?;

            let exe = std::env::current_exe()
                .map_err(|e| jail_error(JailKind::Spawn, format!("current_exe: {e}")))?;
            let exe_c = CString::new(exe.as_os_str().as_encoded_bytes())
                .map_err(|e| jail_error(JailKind::Spawn, format!("exe path has NUL: {e}")))?;
            let jail_arg =
                CString::new("__jail").expect("static literal \"__jail\" contains no NUL byte");
            // Marker name/value for the un-jailed child (set via setenv before
            // exec). Allocated pre-fork so the child arm is allocation-free.
            let env_name = CString::new(draco_jail::JAIL_NO_SANDBOX_ENV)
                .expect("env var name contains no NUL byte");
            let env_val = CString::new("1").expect("static \"1\" contains no NUL byte");

            let child_fd: OwnedFd = child.into();
            let child_raw = child_fd.into_raw_fd();

            // SAFETY: between fork and exec we call only async-signal-safe libc
            // functions (dup2/close/setenv/execv/_exit) and touch no Rust runtime
            // state. `setenv` is safe post-fork in the single-threaded child.
            match unsafe { libc::fork() } {
                -1 => {
                    // SAFETY: child_raw is a valid fd we still own here.
                    unsafe { libc::close(child_raw) };
                    drop(sup);
                    Err(jail_error(JailKind::Spawn, "fork failed"))
                }
                0 => {
                    // SAFETY: async-signal-safe calls only; abort hard on any
                    // failure so we never run supervisor code in the child.
                    unsafe {
                        if child_raw != JAIL_IPC_FD {
                            if libc::dup2(child_raw, JAIL_IPC_FD) < 0 {
                                libc::_exit(126);
                            }
                            libc::close(child_raw);
                        } else if libc::fcntl(child_raw, libc::F_SETFD, 0) < 0 {
                            libc::_exit(126);
                        }
                        // Tell the re-exec'd child to skip the sandbox.
                        if libc::setenv(env_name.as_ptr(), env_val.as_ptr(), 1) < 0 {
                            libc::_exit(125);
                        }
                        let argv = [exe_c.as_ptr(), jail_arg.as_ptr(), std::ptr::null()];
                        libc::execv(exe_c.as_ptr(), argv.as_ptr());
                        // exec failed; diverge so this arm has type `!` (coerces
                        // to the `Result` the other arms produce).
                        libc::_exit(127)
                    }
                }
                pid => {
                    // Parent: close the child end, keep the supervisor end.
                    // SAFETY: child_raw belongs to the child now; close our copy.
                    unsafe { libc::close(child_raw) };
                    Ok(UnjailedChild { pid, ipc: sup })
                }
            }
        }
    }
}

#[cfg(feature = "tier2")]
pub(crate) use prod::ProdTier2Capture;
/// The warm worker pool — public so the daemon can hold one and route scrapes
/// through it (via [`crate::extract_with_pool`]).
#[cfg(feature = "tier2")]
pub use prod::Tier2Pool;

/// Lean-build stub of the warm pool: with no `tier2` feature there is no
/// jail/runtime linked, so the pool cannot host V8. It exists only so the daemon
/// compiles and links the same way in both builds; its capture path finalizes
/// `Unsupported`, exactly like [`DisabledCapture`]. Constructor args are ignored.
#[cfg(not(feature = "tier2"))]
#[derive(Clone)]
pub struct Tier2Pool;

#[cfg(not(feature = "tier2"))]
impl Tier2Pool {
    pub fn new(_size: usize, _max_jobs: u32, _no_jail: bool, _strict_sandbox: bool) -> Self {
        Tier2Pool
    }
    pub fn shutdown(&self) {}
}

#[cfg(not(feature = "tier2"))]
#[async_trait]
impl Tier2Capture for Tier2Pool {
    async fn capture(
        &self,
        _url: &str,
        _html: &[u8],
        _resources: &[ScriptResource],
        _config: &Config,
    ) -> Result<CaptureResult, DracoError> {
        Err(jail_error(
            JailKind::Spawn,
            "built without tier2: Tier 2 (jail-hosted V8 capture) is not compiled in",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::MockFetcher;
    use draco_types::InterceptVia;
    use serde_json::json;

    fn cand(url: &str, accept_json: bool, via: InterceptVia) -> Candidate {
        Candidate {
            method: "GET".into(),
            url: url.into(),
            headers: if accept_json {
                vec![("accept".into(), "application/json".into())]
            } else {
                Vec::new()
            },
            via,
        }
    }

    fn capture_of(candidates: Vec<Candidate>) -> CaptureResult {
        let bodies = vec![None; candidates.len()];
        CaptureResult {
            candidates,
            bodies,
            outcome: RuntimeOutcome::Quiesced,
            sandbox_level: None,
            rendered_html: None,
        }
    }

    #[test]
    fn discover_endpoints_ranks_and_flags_replayable() {
        use draco_types::InterceptVia;
        let target = "https://shop.example/";
        // A real data API (accept: json, api path), an analytics beacon, and a
        // static asset — the ranker should order and flag them accordingly.
        let capture = capture_of(vec![
            cand(
                "https://shop.example/api/products",
                true,
                InterceptVia::Fetch,
            ),
            cand(
                "https://analytics.example/collect",
                false,
                InterceptVia::Xhr,
            ),
        ]);
        let eps = discover_endpoints(&capture, target, false);
        assert_eq!(eps.len(), 2);
        // Highest score first — the JSON API leads.
        assert_eq!(eps[0].url, "https://shop.example/api/products");
        assert!(eps[0].score >= eps[1].score, "not sorted by score desc");
        // The viable same-origin JSON GET is replayable; the low-scored
        // cross-origin beacon is not.
        assert!(eps[0].replayable, "data API should be replayable: {eps:?}");
        assert!(
            !eps[1].replayable,
            "analytics beacon should not be replayable: {eps:?}"
        );
        // Fields carried faithfully for a would-be replay.
        assert_eq!(eps[0].method, "GET");
        assert_eq!(eps[0].via, InterceptVia::Fetch);
        assert!(eps[0].headers.iter().any(|(k, _)| k == "accept"));
    }

    #[test]
    fn quiesce_is_clamped() {
        // Only meaningful with the prod seam compiled, but the helper is
        // tier2-gated, so guard the assertion behind the same cfg.
        #[cfg(feature = "tier2")]
        {
            assert_eq!(default_quiesce_ms(0), 150);
            assert_eq!(default_quiesce_ms(600), 150); // 100 → floored at 150
            assert_eq!(default_quiesce_ms(1_800), 300); // 300
            assert_eq!(default_quiesce_ms(9_000), 500); // 1500 → ceiled at 500
        }
    }

    #[test]
    fn base64_matches_standard_alphabet() {
        // Vectors from RFC 4648 §10.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(br#"{"q":1}"#), "eyJxIjoxfQ==");
    }

    // ---- rank_and_replay: the offline-unit-tested core (mock PageFetcher) ----

    #[tokio::test]
    async fn rank_and_replay_picks_winner_and_returns_json() {
        let capture = capture_of(vec![
            cand("https://cdn.example.com/app.js", false, InterceptVia::Fetch), // asset
            cand(
                "https://api.example.com/v1/items?q=1",
                true,
                InterceptVia::Fetch,
            ), // strong
        ]);
        let fetcher =
            MockFetcher::ok_html(200, "<ignored>").with_replay_json(200, json!({ "price": 42 }));
        let opts = draco_net::SessionOpts::default();

        let out = rank_and_replay(&capture, "https://api.example.com/", &opts, &fetcher, false)
            .await
            .unwrap();
        let (data, detail) = out.expect("a viable winner");
        assert_eq!(data, json!({ "price": 42 }));
        assert!(detail.contains("/v1/items"), "detail: {detail}");
        assert_eq!(fetcher.replay_calls(), 1);
    }

    #[tokio::test]
    async fn rank_and_replay_none_when_all_junk() {
        let capture = capture_of(vec![
            cand("https://cdn.example.com/app.js", false, InterceptVia::Fetch),
            cand(
                "https://www.google-analytics.com/collect",
                false,
                InterceptVia::Xhr,
            ),
        ]);
        let fetcher = MockFetcher::ok_html(200, "<ignored>");
        let opts = draco_net::SessionOpts::default();

        let out = rank_and_replay(&capture, "https://api.example.com/", &opts, &fetcher, false)
            .await
            .unwrap();
        assert!(out.is_none(), "no viable candidate should yield None");
        // Replay must not even be attempted when nothing is viable.
        assert_eq!(fetcher.replay_calls(), 0);
    }

    #[tokio::test]
    async fn rank_and_replay_none_when_replay_body_not_json() {
        let capture = capture_of(vec![cand(
            "https://api.example.com/v1/items",
            true,
            InterceptVia::Fetch,
        )]);
        // Replay returns 200 but an HTML (non-JSON) body.
        let fetcher = MockFetcher::ok_html(200, "<ignored>").with_replay_status(200);
        let opts = draco_net::SessionOpts::default();

        let out = rank_and_replay(&capture, "https://api.example.com/", &opts, &fetcher, false)
            .await
            .unwrap();
        assert!(
            out.is_none(),
            "non-JSON replay body must not finalize success"
        );
    }

    #[tokio::test]
    async fn rank_and_replay_none_on_non_2xx() {
        let capture = capture_of(vec![cand(
            "https://api.example.com/v1/items",
            true,
            InterceptVia::Fetch,
        )]);
        let fetcher = MockFetcher::ok_html(200, "<ignored>").with_replay_status(500);
        let opts = draco_net::SessionOpts::default();
        let out = rank_and_replay(&capture, "https://api.example.com/", &opts, &fetcher, false)
            .await
            .unwrap();
        assert!(out.is_none(), "5xx replay must not finalize success");
    }

    #[tokio::test]
    async fn rank_and_replay_propagates_replay_error() {
        use draco_types::NetKind;
        let capture = capture_of(vec![cand(
            "https://api.example.com/v1/items",
            true,
            InterceptVia::Fetch,
        )]);
        let fetcher = crate::testutil::err_replay_fetcher(DracoError::Network {
            reason: NetKind::Timeout,
            detail: "replay timed out".into(),
        });
        let opts = draco_net::SessionOpts::default();
        let err = rank_and_replay(&capture, "https://api.example.com/", &opts, &fetcher, false)
            .await
            .unwrap_err();
        assert!(matches!(err, DracoError::Network { .. }));
    }

    // ---- mutation-safety at replay time ----

    /// Build a capture with a single unsafe state-changing POST (same-origin,
    /// api path, JSON *intent* via Accept → viable at 23, but NOT a read: no
    /// query-path marker and no JSON request content-type). The parallel body is
    /// present so the `allow_unsafe` replay path exercises body attachment too.
    fn unsafe_post_capture() -> CaptureResult {
        CaptureResult {
            candidates: vec![Candidate {
                method: "POST".into(),
                url: "https://shop.example.com/api/cart/add".into(),
                headers: vec![("accept".into(), "application/json".into())],
                via: InterceptVia::Fetch,
            }],
            bodies: vec![Some(br#"{"sku":"ABC","qty":1}"#.to_vec())],
            outcome: RuntimeOutcome::Quiesced,
            sandbox_level: None,
            rendered_html: None,
        }
    }

    #[tokio::test]
    async fn rank_and_replay_withholds_unsafe_post_by_default() {
        let capture = unsafe_post_capture();
        // Replay would 200-with-JSON *if* attempted — proving the None is a
        // safety withhold, not a replay miss.
        let fetcher =
            MockFetcher::ok_html(200, "<ignored>").with_replay_json(200, json!({ "ok": true }));
        let opts = draco_net::SessionOpts::default();

        let out = rank_and_replay(
            &capture,
            "https://shop.example.com/",
            &opts,
            &fetcher,
            false,
        )
        .await
        .unwrap();
        assert!(
            out.is_none(),
            "an unsafe-POST-only capture must yield Ok(None) by default"
        );
        assert_eq!(
            fetcher.replay_calls(),
            0,
            "the unsafe POST must never be replayed in safe mode"
        );
        // And the miss is classified as a safety skip (observable in the trace).
        assert_eq!(
            no_replay_reason(&capture, "https://shop.example.com/"),
            NoReplayReason::UnsafeSkipped
        );
    }

    #[tokio::test]
    async fn rank_and_replay_replays_unsafe_post_when_allowed() {
        let capture = unsafe_post_capture();
        let fetcher = MockFetcher::ok_html(200, "<ignored>")
            .with_replay_json(200, json!({ "cart": { "count": 1 } }));
        let opts = draco_net::SessionOpts::default();

        let out = rank_and_replay(&capture, "https://shop.example.com/", &opts, &fetcher, true)
            .await
            .unwrap();
        let (data, detail) = out.expect("allow_unsafe must replay the winner");
        assert_eq!(data, json!({ "cart": { "count": 1 } }));
        assert!(detail.contains("/api/cart/add"), "detail: {detail}");
        assert_eq!(
            fetcher.replay_calls(),
            1,
            "the winner must be replayed once"
        );
    }
}
