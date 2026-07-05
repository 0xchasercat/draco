//! The escalation **state machine** (spec §11).
//!
//! ```text
//!   Fetch ──▶ Tier0 ──▶ Tier1 ──▶ Tier2 ──▶ Finalize
//!     │         │         │         │
//!     │         │         │         └─ (Slice 4) V8 intercept + ranked replay
//!     │         │         └─ Next.js build-id `_next/data` replay
//!     │         └─ static embedded state (__NEXT_DATA__, JSON-LD, __NUXT__)
//!     └─ single Tier-0 GET; challenge short-circuit runs on this response
//! ```
//!
//! Each transition appends a [`TraceStep`] and folds its wall time into the
//! [`Timing`] breakdown. The ladder stops at the **cheapest** tier that
//! produces data; a run either finalizes `Success` (with a `source_tier`),
//! `NeedsBrowser` (challenge detected), `Unsupported` (ran out of tiers with
//! no match), or `Error` (a hard failure, e.g. the initial fetch).
//!
//! ## Testability seam
//!
//! The machine reaches the network only through [`PageFetcher`] and the static
//! extractors only through [`StaticEngine`]. In WS-C both `draco-net` and
//! `draco-static` are still `todo!()` stubs, so the production adapters
//! ([`crate::fetcher::NetFetcher`], [`ProdStatic`]) would panic if driven by a
//! unit test. Routing everything through these two traits lets tests supply
//! mocks that return fixtures, exercising the *entire* control flow —
//! sequencing, challenge short-circuit, `tier_max` clamp, trace/timing
//! assembly — with no network and no live extractor.

use std::time::Instant;

use draco_net::SessionOpts;
use draco_static::content::ScrapeResult;
use draco_static::StaticOutcome;
use draco_types::{
    DracoError, ExtractionResult, SourceTier, Status, StepOutcome, Timing, TraceStep,
};

use crate::challenge::detect_challenge;
use crate::fetcher::{NetFetcher, PageFetcher};
use crate::tier2::Tier2Capture;
use crate::{Config, OutputFormat};

// ---------------------------------------------------------------------------
// Tier ceilings (spec §11 `tier_max`)
// ---------------------------------------------------------------------------

/// Highest tier index Draco implements (2 = runtime interception).
pub const TIER_CEILING: u8 = 2;

/// Below this many non-whitespace Markdown characters, the scraped page is
/// treated as a thin client-rendered SPA shell and the `static.markdown` trace
/// step notes that an SPA render pass would help.
const THIN_CONTENT_CHARS: usize = 200;

/// Clamp a caller-supplied `tier_max` into the implemented range `0..=2`.
///
/// The CLI takes an arbitrary `u8`; anything above [`TIER_CEILING`] is treated
/// as "run the whole ladder", never as an error. Exposed (and unit-tested) so
/// the clamp is a documented, verifiable part of the policy.
pub fn clamp_tier_max(tier_max: u8) -> u8 {
    tier_max.min(TIER_CEILING)
}

// ---------------------------------------------------------------------------
// Static-extraction seam
// ---------------------------------------------------------------------------

/// The Tier 0/1 static operations the machine needs, behind a trait so the
/// ladder is drivable offline (see module docs). Mirrors the frozen
/// `draco-static` free functions one-to-one.
pub trait StaticEngine: Send + Sync {
    /// Markdown scrape: HTML → clean Markdown + metadata (the default path).
    fn scrape(
        &self,
        html: &str,
        url: &str,
        status: u16,
        content_type: &str,
        only_main_content: bool,
    ) -> ScrapeResult;
    /// Tier 0: scan HTML for embedded state.
    fn extract_static(&self, html: &str) -> StaticOutcome;
    /// Tier 1: discover a Next.js build id, if present.
    fn discover_build_id(&self, html: &str) -> Option<String>;
    /// Tier 1: construct the `_next/data/<build_id><pathname>.json` URL.
    fn next_data_url(&self, build_id: &str, pathname: &str, query: &[(String, String)]) -> String;
    /// Tier 1 guard: app-router (RSC) pages are not build-id eligible in v0.1.
    fn is_app_router(&self, html: &str) -> bool;
}

/// Production [`StaticEngine`] — delegates straight to `draco-static`.
///
/// The only place in `draco-core` that names the concrete static functions,
/// keeping the machine's control flow independent of `draco-static`'s
/// (currently stubbed) bodies.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProdStatic;

impl StaticEngine for ProdStatic {
    fn scrape(
        &self,
        html: &str,
        url: &str,
        status: u16,
        content_type: &str,
        only_main_content: bool,
    ) -> ScrapeResult {
        draco_static::content::scrape(html, url, status, content_type, only_main_content)
    }
    fn extract_static(&self, html: &str) -> StaticOutcome {
        draco_static::extract_static(html)
    }
    fn discover_build_id(&self, html: &str) -> Option<String> {
        draco_static::discover_build_id(html)
    }
    fn next_data_url(&self, build_id: &str, pathname: &str, query: &[(String, String)]) -> String {
        draco_static::next_data_url(build_id, pathname, query)
    }
    fn is_app_router(&self, html: &str) -> bool {
        draco_static::is_app_router(html)
    }
}

// ---------------------------------------------------------------------------
// SessionOpts derivation
// ---------------------------------------------------------------------------

/// Project the orchestration [`Config`] onto the network layer's per-session
/// options. Extracted (and tested) so the mapping is explicit.
pub fn session_opts(config: &Config) -> SessionOpts {
    SessionOpts {
        proxy: config.proxy.clone(),
        delay_ms: config.delay_ms,
        respect_robots: config.respect_robots,
        timeout_ms: config.timeout_ms,
    }
}

// ---------------------------------------------------------------------------
// Trace/timing accumulator
// ---------------------------------------------------------------------------

/// Collects [`TraceStep`]s and running [`Timing`] as the ladder advances, then
/// bakes them into an [`ExtractionResult`]. Keeps step-recording uniform (every
/// tier funnels through `record`) so no transition can forget to log itself.
struct Run {
    url: String,
    started: Instant,
    trace: Vec<TraceStep>,
    timing: Timing,
    /// Markdown-scrape output, staged before [`Run::finish`] bakes the result.
    /// Populated by the Markdown path (and the `Both` path); `None` on the
    /// pure-JSON (`Json`) path.
    markdown: Option<String>,
    /// Flat page metadata, staged alongside [`Run::markdown`].
    metadata: Option<serde_json::Value>,
}

/// Which timing bucket a step's elapsed time is charged to.
#[derive(Clone, Copy)]
enum Bucket {
    /// `draco-net` wall time.
    Network,
    /// Tier 0/1 AST/parse work.
    Parse,
    /// Tier 2 isolate wall time.
    Runtime,
    /// Pure orchestration — recorded in the trace but not charged to a bucket.
    None,
}

impl Run {
    fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            started: Instant::now(),
            trace: Vec::new(),
            timing: Timing::default(),
            markdown: None,
            metadata: None,
        }
    }

    /// Append a trace step, charge `elapsed_ms` to the given [`Bucket`].
    fn record(
        &mut self,
        tier: SourceTier,
        action: &str,
        outcome: StepOutcome,
        elapsed_ms: u64,
        bucket: Bucket,
        detail: Option<String>,
    ) {
        match bucket {
            Bucket::Network => self.timing.network_ms += elapsed_ms,
            Bucket::Parse => self.timing.parse_ms += elapsed_ms,
            Bucket::Runtime => self.timing.runtime_ms += elapsed_ms,
            Bucket::None => {}
        }
        self.trace.push(TraceStep {
            tier,
            action: action.to_string(),
            outcome,
            elapsed_ms,
            detail,
        });
    }

    /// Finalize with total wall time stamped from the run's start.
    fn finish(
        mut self,
        status: Status,
        source_tier: Option<SourceTier>,
        data: Option<serde_json::Value>,
        error: Option<DracoError>,
    ) -> ExtractionResult {
        self.timing.total_ms = self.started.elapsed().as_millis() as u64;
        ExtractionResult {
            url: self.url,
            status,
            source_tier,
            data,
            markdown: self.markdown,
            metadata: self.metadata,
            timing: self.timing,
            trace: self.trace,
            error,
        }
    }
}

// ---------------------------------------------------------------------------
// The ladder
// ---------------------------------------------------------------------------

/// Run the full escalation ladder with the production adapters. This is what
/// [`crate::extract`] calls; the generic [`run_ladder`] underneath is what
/// tests drive with mocks.
///
/// The Tier 2 capture seam is chosen by the `tier2` feature: the real
/// jail-spawning seam when on, a disabled seam (records "built without tier2")
/// when off. Both keep `extract` returning a well-formed result.
pub(crate) async fn run(url: &str, config: &Config) -> ExtractionResult {
    #[cfg(feature = "tier2")]
    let capture = crate::tier2::ProdTier2Capture;
    #[cfg(not(feature = "tier2"))]
    let capture = crate::tier2::DisabledCapture;
    run_ladder(url, config, &NetFetcher, &ProdStatic, &capture).await
}

/// The escalation ladder, generic over its three effect seams (network, static
/// extraction, Tier 2 capture) so it can be exercised offline. See module docs.
///
/// The `capture` seam abstracts the jail-hosted V8 capture: production passes the
/// real jail-spawning seam, tests pass a mock that fabricates intercepts so the
/// full ladder — including the Tier 2 rank/replay path — is exercisable without
/// forking a child. When the crate is built without the `tier2` feature, the
/// Tier 2 branch never touches `capture`; it records a "built without tier2" note
/// and finalizes `Unsupported`.
pub(crate) async fn run_ladder<F, S, T>(
    url: &str,
    config: &Config,
    fetcher: &F,
    statics: &S,
    capture: &T,
) -> ExtractionResult
where
    F: PageFetcher + ?Sized,
    S: StaticEngine + ?Sized,
    T: Tier2Capture + ?Sized,
{
    let mut run = Run::new(url);
    let opts = session_opts(config);
    let tier_max = clamp_tier_max(config.tier_max);

    // ---- Fetch ---------------------------------------------------------
    let fetch_started = Instant::now();
    let resp = match fetcher.fetch(url, &opts).await {
        Ok(resp) => {
            let elapsed = fetch_started.elapsed().as_millis() as u64;
            // Prefer the network layer's own measured elapsed if present.
            let net_ms = if resp.meta.elapsed_ms > 0 {
                resp.meta.elapsed_ms
            } else {
                elapsed
            };
            run.record(
                SourceTier::Static,
                "net.fetch",
                StepOutcome::Matched,
                net_ms,
                Bucket::Network,
                Some(format!("{} {}", resp.meta.status, resp.meta.final_url)),
            );
            resp
        }
        Err(e) => {
            let elapsed = fetch_started.elapsed().as_millis() as u64;
            run.record(
                SourceTier::Static,
                "net.fetch",
                StepOutcome::Failed,
                elapsed,
                Bucket::Network,
                Some(error_summary(&e)),
            );
            return run.finish(Status::Error, None, None, Some(e));
        }
    };

    let body = String::from_utf8_lossy(&resp.body).into_owned();

    // ---- Challenge short-circuit (spec §3) -----------------------------
    if let Some(kind) = detect_challenge(resp.meta.status, &resp.meta.headers, &body) {
        run.record(
            SourceTier::Static,
            "core.challenge",
            StepOutcome::Matched,
            0,
            Bucket::None,
            Some(kind.as_str().to_string()),
        );
        return run.finish(Status::NeedsBrowser, None, None, None);
    }

    // ---- Markdown scrape (the DEFAULT fast path; spec: Firecrawl-style) --
    //
    // For `Markdown` this is terminal: fetch → challenge → scrape → Success,
    // never touching V8/the jail (~300ms). For `Both` we stage the Markdown +
    // metadata onto the run and fall through to the JSON ladder below. For
    // `Json` the scrape is skipped entirely.
    if matches!(config.format, OutputFormat::Markdown | OutputFormat::Both) {
        let t_md = Instant::now();
        let content_type = content_type_of(&resp.meta.headers);
        let scraped = statics.scrape(&body, url, resp.meta.status, &content_type, true);
        let md_ms = t_md.elapsed().as_millis() as u64;

        // A thin, client-rendered SPA shell has almost no main content. We still
        // return what there is, but note that an SPA render pass would help.
        // (The render→markdown escalation itself is a deliberate follow-up.)
        let thin = draco_static::content::is_thin_content(&scraped.markdown, THIN_CONTENT_CHARS);
        run.record(
            SourceTier::Static,
            "static.markdown",
            StepOutcome::Matched,
            md_ms,
            Bucket::Parse,
            Some(if thin {
                format!(
                    "{} chars (thin: client-rendered SPA shell — an SPA render pass would help)",
                    scraped.markdown.len()
                )
            } else {
                format!("{} chars", scraped.markdown.len())
            }),
        );

        run.markdown = Some(scraped.markdown);
        run.metadata = Some(scraped.metadata);

        if config.format == OutputFormat::Markdown {
            // Terminal for the default path — no tier escalation.
            return run.finish(Status::Success, Some(SourceTier::Static), None, None);
        }
        // `Both`: continue into the JSON ladder, carrying markdown+metadata.
    }

    // ---- Tier 0: static embedded state ---------------------------------
    let t0 = Instant::now();
    match statics.extract_static(&body) {
        StaticOutcome::Hit(extracted) => {
            let elapsed = t0.elapsed().as_millis() as u64;
            run.record(
                SourceTier::Static,
                action_for_origin(extracted.origin),
                StepOutcome::Matched,
                elapsed,
                Bucket::Parse,
                None,
            );
            return run.finish(
                Status::Success,
                Some(SourceTier::Static),
                Some(extracted.data),
                None,
            );
        }
        StaticOutcome::Miss => {
            let elapsed = t0.elapsed().as_millis() as u64;
            run.record(
                SourceTier::Static,
                "static.scan",
                StepOutcome::Missed,
                elapsed,
                Bucket::Parse,
                None,
            );
        }
    }

    // ---- Tier 1: Next.js build-id `_next/data` replay ------------------
    if tier_max >= 1 {
        if let Some(outcome) = try_tier1(&mut run, url, &body, &opts, fetcher, statics).await {
            return outcome;
        }
    } else {
        run.record(
            SourceTier::HeuristicApiReplay,
            "tier1.build_id",
            StepOutcome::Skipped,
            0,
            Bucket::None,
            Some(format!("tier_max={tier_max}")),
        );
    }

    // ---- Tier 2: runtime interception (Slice 4) ------------------------
    if tier_max >= 2 {
        if let Some(outcome) =
            try_tier2(&mut run, url, &body, config, &opts, fetcher, capture).await
        {
            return outcome;
        }
    } else {
        run.record(
            SourceTier::RuntimeInterception,
            "runtime.capture",
            StepOutcome::Skipped,
            0,
            Bucket::None,
            Some(format!("tier_max={tier_max}")),
        );
    }

    // ---- Finalize: ran the whole (permitted) ladder, nothing matched ---
    //
    // Under `Both`, the JSON ladder found nothing — but we already produced
    // Markdown + metadata, which is the primary deliverable of that mode. So a
    // `Both` run with staged Markdown is a `Success` (source_tier: Static),
    // just without a `data` payload. A pure `Json` run stays `Unsupported`.
    if run.markdown.is_some() {
        return run.finish(Status::Success, Some(SourceTier::Static), None, None);
    }
    run.finish(Status::Unsupported, None, None, None)
}

/// Tier 2 sub-flow: jail-hosted V8 capture → ranked replay. Returns
/// `Some(result)` if the ladder should terminate here (a successful replay, or a
/// hard jail failure), or `None` to fall through to `Unsupported`.
///
/// Trace steps: `runtime.spawn` (spawn+capture the child), `runtime.sandbox`
/// (achieved sandbox level the child reported), `runtime.capture` (intercept
/// count + outcome), `runtime.rank` (winning score / no viable candidate),
/// `runtime.replay` (replaying the winner). Isolate wall time is charged to the
/// [`Bucket::Runtime`] timing bucket.
#[cfg(feature = "tier2")]
async fn try_tier2<F, T>(
    run: &mut Run,
    url: &str,
    body: &str,
    config: &Config,
    opts: &SessionOpts,
    fetcher: &F,
    capture: &T,
) -> Option<ExtractionResult>
where
    F: PageFetcher + ?Sized,
    T: Tier2Capture + ?Sized,
{
    use crate::tier2::{no_replay_reason, rank_and_replay};

    // --- Spawn + capture (jailed child hosts the isolate) ------------------
    let t_cap = Instant::now();
    let capture_result = match capture.capture(url, body.as_bytes(), config).await {
        Ok(c) => c,
        Err(e) => {
            // A jail/IPC failure is a hard failure of Tier 2: record it and
            // finalize `Error` carrying the mapped `DracoError::Jail`.
            run.record(
                SourceTier::RuntimeInterception,
                "runtime.spawn",
                StepOutcome::Failed,
                t_cap.elapsed().as_millis() as u64,
                Bucket::Runtime,
                Some(error_summary(&e)),
            );
            let owned = run.take_for_finish();
            return Some(owned.finish(Status::Error, None, None, Some(e)));
        }
    };
    let cap_ms = t_cap.elapsed().as_millis() as u64;
    run.record(
        SourceTier::RuntimeInterception,
        "runtime.spawn",
        StepOutcome::Matched,
        0,
        Bucket::None,
        None,
    );
    // Surface the achieved sandbox posture the child reported (e.g.
    // "hardened: seccomp+netns+landlock" or "isolate: v8 no host bindings
    // (macos)"). Informational — no timing bucket.
    if let Some(level) = capture_result.sandbox_level.as_deref() {
        run.record(
            SourceTier::RuntimeInterception,
            "runtime.sandbox",
            StepOutcome::Matched,
            0,
            Bucket::None,
            Some(level.to_string()),
        );
    }
    run.record(
        SourceTier::RuntimeInterception,
        "runtime.capture",
        if capture_result.candidates.is_empty() {
            StepOutcome::Missed
        } else {
            StepOutcome::Matched
        },
        cap_ms,
        Bucket::Runtime,
        Some(format!(
            "{:?}, {} intercept(s)",
            capture_result.outcome,
            capture_result.candidates.len()
        )),
    );

    // --- Rank + replay the winner -----------------------------------------
    // Mutation-safety (see `ranking::best_replayable`) is applied here at replay
    // time: `config.allow_unsafe_replay` decides whether a state-changing request
    // the ranker picked may be replayed.
    let t_rank = Instant::now();
    match rank_and_replay(
        &capture_result,
        url,
        opts,
        fetcher,
        config.allow_unsafe_replay,
    )
    .await
    {
        Ok(Some((data, detail))) => {
            // `runtime.rank` picked a viable winner; `runtime.replay` fetched
            // JSON. Charge the replay hop to the network bucket.
            run.record(
                SourceTier::RuntimeInterception,
                "runtime.rank",
                StepOutcome::Matched,
                0,
                Bucket::None,
                Some(detail.clone()),
            );
            run.record(
                SourceTier::RuntimeInterception,
                "runtime.replay",
                StepOutcome::Matched,
                t_rank.elapsed().as_millis() as u64,
                Bucket::Network,
                Some(detail),
            );
            let owned = run.take_for_finish();
            Some(owned.finish(
                Status::Success,
                Some(SourceTier::RuntimeInterception),
                Some(data),
                None,
            ))
        }
        Ok(None) => {
            // Either nothing cleared the viability bar, a viable candidate was
            // withheld for mutation-safety, or the winner's replay was non-2xx /
            // not JSON. Record a Missed rank with the precise reason (so a safety
            // withhold — and the `--allow-unsafe-replay` escape hatch — is
            // observable in the trace) and fall through to Unsupported.
            run.record(
                SourceTier::RuntimeInterception,
                "runtime.rank",
                StepOutcome::Missed,
                t_rank.elapsed().as_millis() as u64,
                Bucket::None,
                Some(no_replay_reason(&capture_result, url).note().to_string()),
            );
            None
        }
        Err(e) => {
            // The replay itself failed at the transport level. Record it and
            // finalize `Error` with the network error.
            run.record(
                SourceTier::RuntimeInterception,
                "runtime.replay",
                StepOutcome::Failed,
                t_rank.elapsed().as_millis() as u64,
                Bucket::Network,
                Some(error_summary(&e)),
            );
            let owned = run.take_for_finish();
            Some(owned.finish(Status::Error, None, None, Some(e)))
        }
    }
}

/// Tier 2 branch for the **lean** build (no `tier2` feature): there is no jail /
/// runtime linked, so record a "built without tier2" note and fall through to
/// `Unsupported`. Signature mirrors the tier2 version so the call site is
/// feature-agnostic; `_capture` is unused here.
#[cfg(not(feature = "tier2"))]
async fn try_tier2<F, T>(
    run: &mut Run,
    _url: &str,
    _body: &str,
    _config: &Config,
    _opts: &SessionOpts,
    _fetcher: &F,
    _capture: &T,
) -> Option<ExtractionResult>
where
    F: PageFetcher + ?Sized,
    T: Tier2Capture + ?Sized,
{
    run.record(
        SourceTier::RuntimeInterception,
        "runtime.capture",
        StepOutcome::Skipped,
        0,
        Bucket::None,
        Some("built without tier2: runtime interception not compiled in".to_string()),
    );
    None
}

/// Tier 1 sub-flow. Returns `Some(result)` if the ladder should terminate here
/// (a successful build-id replay), or `None` to fall through to Tier 2.
async fn try_tier1<F, S>(
    run: &mut Run,
    url: &str,
    body: &str,
    opts: &SessionOpts,
    fetcher: &F,
    statics: &S,
) -> Option<ExtractionResult>
where
    F: PageFetcher + ?Sized,
    S: StaticEngine + ?Sized,
{
    // App-router (RSC) pages are not build-id eligible in v0.1 — bail early.
    if statics.is_app_router(body) {
        run.record(
            SourceTier::HeuristicApiReplay,
            "tier1.build_id",
            StepOutcome::Skipped,
            0,
            Bucket::Parse,
            Some("app-router (rsc) page — not tier-1 eligible".to_string()),
        );
        return None;
    }

    let t1_discover = Instant::now();
    let build_id = match statics.discover_build_id(body) {
        Some(id) => {
            run.record(
                SourceTier::HeuristicApiReplay,
                "tier1.build_id",
                StepOutcome::Matched,
                t1_discover.elapsed().as_millis() as u64,
                Bucket::Parse,
                Some(id.clone()),
            );
            id
        }
        None => {
            run.record(
                SourceTier::HeuristicApiReplay,
                "tier1.build_id",
                StepOutcome::Missed,
                t1_discover.elapsed().as_millis() as u64,
                Bucket::Parse,
                None,
            );
            return None;
        }
    };

    // Build the `_next/data` URL from the page's path + query.
    let (pathname, query) = split_path_query(url);
    let data_url = statics.next_data_url(&build_id, &pathname, &query);
    let spec = draco_types::HttpRequestSpec {
        method: "GET".to_string(),
        url: data_url.clone(),
        headers: Vec::new(),
        body_b64: None,
    };

    // Replay it.
    let t1_replay = Instant::now();
    let resp = match fetcher.replay(&spec, opts).await {
        Ok(r) => {
            let net_ms = if r.meta.elapsed_ms > 0 {
                r.meta.elapsed_ms
            } else {
                t1_replay.elapsed().as_millis() as u64
            };
            run.record(
                SourceTier::HeuristicApiReplay,
                "tier1.replay",
                if is_2xx(r.meta.status) {
                    StepOutcome::Matched
                } else {
                    StepOutcome::Missed
                },
                net_ms,
                Bucket::Network,
                Some(format!("{} {}", r.meta.status, data_url)),
            );
            r
        }
        Err(e) => {
            run.record(
                SourceTier::HeuristicApiReplay,
                "tier1.replay",
                StepOutcome::Failed,
                t1_replay.elapsed().as_millis() as u64,
                Bucket::Network,
                Some(error_summary(&e)),
            );
            return None;
        }
    };

    if !is_2xx(resp.meta.status) {
        return None;
    }

    // Parse the JSON body.
    let t1_parse = Instant::now();
    match serde_json::from_slice::<serde_json::Value>(&resp.body) {
        Ok(value) => {
            run.record(
                SourceTier::HeuristicApiReplay,
                "tier1.parse",
                StepOutcome::Matched,
                t1_parse.elapsed().as_millis() as u64,
                Bucket::Parse,
                None,
            );
            let owned = run.take_for_finish();
            Some(owned.finish(
                Status::Success,
                Some(SourceTier::HeuristicApiReplay),
                Some(value),
                None,
            ))
        }
        Err(e) => {
            run.record(
                SourceTier::HeuristicApiReplay,
                "tier1.parse",
                StepOutcome::Failed,
                t1_parse.elapsed().as_millis() as u64,
                Bucket::Parse,
                Some(format!("non-json _next/data body: {e}")),
            );
            None
        }
    }
}

impl Run {
    /// Move the accumulator out of a `&mut` borrow so a sub-flow can `finish`
    /// it. Cheap: the fields are `String`/`Vec`, swapped for empties we drop.
    fn take_for_finish(&mut self) -> Run {
        Run {
            url: std::mem::take(&mut self.url),
            started: self.started,
            trace: std::mem::take(&mut self.trace),
            timing: std::mem::take(&mut self.timing),
            markdown: self.markdown.take(),
            metadata: self.metadata.take(),
        }
    }
}

// ---------------------------------------------------------------------------
// Small helpers (all pure — unit-tested below)
// ---------------------------------------------------------------------------

/// Map an [`ExtractOrigin`](draco_types::ExtractOrigin) to its Tier 0 trace
/// action name.
fn action_for_origin(origin: draco_types::ExtractOrigin) -> &'static str {
    use draco_types::ExtractOrigin::*;
    match origin {
        NextData => "static.next_data",
        JsonLd => "static.json_ld",
        NuxtWindow => "static.nuxt",
        NextBuildApi => "static.next_data", // Tier 1 origin surfaced via Tier 0 label
    }
}

/// Is this an HTTP 2xx status?
fn is_2xx(status: u16) -> bool {
    (200..300).contains(&status)
}

/// Pull the `Content-Type` value from a response header list (case-insensitive
/// header name), defaulting to `text/html` when absent — the Markdown scrape
/// surfaces this verbatim as the `contentType` metadata key.
fn content_type_of(headers: &[(String, String)]) -> String {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| "text/html".to_string())
}

/// One-line, log-safe summary of a [`DracoError`] for a trace `detail`.
fn error_summary(e: &DracoError) -> String {
    match e {
        DracoError::Network { reason, detail } => format!("network/{reason:?}: {detail}"),
        DracoError::Parse { detail } => format!("parse: {detail}"),
        DracoError::Jail { reason, detail } => format!("jail/{reason:?}: {detail}"),
        DracoError::Runtime { detail } => format!("runtime: {detail}"),
        DracoError::Ipc { detail } => format!("ipc: {detail}"),
        DracoError::Config { detail } => format!("config: {detail}"),
    }
}

/// Split a URL into `(pathname, query pairs)`, tolerating relative inputs and a
/// missing/opaque URL by defaulting to `/`. Tier 1 needs the page's route to
/// build the `_next/data` URL.
fn split_path_query(url: &str) -> (String, Vec<(String, String)>) {
    if let Ok(parsed) = url::Url::parse(url) {
        let path = if parsed.path().is_empty() {
            "/".to_string()
        } else {
            parsed.path().to_string()
        };
        let query: Vec<(String, String)> = parsed
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        return (path, query);
    }
    // Relative or unparseable: strip a query manually, default path to "/".
    let (path, q) = match url.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (url, None),
    };
    let path = if path.is_empty() { "/" } else { path };
    let query = q
        .map(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect()
        })
        .unwrap_or_default();
    (path.to_string(), query)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ranking::Candidate;
    use crate::testutil::{err_fetcher, noop_capture, MockCapture, MockFetcher, MockStatic};
    use draco_types::{ExtractOrigin, ExtractedData, InterceptVia, NetKind};
    use serde_json::json;

    // ---- pure helpers -------------------------------------------------

    #[test]
    fn tier_max_clamps_into_range() {
        assert_eq!(clamp_tier_max(0), 0);
        assert_eq!(clamp_tier_max(1), 1);
        assert_eq!(clamp_tier_max(2), 2);
        assert_eq!(clamp_tier_max(3), 2);
        assert_eq!(clamp_tier_max(255), 2);
    }

    #[test]
    fn session_opts_projects_config() {
        let cfg = Config {
            proxy: Some("http://p:8080".into()),
            delay_ms: 250,
            timeout_ms: 9_000,
            respect_robots: false,
            ..Config::default()
        };
        let o = session_opts(&cfg);
        assert_eq!(o.proxy.as_deref(), Some("http://p:8080"));
        assert_eq!(o.delay_ms, 250);
        assert_eq!(o.timeout_ms, 9_000);
        assert!(!o.respect_robots);
    }

    #[test]
    fn split_path_query_handles_absolute_relative_and_bare() {
        let (p, q) = split_path_query("https://x.com/a/b?k=1&j=two");
        assert_eq!(p, "/a/b");
        assert_eq!(
            q,
            vec![
                ("k".to_string(), "1".to_string()),
                ("j".to_string(), "two".to_string())
            ]
        );

        let (p, q) = split_path_query("/rel/path");
        assert_eq!(p, "/rel/path");
        assert!(q.is_empty());

        let (p, _q) = split_path_query("https://x.com");
        assert_eq!(p, "/");
    }

    #[test]
    fn error_summary_is_one_line() {
        let s = error_summary(&DracoError::Network {
            reason: NetKind::Timeout,
            detail: "connect".into(),
        });
        assert!(s.starts_with("network/Timeout"));
        assert!(!s.contains('\n'));
    }

    // ---- ladder via mocks (offline) -----------------------------------

    /// A `Json`-format config with the given `tier_max`. The JSON tiers (0/1/2)
    /// are what these ladder tests exercise; the default `Markdown` format is
    /// covered by its own tests below.
    fn cfg(tier_max: u8) -> Config {
        Config {
            format: OutputFormat::Json,
            tier_max,
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn fetch_failure_finalizes_error() {
        let fetcher = err_fetcher(DracoError::Network {
            reason: NetKind::Dns,
            detail: "no such host".into(),
        });
        let statics = MockStatic::default(); // never consulted
        let r = run_ladder(
            "https://x.com",
            &cfg(2),
            &fetcher,
            &statics,
            &noop_capture(),
        )
        .await;
        assert_eq!(r.status, Status::Error);
        assert!(r.error.is_some());
        assert_eq!(r.source_tier, None);
        // Exactly one trace step: the failed fetch.
        assert_eq!(r.trace.len(), 1);
        assert_eq!(r.trace[0].action, "net.fetch");
        assert_eq!(r.trace[0].outcome, StepOutcome::Failed);
    }

    #[tokio::test]
    async fn challenge_short_circuits_to_needs_browser() {
        // A Cloudflare "just a moment" interstitial.
        let html = r#"<html><head><title>Just a moment...</title></head>
            <body>cloudflare challenge-platform cf_chl_opt</body></html>"#;
        let fetcher = MockFetcher::ok_html(503, html)
            .with_header("server", "cloudflare")
            .with_header("cf-mitigated", "challenge");
        let statics = MockStatic::hit_next_data(); // must NOT be consulted
        let r = run_ladder(
            "https://x.com/p",
            &cfg(2),
            &fetcher,
            &statics,
            &noop_capture(),
        )
        .await;
        assert_eq!(r.status, Status::NeedsBrowser);
        assert_eq!(r.source_tier, None);
        assert!(r.data.is_none());
        // fetch + challenge steps; no tier steps after the short-circuit.
        let actions: Vec<&str> = r.trace.iter().map(|t| t.action.as_str()).collect();
        assert_eq!(actions, vec!["net.fetch", "core.challenge"]);
        assert_eq!(r.trace[1].detail.as_deref(), Some("cloudflare"));
    }

    #[tokio::test]
    async fn tier0_hit_finalizes_success_static() {
        let fetcher = MockFetcher::ok_html(200, "<html>__NEXT_DATA__</html>");
        let statics = MockStatic::hit(ExtractedData {
            tier: SourceTier::Static,
            origin: ExtractOrigin::NextData,
            data: json!({ "props": { "ok": true } }),
        });
        let r = run_ladder(
            "https://x.com/p",
            &cfg(2),
            &fetcher,
            &statics,
            &noop_capture(),
        )
        .await;
        assert_eq!(r.status, Status::Success);
        assert_eq!(r.source_tier, Some(SourceTier::Static));
        assert_eq!(r.data, Some(json!({ "props": { "ok": true } })));
        let actions: Vec<&str> = r.trace.iter().map(|t| t.action.as_str()).collect();
        assert_eq!(actions, vec!["net.fetch", "static.next_data"]);
        assert_eq!(r.trace[1].outcome, StepOutcome::Matched);
    }

    #[tokio::test]
    async fn tier1_build_id_replay_success() {
        let fetcher = MockFetcher::ok_html(200, "<html>next build</html>")
            // The Tier 1 replay returns JSON.
            .with_replay_json(200, json!({ "pageProps": { "price": 42 } }));
        let statics = MockStatic::miss_then_build_id("BUILDID123");
        let r = run_ladder(
            "https://shop.example.com/p/1",
            &cfg(2),
            &fetcher,
            &statics,
            &noop_capture(),
        )
        .await;
        assert_eq!(r.status, Status::Success);
        assert_eq!(r.source_tier, Some(SourceTier::HeuristicApiReplay));
        assert_eq!(r.data, Some(json!({ "pageProps": { "price": 42 } })));
        let actions: Vec<&str> = r.trace.iter().map(|t| t.action.as_str()).collect();
        assert_eq!(
            actions,
            vec![
                "net.fetch",
                "static.scan",
                "tier1.build_id",
                "tier1.replay",
                "tier1.parse"
            ]
        );
        // The replay URL was constructed from the build id + path.
        let bid = &r.trace[2];
        assert_eq!(bid.detail.as_deref(), Some("BUILDID123"));
        // Timing: both fetch and replay charged to network.
        assert!(r.timing.network_ms >= 2, "two network hops expected");
    }

    #[tokio::test]
    async fn app_router_skips_tier1() {
        let fetcher = MockFetcher::ok_html(200, "<html>rsc</html>");
        let statics = MockStatic::miss_app_router();
        let r = run_ladder(
            "https://x.com/p",
            &cfg(2),
            &fetcher,
            &statics,
            &noop_capture(),
        )
        .await;
        // No build-id attempt; Tier 2 runs but the mock capture yields nothing,
        // so the ladder falls through to Unsupported.
        assert_eq!(r.status, Status::Unsupported);
        let bid = r
            .trace
            .iter()
            .find(|t| t.action == "tier1.build_id")
            .unwrap();
        assert_eq!(bid.outcome, StepOutcome::Skipped);
        assert!(bid.detail.as_deref().unwrap().contains("app-router"));
    }

    #[tokio::test]
    async fn tier_max_zero_skips_tier1_and_tier2() {
        let fetcher = MockFetcher::ok_html(200, "<html>plain</html>");
        let statics = MockStatic::miss_then_build_id("SHOULD_NOT_BE_USED");
        let r = run_ladder(
            "https://x.com/p",
            &cfg(0),
            &fetcher,
            &statics,
            &noop_capture(),
        )
        .await;
        assert_eq!(r.status, Status::Unsupported);
        // Tier 1 and Tier 2 both recorded as Skipped with a tier_max reason.
        let t1 = r
            .trace
            .iter()
            .find(|t| t.action == "tier1.build_id")
            .unwrap();
        assert_eq!(t1.outcome, StepOutcome::Skipped);
        assert!(t1.detail.as_deref().unwrap().contains("tier_max=0"));
        let t2 = r
            .trace
            .iter()
            .find(|t| t.action == "runtime.capture")
            .unwrap();
        assert_eq!(t2.outcome, StepOutcome::Skipped);
        // discover_build_id must NOT have been called (tier gated off).
        assert_eq!(fetcher.replay_calls(), 0, "no replay under tier_max=0");
    }

    #[tokio::test]
    async fn tier_max_one_runs_tier1_but_skips_tier2() {
        let fetcher = MockFetcher::ok_html(200, "<html>plain</html>");
        let statics = MockStatic::miss_no_build_id();
        let r = run_ladder(
            "https://x.com/p",
            &cfg(1),
            &fetcher,
            &statics,
            &noop_capture(),
        )
        .await;
        assert_eq!(r.status, Status::Unsupported);
        let t1 = r
            .trace
            .iter()
            .find(|t| t.action == "tier1.build_id")
            .unwrap();
        assert_eq!(t1.outcome, StepOutcome::Missed); // ran, found nothing
        let t2 = r
            .trace
            .iter()
            .find(|t| t.action == "runtime.capture")
            .unwrap();
        assert_eq!(t2.outcome, StepOutcome::Skipped);
        assert!(t2.detail.as_deref().unwrap().contains("tier_max=1"));
    }

    #[tokio::test]
    async fn tier2_no_intercepts_is_unsupported() {
        // Tier 2 runs but the SPA never fetched: capture yields nothing, so the
        // ladder finalizes Unsupported after recording the capture + a missed rank.
        let fetcher = MockFetcher::ok_html(200, "<html>spa</html>");
        let statics = MockStatic::miss_no_build_id();
        let r = run_ladder(
            "https://x.com/p",
            &cfg(2),
            &fetcher,
            &statics,
            &noop_capture(),
        )
        .await;
        assert_eq!(r.status, Status::Unsupported);
        let actions: Vec<&str> = r.trace.iter().map(|t| t.action.as_str()).collect();
        assert!(actions.contains(&"runtime.spawn"), "trace: {actions:?}");
        let cap = r
            .trace
            .iter()
            .find(|t| t.action == "runtime.capture")
            .unwrap();
        assert_eq!(cap.outcome, StepOutcome::Missed);
        assert!(cap.detail.as_deref().unwrap().contains("NoIntercepts"));
        let rank = r.trace.iter().find(|t| t.action == "runtime.rank").unwrap();
        assert_eq!(rank.outcome, StepOutcome::Missed);
    }

    #[tokio::test]
    async fn tier2_ranks_and_replays_winner_to_success() {
        // Capture surfaces a junk asset + a strong JSON API; the ladder must pick
        // the API, replay it, and finalize Success/RuntimeInterception.
        let fetcher = MockFetcher::ok_html(200, "<html>spa</html>")
            .with_replay_json(200, json!({ "price": 42, "title": "Widget" }));
        let statics = MockStatic::miss_no_build_id();
        let capture = MockCapture::with_candidates(vec![
            Candidate::get("https://cdn.example.com/app.js", InterceptVia::Fetch),
            Candidate {
                method: "GET".into(),
                url: "https://api.example.com/v1/items?id=1".into(),
                headers: vec![("accept".into(), "application/json".into())],
                via: InterceptVia::Fetch,
            },
        ]);
        let r = run_ladder(
            "https://shop.example.com/p",
            &cfg(2),
            &fetcher,
            &statics,
            &capture,
        )
        .await;
        assert_eq!(r.status, Status::Success);
        assert_eq!(r.source_tier, Some(SourceTier::RuntimeInterception));
        assert_eq!(r.data, Some(json!({ "price": 42, "title": "Widget" })));
        assert_eq!(capture.calls(), 1, "capture seam must be exercised once");
        assert_eq!(fetcher.replay_calls(), 1, "the winner must be replayed");
        // The trace records the full Tier 2 sequence.
        let actions: Vec<&str> = r.trace.iter().map(|t| t.action.as_str()).collect();
        for step in [
            "runtime.spawn",
            "runtime.capture",
            "runtime.rank",
            "runtime.replay",
        ] {
            assert!(actions.contains(&step), "missing {step} in {actions:?}");
        }
        let replay = r
            .trace
            .iter()
            .find(|t| t.action == "runtime.replay")
            .unwrap();
        assert_eq!(replay.outcome, StepOutcome::Matched);
        // Isolate capture time is charged to the runtime bucket.
        // (The mock capture returns instantly, so we only assert the bucket
        //  exists in the trace, not a positive duration.)
        assert!(r.trace.iter().any(|t| t.action == "runtime.capture"));
    }

    #[tokio::test]
    async fn tier2_all_junk_intercepts_is_unsupported() {
        // Capture surfaces only assets/analytics: nothing clears MIN_VIABLE_SCORE,
        // so no replay is attempted and the run is Unsupported.
        let fetcher = MockFetcher::ok_html(200, "<html>spa</html>");
        let statics = MockStatic::miss_no_build_id();
        let capture = MockCapture::with_candidates(vec![
            Candidate::get("https://cdn.example.com/app.js", InterceptVia::Fetch),
            Candidate::get(
                "https://www.google-analytics.com/collect",
                InterceptVia::Xhr,
            ),
        ]);
        let r = run_ladder("https://x.com/p", &cfg(2), &fetcher, &statics, &capture).await;
        assert_eq!(r.status, Status::Unsupported);
        assert_eq!(fetcher.replay_calls(), 0, "junk must not be replayed");
        let rank = r.trace.iter().find(|t| t.action == "runtime.rank").unwrap();
        assert_eq!(rank.outcome, StepOutcome::Missed);
    }

    #[tokio::test]
    async fn tier2_jail_failure_finalizes_error() {
        // A spawn/protocol failure in the capture seam maps to DracoError::Jail
        // and finalizes Status::Error.
        let fetcher = MockFetcher::ok_html(200, "<html>spa</html>");
        let statics = MockStatic::miss_no_build_id();
        let capture = MockCapture::failing(DracoError::Jail {
            reason: draco_types::JailKind::Killed,
            detail: "child SIGSYS".into(),
        });
        let r = run_ladder("https://x.com/p", &cfg(2), &fetcher, &statics, &capture).await;
        assert_eq!(r.status, Status::Error);
        assert!(matches!(r.error, Some(DracoError::Jail { .. })));
        let spawn = r
            .trace
            .iter()
            .find(|t| t.action == "runtime.spawn")
            .unwrap();
        assert_eq!(spawn.outcome, StepOutcome::Failed);
        assert_eq!(fetcher.replay_calls(), 0, "no replay after a jail failure");
    }

    #[tokio::test]
    async fn tier2_replay_transport_failure_finalizes_error() {
        // A viable winner whose replay fails at the transport level → Error.
        let fetcher = crate::testutil::err_replay_fetcher(DracoError::Network {
            reason: NetKind::Timeout,
            detail: "replay timed out".into(),
        });
        let statics = MockStatic::miss_no_build_id();
        let capture = MockCapture::with_candidates(vec![Candidate {
            method: "GET".into(),
            url: "https://api.example.com/v1/items".into(),
            headers: vec![("accept".into(), "application/json".into())],
            via: InterceptVia::Fetch,
        }]);
        let r = run_ladder("https://x.com/p", &cfg(2), &fetcher, &statics, &capture).await;
        assert_eq!(r.status, Status::Error);
        assert!(matches!(r.error, Some(DracoError::Network { .. })));
        let replay = r
            .trace
            .iter()
            .find(|t| t.action == "runtime.replay")
            .unwrap();
        assert_eq!(replay.outcome, StepOutcome::Failed);
    }

    #[tokio::test]
    async fn timing_buckets_are_attributed() {
        // Tier 0 hit: one network hop (the mock reports elapsed_ms = 1) and one
        // parse step. total_ms is stamped from the wall clock independently and
        // may be 0 for a sub-ms mock run, so it is *not* comparable to the mock's
        // fabricated network_ms — assert each bucket on its own terms.
        let fetcher = MockFetcher::ok_html(200, "<html>x</html>");
        let statics = MockStatic::hit_next_data();
        let r = run_ladder(
            "https://x.com",
            &cfg(0),
            &fetcher,
            &statics,
            &noop_capture(),
        )
        .await;
        // The fetch charged the mock's reported elapsed to the network bucket.
        assert_eq!(
            r.timing.network_ms, 1,
            "mock elapsed_ms folds into network_ms"
        );
        // No Tier 2, so runtime stays zero.
        assert_eq!(r.timing.runtime_ms, 0);
        // Every trace step's elapsed_ms summed by bucket equals the Timing totals
        // (minus total_ms, which is a wall-clock stamp, not a per-step sum).
        let net_sum: u64 = r
            .trace
            .iter()
            .filter(|s| s.action == "net.fetch" || s.action == "tier1.replay")
            .map(|s| s.elapsed_ms)
            .sum();
        assert_eq!(net_sum, r.timing.network_ms);
    }

    #[tokio::test]
    async fn tier1_non_2xx_replay_falls_through() {
        let fetcher = MockFetcher::ok_html(200, "<html>next</html>").with_replay_status(404); // _next/data 404s
        let statics = MockStatic::miss_then_build_id("BID");
        let r = run_ladder(
            "https://x.com/p",
            &cfg(2),
            &fetcher,
            &statics,
            &noop_capture(),
        )
        .await;
        assert_eq!(r.status, Status::Unsupported);
        let replay = r.trace.iter().find(|t| t.action == "tier1.replay").unwrap();
        assert_eq!(replay.outcome, StepOutcome::Missed);
    }

    // ---- Markdown (default) path --------------------------------------

    /// A Markdown-format config.
    fn cfg_markdown() -> Config {
        Config {
            format: OutputFormat::Markdown,
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn markdown_is_the_fast_path_and_never_touches_tier2() {
        // Default Markdown format: fetch → scrape → Success/Static, with a
        // `static.markdown` step and NO tier1/tier2 escalation. The capture seam
        // must never be reached (it would panic on the real jail).
        let fetcher = MockFetcher::ok_html(200, "<html><body><h1>Hi</h1></body></html>")
            .with_header("content-type", "text/html; charset=utf-8");
        let statics = MockStatic::default().with_markdown("# Hi\n\nSome body text here.");
        // A capture double that *panics* if the ladder ever reaches Tier 2.
        let capture = MockCapture::failing(DracoError::Jail {
            reason: draco_types::JailKind::Killed,
            detail: "must not be reached on the markdown path".into(),
        });
        let r = run_ladder(
            "https://x.com/p",
            &cfg_markdown(),
            &fetcher,
            &statics,
            &capture,
        )
        .await;

        assert_eq!(r.status, Status::Success);
        assert_eq!(r.source_tier, Some(SourceTier::Static));
        assert!(r.data.is_none(), "markdown path carries no JSON data");
        assert_eq!(r.markdown.as_deref(), Some("# Hi\n\nSome body text here."));
        // Metadata carries the synthetic keys from the (mock) scrape.
        let meta = r.metadata.expect("metadata present");
        assert_eq!(meta["statusCode"], 200);
        assert_eq!(meta["contentType"], "text/html; charset=utf-8");

        // Trace: fetch + static.markdown only. No tier1/tier2 steps, no capture.
        let actions: Vec<&str> = r.trace.iter().map(|t| t.action.as_str()).collect();
        assert_eq!(actions, vec!["net.fetch", "static.markdown"]);
        assert_eq!(
            capture.calls(),
            0,
            "Tier 2 capture must NOT run on the markdown path"
        );
        assert_eq!(fetcher.replay_calls(), 0, "no replay on the markdown path");
    }

    #[tokio::test]
    async fn markdown_thin_spa_notes_render_pass() {
        // A near-empty (thin) scrape still succeeds but the trace notes that an
        // SPA render pass would help.
        let fetcher = MockFetcher::ok_html(200, "<html><body><div id=root></div></body></html>");
        let statics = MockStatic::default().with_markdown("x"); // 1 char → thin
        let r = run_ladder(
            "https://spa.example/",
            &cfg_markdown(),
            &fetcher,
            &statics,
            &noop_capture(),
        )
        .await;
        assert_eq!(r.status, Status::Success);
        let md_step = r
            .trace
            .iter()
            .find(|t| t.action == "static.markdown")
            .unwrap();
        assert!(
            md_step
                .detail
                .as_deref()
                .unwrap()
                .contains("SPA render pass"),
            "thin content should note an SPA render pass: {:?}",
            md_step.detail
        );
    }

    #[tokio::test]
    async fn markdown_still_short_circuits_on_challenge() {
        // The challenge short-circuit runs before the markdown scrape, so a
        // bot-wall still yields NeedsBrowser with no markdown.
        let html = "<html><head><title>Just a moment...</title></head>\
            <body>cloudflare challenge-platform cf_chl_opt</body></html>";
        let fetcher = MockFetcher::ok_html(503, html)
            .with_header("server", "cloudflare")
            .with_header("cf-mitigated", "challenge");
        let statics = MockStatic::default().with_markdown("should not be produced");
        let r = run_ladder(
            "https://x.com/p",
            &cfg_markdown(),
            &fetcher,
            &statics,
            &noop_capture(),
        )
        .await;
        assert_eq!(r.status, Status::NeedsBrowser);
        assert!(r.markdown.is_none(), "no markdown when challenged");
    }

    // ---- Both path ----------------------------------------------------

    fn cfg_both() -> Config {
        Config {
            format: OutputFormat::Both,
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn both_populates_markdown_and_json_when_tier0_hits() {
        // Both: markdown+metadata AND the JSON ladder. Tier 0 hits here, so all
        // three of markdown/metadata/data are populated.
        let fetcher = MockFetcher::ok_html(200, "<html>__NEXT_DATA__</html>");
        let statics = MockStatic::hit(ExtractedData {
            tier: SourceTier::Static,
            origin: ExtractOrigin::NextData,
            data: json!({ "props": { "ok": true } }),
        })
        .with_markdown("# From Both");
        let r = run_ladder(
            "https://x.com/p",
            &cfg_both(),
            &fetcher,
            &statics,
            &noop_capture(),
        )
        .await;

        assert_eq!(r.status, Status::Success);
        assert_eq!(r.source_tier, Some(SourceTier::Static));
        assert_eq!(r.data, Some(json!({ "props": { "ok": true } })));
        assert_eq!(r.markdown.as_deref(), Some("# From Both"));
        assert!(r.metadata.is_some());
        // Both the markdown step and the JSON tier-0 step are recorded.
        let actions: Vec<&str> = r.trace.iter().map(|t| t.action.as_str()).collect();
        assert!(actions.contains(&"static.markdown"), "trace: {actions:?}");
        assert!(actions.contains(&"static.next_data"), "trace: {actions:?}");
    }

    #[tokio::test]
    async fn both_succeeds_with_markdown_even_when_json_ladder_finds_nothing() {
        // Both: markdown is produced but the JSON ladder finds nothing. The run
        // is still Success (source_tier: Static) with markdown but no data.
        let fetcher = MockFetcher::ok_html(200, "<html><body>plain</body></html>");
        let statics = MockStatic::miss_no_build_id().with_markdown("# Only markdown");
        let r = run_ladder(
            "https://x.com/p",
            &cfg_both(),
            &fetcher,
            &statics,
            &noop_capture(),
        )
        .await;

        assert_eq!(r.status, Status::Success);
        assert_eq!(r.source_tier, Some(SourceTier::Static));
        assert!(r.data.is_none(), "JSON ladder found nothing");
        assert_eq!(r.markdown.as_deref(), Some("# Only markdown"));
        // The JSON tiers still ran (and missed) — the markdown step precedes them.
        let actions: Vec<&str> = r.trace.iter().map(|t| t.action.as_str()).collect();
        assert!(actions.contains(&"static.markdown"), "trace: {actions:?}");
        assert!(actions.contains(&"static.scan"), "trace: {actions:?}");
    }

    #[tokio::test]
    async fn json_format_skips_the_markdown_scrape() {
        // Pure Json: no markdown/metadata, and no `static.markdown` trace step.
        let fetcher = MockFetcher::ok_html(200, "<html>__NEXT_DATA__</html>");
        let statics = MockStatic::hit(ExtractedData {
            tier: SourceTier::Static,
            origin: ExtractOrigin::NextData,
            data: json!({ "ok": true }),
        })
        .with_markdown("should be ignored");
        let r = run_ladder(
            "https://x.com/p",
            &cfg(2),
            &fetcher,
            &statics,
            &noop_capture(),
        )
        .await;

        assert_eq!(r.status, Status::Success);
        assert!(
            r.markdown.is_none(),
            "json format must not produce markdown"
        );
        assert!(
            r.metadata.is_none(),
            "json format must not produce metadata"
        );
        let actions: Vec<&str> = r.trace.iter().map(|t| t.action.as_str()).collect();
        assert!(!actions.contains(&"static.markdown"), "trace: {actions:?}");
    }
}
