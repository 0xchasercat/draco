//! Tier 2 wiring: in-process V8 capture → ranked replay.
//!
//! When the ladder reaches Tier 2 (Tiers 0/1 missed, it is not a challenge, and
//! `tier_max >= 2`), it drives a [`Tier2Capture`] seam to obtain a
//! [`CaptureResult`], then [`rank_and_replay`]s it:
//!
//! 1. **Capture.** The production seam ([`ProdTier2Capture`], `tier2` feature on)
//!    runs the page **in-process** via [`draco_runtime::run_capture`]: a fresh V8
//!    isolate (happy-dom, no host-capability bindings) hydrates the Tier-0 HTML
//!    and every request it makes is recorded. The isolate pulls its own code —
//!    external `<script src>`, `import()`, and webpack/Next chunks — through an
//!    async fetcher backed by `draco-net` + the immutable chunk cache, so those
//!    loads fan out concurrently on its event loop. With the feature OFF, the
//!    seam is [`DisabledCapture`], which reports "built without tier2".
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
//! that returns a canned `Vec` of intercepts, so no isolate is booted. The
//! rank/replay logic and the `CaptureResult` shape are V8-free and always
//! compiled; only the isolate-hosting production seam is behind
//! `#[cfg(feature = "tier2")]`.
//!
//! ## Driving `!Send` V8 from an async ladder
//!
//! [`draco_runtime::run_capture`] owns a current-thread tokio runtime and drives a
//! `!Send` `JsRuntime`, so it cannot run on an async worker. The ladder runs under
//! a multi-thread runtime, so [`ProdTier2Capture`] pushes each capture onto
//! [`tokio::task::spawn_blocking`] — the isolate lives and dies on that one
//! blocking thread. The isolate's own script/chunk fetches are async (real
//! `draco-net` sockets) driven on its current-thread runtime; only the final
//! *replay* goes back through the ladder's normal async `PageFetcher`.

use async_trait::async_trait;
use draco_types::{DiscoveredEndpoint, DracoError, JailKind, RuntimeOutcome};

use crate::fetcher::PageFetcher;
use crate::ranking::{best_candidate, best_replayable, Candidate};
use crate::Config;

// ===========================================================================
// Always-on: capture result shape, the seam, and rank+replay.
// (No dependency on V8; compiled in the lean build too.)
// ===========================================================================

/// The outcome of the capture phase: what the isolate intercepted, plus the
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
    /// The capture's terminal runtime outcome.
    pub outcome: RuntimeOutcome,
    /// The achieved containment posture (e.g. `"isolate: in-process v8 (no host
    /// bindings)"`), surfaced as the `runtime.sandbox` trace step. `None` if the
    /// seam did not report one (e.g. the offline mock capture).
    pub sandbox_level: Option<String>,
    /// The hydrated DOM the runtime serialized (`document.documentElement.
    /// outerHTML`). `Some` when the isolate produced usable markup — the input to
    /// the render-then-Markdown escalation ([`crate::machine`]); `None` otherwise
    /// (nothing usable, or the offline mock capture).
    pub rendered_html: Option<String>,
    /// Page-side runtime diagnostics (glue-swallowed exceptions, console.error
    /// lines, page-script throws). Bounded runtime-side; surfaced as
    /// `runtime.log` trace steps when [`Config::runtime_log`](crate::Config)
    /// asks for them.
    pub logs: Vec<String>,
}

/// Which fetch policy the isolate runs the page's own data requests under.
///
///   * [`Observe`](CaptureMode::Observe) — `fetch`/XHR are recorded and answered
///     with a cheap synthetic stub; nothing is fetched live. This is `discover`'s
///     model (enumerate endpoints, rank + replay ourselves) and the fast path for
///     SSR/hybrid `scrape`, where the shell already carries content.
///   * [`Render`](CaptureMode::Render) — the page's *safe* data requests are
///     fetched live via `draco-net` so a pure-CSR shell's content materializes
///     before the DOM is serialized. The ladder selects it when static extraction
///     sees a thin/CSR shell (or `--force-render` forces it).
///
/// `#[allow(dead_code)]`: in a `--no-default-features` (V8-free) build the variants
/// are never constructed (the lean Tier 2 branch never captures), but the type is
/// still part of the always-compiled [`Tier2Capture`] signature.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureMode {
    Observe,
    Render,
}

/// The Tier 2 capture seam: given the page URL + Tier-0 HTML and a [`CaptureMode`],
/// produce a [`CaptureResult`] (or a [`DracoError::Jail`]). Behind a trait so the
/// ladder is drivable offline with a mock that fabricates intercepts, no isolate
/// booted. There is no resource list to pass: the isolate pulls the code it needs
/// itself, on demand and concurrently, through its async fetcher.
#[async_trait]
pub(crate) trait Tier2Capture: Send + Sync {
    async fn capture(
        &self,
        url: &str,
        html: &[u8],
        config: &Config,
        opts: &draco_net::SessionOpts,
        mode: CaptureMode,
    ) -> Result<CaptureResult, DracoError>;
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
    use crate::ranking::{
        is_read_style_post, is_safe_method, is_streaming_endpoint, score_request, MIN_VIABLE_SCORE,
    };

    let mut out: Vec<DiscoveredEndpoint> = capture
        .candidates
        .iter()
        .map(|c| {
            let score = score_request(c, Some(target_url));
            // Must mirror `best_replayable`'s eligibility EXACTLY, or the catalog's
            // `replayable` flag contradicts what rank/replay actually does (a
            // read-style POST like a GraphQL/JSON-RPC query — or thrill's
            // `POST /tickets` with a JSON content-type — is replayed, so it must be
            // flagged replayable here too). Streaming endpoints are reported but
            // never replayable: replaying an infinite stream hangs until timeout.
            let replayable = score >= MIN_VIABLE_SCORE
                && !is_streaming_endpoint(c)
                && (allow_unsafe || is_safe_method(&c.method) || is_read_style_post(c));
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
/// there is no runtime (V8) linked, so Tier 2 cannot run. The ladder records a
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
        _config: &Config,
        _opts: &draco_net::SessionOpts,
        _mode: CaptureMode,
    ) -> Result<CaptureResult, DracoError> {
        Err(jail_error(
            JailKind::Spawn,
            "built without tier2: Tier 2 (in-process V8 capture) is not compiled in",
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
const MAX_SUPERVISOR_CAPTURE_WINDOW_MS: u64 = 2_500;

fn effective_capture_window_ms(requested: u64) -> u64 {
    requested.min(MAX_SUPERVISOR_CAPTURE_WINDOW_MS)
}

/// Per-fetch timeout clamp for script **subresources** (the isolate's on-demand
/// script/module/chunk loads). The session's own timeout (default 30 s) is sized
/// for the page fetch; letting a single hung chunk CDN pin a capture for that
/// long multiplies into the tens of seconds the capture-window clamp was
/// supposed to prevent. A chunk that can't answer in 2.5 s is treated as
/// missing — the page sees the same `onerror` shape as a network miss. (The
/// capture window itself independently bounds total job time; loads still in
/// flight when it closes are simply abandoned with the isolate.)
#[cfg(feature = "tier2")]
pub(crate) const SUBRESOURCE_FETCH_TIMEOUT_MS: u64 = 2_500;

/// Derive the [`draco_net::SessionOpts`] used for script subresource fetches
/// from the session's own: per-fetch timeout clamped to
/// [`SUBRESOURCE_FETCH_TIMEOUT_MS`], and the politeness `delay_ms` dropped —
/// browsers burst-load a page's chunks over pooled connections, and a per-host
/// delay would serialize the isolate's concurrent load fan-out. The session's
/// cookie jar is preserved (chunk CDNs behind Cloudflare need the page's
/// `__cf_bm` cookie).
#[cfg(feature = "tier2")]
pub(crate) fn subresource_opts(opts: &draco_net::SessionOpts) -> draco_net::SessionOpts {
    let mut o = opts.clone();
    o.timeout_ms = o.timeout_ms.min(SUBRESOURCE_FETCH_TIMEOUT_MS);
    o.delay_ms = 0;
    o
}

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
// tier2-gated: the production capture seam (in-process V8, async net fetches).
// ===========================================================================

#[cfg(feature = "tier2")]
mod prod {
    use std::sync::Arc;

    use async_trait::async_trait;
    use draco_types::{DracoError, HttpRequestSpec, InterceptVia, JailKind};

    use super::{
        base64_encode, default_quiesce_ms, effective_capture_window_ms, jail_error,
        subresource_opts, CaptureMode, CaptureResult, Config, Tier2Capture, MAX_INTERCEPTS,
    };
    use crate::chunk_cache::ChunkCache;
    use crate::ranking::{
        is_analytics_url, is_read_style_post, is_safe_method, is_streaming_endpoint, Candidate,
    };

    /// In-process async source of script / module / chunk bytes for the isolate:
    /// a pooled `draco-net` fetch (with the job's subresource posture + shared
    /// cookie jar) fronted by the immutable [`ChunkCache`]. Implements
    /// [`draco_runtime::ScriptFetcher`] so the isolate `.await`s it directly on its
    /// own event loop — many chunk/module loads kicked off in a burst fan out
    /// concurrently over Tokio instead of serializing one blocking round-trip at a
    /// time (the whole point of retiring the jail's per-chunk IPC).
    struct NetScriptFetcher {
        opts: draco_net::SessionOpts,
        cache: Arc<ChunkCache>,
    }

    impl draco_runtime::ScriptFetcher for NetScriptFetcher {
        // Return type spelled out (rather than the `futures::future::LocalBoxFuture`
        // alias the trait declares) so draco-core needs no `futures` dependency —
        // the alias is transparently `Pin<Box<dyn Future + 'a>>`.
        fn fetch<'a>(
            &'a self,
            url: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<u8>>> + 'a>> {
            Box::pin(async move {
                // Code chunks carry content-hashed filenames, so a URL is an
                // immutable key: serve from cache when present (this is what makes a
                // repeat scrape of a site sub-second).
                if let Some(bytes) = self.cache.get(url) {
                    return Some(bytes);
                }
                match draco_net::fetch_target(url, &self.opts).await {
                    Ok(resp) if (200..300).contains(&resp.meta.status) => {
                        let bytes = resp.body.to_vec();
                        self.cache.put(url, &bytes);
                        Some(bytes)
                    }
                    // 403 → bot-wall, 404 → moved/renamed chunk, others verbatim:
                    // a miss rejects exactly that load (the module loader / script
                    // onerror), never poisons the graph. Never cache non-2xx.
                    _ => None,
                }
            })
        }
    }

    /// In-process async responder for the page's own data requests in **Render
    /// mode**. Issues the safe ones live through the pooled `draco-net` client
    /// (with the job's subresource posture + shared cookie jar) and returns the
    /// real `{status, headers, body}`; declines the rest, returning `None` so the
    /// runtime falls back to its built-in synthetic stub.
    ///
    /// The stub-vs-live line reuses the ranking module's mutation-safety policy so
    /// it matches replay exactly: never live-execute a streaming endpoint (it would
    /// hang the capture window), an analytics/tracking beacon (waste + privacy
    /// leak), or an unsafe state-changing method — but DO run GET/HEAD and
    /// read-style POST/PUT (GraphQL/JSON-RPC), which is how a CSR SPA fetches its
    /// layout data. `allow_unsafe` (from `--allow-unsafe-replay`) opens the gate to
    /// any method.
    struct NetApiFetcher {
        opts: draco_net::SessionOpts,
        allow_unsafe: bool,
    }

    impl NetApiFetcher {
        /// Eligibility mirror of `ranking::best_replayable`'s policy: safe to send
        /// live iff not a stream, not analytics, and (a safe method OR a read-style
        /// POST/PUT OR `allow_unsafe`).
        fn may_fetch_live(&self, cand: &Candidate) -> bool {
            if is_streaming_endpoint(cand) || is_analytics_url(&cand.url) {
                return false;
            }
            self.allow_unsafe || is_safe_method(&cand.method) || is_read_style_post(cand)
        }
    }

    impl draco_runtime::ApiFetcher for NetApiFetcher {
        fn fetch<'a>(
            &'a self,
            req: &'a draco_runtime::ApiRequest,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Option<draco_runtime::ApiResponse>> + 'a>,
        > {
            Box::pin(async move {
                // View the request as a Candidate to reuse the ranking policy verbatim.
                let cand = Candidate {
                    method: req.method.clone(),
                    url: req.url.clone(),
                    headers: req.headers.clone(),
                    via: InterceptVia::Fetch,
                };
                if !self.may_fetch_live(&cand) {
                    return None; // → the runtime's built-in synthetic stub
                }
                let spec = HttpRequestSpec {
                    method: req.method.clone(),
                    url: req.url.clone(),
                    headers: req.headers.clone(),
                    body_b64: req.body.as_ref().map(|b| base64_encode(b)),
                };
                match draco_net::replay(&spec, &self.opts).await {
                    // Return the REAL status/headers/body — including a non-2xx such
                    // as a 403 JSON, so the page's router runs its logged-out path
                    // instead of throwing on a synthetic 404.
                    Ok(resp) => Some(draco_runtime::ApiResponse {
                        status: resp.meta.status,
                        headers: resp.meta.headers.clone(),
                        body: resp.body.to_vec(),
                    }),
                    // Transport failure → fall back to the stub; hydration proceeds.
                    Err(_) => None,
                }
            })
        }
    }

    /// Translate the ladder's [`Config`] into the isolate's capture knobs. The
    /// requested capture window is clamped ([`effective_capture_window_ms`]); the
    /// stub body is `"[]"` (an empty JSON array — the shape most page code
    /// `.flatMap`/`.map`s over without throwing, so hydration proceeds).
    fn capture_config(config: &Config) -> draco_runtime::CaptureConfig {
        let capture_window_ms = effective_capture_window_ms(config.capture_window_ms);
        draco_runtime::CaptureConfig {
            capture_window_ms,
            quiesce_ms: default_quiesce_ms(capture_window_ms),
            max_intercepts: MAX_INTERCEPTS,
            stub_response_json: "[]".to_string(),
        }
    }

    /// Map a [`draco_runtime::CaptureReport`] onto the ladder's [`CaptureResult`]
    /// (rank/replay's V8-free input). Header order is preserved verbatim (it is
    /// fingerprint-relevant to replay).
    fn to_capture_result(report: draco_runtime::CaptureReport) -> CaptureResult {
        let mut candidates = Vec::with_capacity(report.requests.len());
        let mut bodies = Vec::with_capacity(report.requests.len());
        for r in report.requests {
            candidates.push(Candidate {
                method: r.method,
                url: r.url,
                headers: r.headers,
                via: r.via,
            });
            bodies.push(r.body);
        }
        CaptureResult {
            candidates,
            bodies,
            outcome: report.outcome,
            // The containment story is now purely the isolate: page JS runs with no
            // host-capability bindings, in-process. Surfaced as `runtime.sandbox`.
            sandbox_level: Some("isolate: in-process v8 (no host bindings)".to_string()),
            rendered_html: report.rendered_html,
            logs: report.logs,
        }
    }

    /// Run one capture **in-process**. Blocking — always invoked from
    /// `spawn_blocking`, because [`draco_runtime::run_capture`] owns a current-thread
    /// tokio runtime and drives a `!Send` `JsRuntime`; it must not run on an async
    /// worker. The `NetScriptFetcher` is built here (on the blocking thread) since
    /// it holds a `!Send` `Rc<dyn ScriptFetcher>` once inside `run_capture`.
    fn capture_blocking(
        url: &str,
        html: &[u8],
        config: &Config,
        opts: &draco_net::SessionOpts,
        mode: CaptureMode,
    ) -> CaptureResult {
        let script_fetcher: std::rc::Rc<dyn draco_runtime::ScriptFetcher> =
            std::rc::Rc::new(NetScriptFetcher {
                opts: subresource_opts(opts),
                cache: ChunkCache::shared(),
            });
        let html = String::from_utf8_lossy(html);
        let cfg = capture_config(config);
        let report = match mode {
            // Observe: data requests are stubbed (discover; SSR/hybrid fast path).
            CaptureMode::Observe => {
                draco_runtime::run_capture(url, &html, &cfg, script_fetcher)
            }
            // Render: the page's safe data requests hit the live network so a
            // pure-CSR shell's content materializes. API fetches share the same
            // subresource posture (clamped timeout, dropped delay, shared cookie
            // jar) as chunk loads.
            CaptureMode::Render => {
                let api: std::rc::Rc<dyn draco_runtime::ApiFetcher> =
                    std::rc::Rc::new(NetApiFetcher {
                        opts: subresource_opts(opts),
                        allow_unsafe: config.allow_unsafe_replay,
                    });
                draco_runtime::run_capture_render(url, &html, &cfg, script_fetcher, api)
            }
        };
        to_capture_result(report)
    }

    /// Production [`Tier2Capture`]: run one capture in-process on a dedicated
    /// blocking thread. There is no child process, no IPC, and no prefetch — the
    /// isolate pulls exactly the code it needs, concurrently, via the fetcher.
    pub(crate) struct ProdTier2Capture;

    #[async_trait]
    impl Tier2Capture for ProdTier2Capture {
        async fn capture(
            &self,
            url: &str,
            html: &[u8],
            config: &Config,
            opts: &draco_net::SessionOpts,
            mode: CaptureMode,
        ) -> Result<CaptureResult, DracoError> {
            let url = url.to_string();
            let html = html.to_vec();
            let config = config.clone();
            let opts = opts.clone();
            tokio::task::spawn_blocking(move || capture_blocking(&url, &html, &config, &opts, mode))
                .await
                .map_err(|e| {
                    jail_error(
                        JailKind::Spawn,
                        format!("capture task panicked/cancelled: {e}"),
                    )
                })
        }
    }

    /// Bounded-concurrency Tier 2 capture for the daemon.
    ///
    /// With the jail gone there is no warm child process to keep alive: every
    /// capture already builds a **fresh** snapshot-restored isolate (no cross-scrape
    /// state), and that snapshot-restore is the dominant per-job cost, so a "warm
    /// pool" would save almost nothing. What the daemon *does* need is a ceiling on
    /// how many isolates run at once — each is a live V8 heap — so this pool is a
    /// [`tokio::sync::Semaphore`] guarding the shared in-process capture path.
    ///
    /// Cloneable + `Send + Sync` (an `Arc` inner) so the daemon holds one and shares
    /// it across request handlers.
    #[derive(Clone)]
    pub struct Tier2Pool {
        permits: Arc<tokio::sync::Semaphore>,
    }

    impl Tier2Pool {
        /// Create a pool bounding concurrent captures to `size` (a good default is
        /// the CPU count — each concurrent capture is a live isolate). `max_jobs`,
        /// `no_jail`, and `strict_sandbox` are accepted for API/CLI compatibility
        /// but no longer affect capture now that the OS jail is gone.
        pub fn new(size: usize, _max_jobs: u32, _no_jail: bool, _strict_sandbox: bool) -> Self {
            Tier2Pool {
                permits: Arc::new(tokio::sync::Semaphore::new(size.max(1))),
            }
        }

        /// No warm children to retire; kept for daemon API compatibility.
        pub fn shutdown(&self) {}
    }

    #[async_trait]
    impl Tier2Capture for Tier2Pool {
        async fn capture(
            &self,
            url: &str,
            html: &[u8],
            config: &Config,
            opts: &draco_net::SessionOpts,
            mode: CaptureMode,
        ) -> Result<CaptureResult, DracoError> {
            // Cap concurrent isolates. The permit is held for the whole capture and
            // released on drop. The semaphore is never closed, so `acquire_owned`
            // only errors in impossible conditions — treat as a spawn error, don't
            // panic.
            let _permit = self.permits.clone().acquire_owned().await.map_err(|e| {
                jail_error(JailKind::Spawn, format!("pool semaphore closed: {e}"))
            })?;
            ProdTier2Capture.capture(url, html, config, opts, mode).await
        }
    }
}

#[cfg(feature = "tier2")]
pub(crate) use prod::ProdTier2Capture;
/// The capture concurrency pool — public so the daemon can hold one and route
/// scrapes through it (via [`crate::extract_with_pool`]).
#[cfg(feature = "tier2")]
pub use prod::Tier2Pool;

/// Lean-build stub of the capture pool: with no `tier2` feature there is no
/// runtime (V8) linked, so the pool cannot host captures. It exists only so the
/// daemon compiles and links the same way in both builds; its capture path
/// finalizes `Unsupported`, exactly like [`DisabledCapture`]. Constructor args
/// are ignored.
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
        _config: &Config,
        _opts: &draco_net::SessionOpts,
        _mode: CaptureMode,
    ) -> Result<CaptureResult, DracoError> {
        Err(jail_error(
            JailKind::Spawn,
            "built without tier2: Tier 2 (in-process V8 capture) is not compiled in",
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
            logs: Vec::new(),
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
    fn discover_flags_read_style_post_replayable() {
        // A JSON-content-type POST (a GraphQL/JSON-RPC read, or thrill.com's
        // `POST /api/websocket-manager/v1/tickets`) IS replayed by rank_and_replay,
        // so the discovery catalog's `replayable` flag must agree — the two must
        // never contradict (the pre-fix catalog flagged it false yet replay ran it).
        let capture = capture_of(vec![Candidate {
            method: "POST".into(),
            url: "https://api.example.com/graphql".into(),
            headers: vec![("content-type".into(), "application/json".into())],
            via: InterceptVia::Fetch,
        }]);
        let eps = discover_endpoints(&capture, "https://api.example.com/", false);
        assert_eq!(eps.len(), 1);
        assert!(
            eps[0].replayable,
            "a read-style JSON POST must be flagged replayable (mirror best_replayable): {eps:?}"
        );
    }

    #[test]
    fn quiesce_and_capture_window_are_clamped() {
        // Only meaningful with the prod seam compiled, but the helpers are
        // tier2-gated, so guard the assertion behind the same cfg.
        #[cfg(feature = "tier2")]
        {
            assert_eq!(effective_capture_window_ms(2_000), 2_000);
            assert_eq!(effective_capture_window_ms(60_000), 2_500);
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
            logs: Vec::new(),
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
