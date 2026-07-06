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
    /// The tier the staged Markdown came from: [`SourceTier::Static`] for a
    /// plain fetch+parse, or [`SourceTier::RuntimeInterception`] once the
    /// render-then-Markdown escalation hydrated a thin shell and re-scraped it.
    md_tier: SourceTier,
    /// The discovered API-endpoint catalog, staged by the discovery branch when
    /// `config.discover_endpoints` is set; `None` otherwise.
    endpoints: Option<Vec<draco_types::DiscoveredEndpoint>>,
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
            md_tier: SourceTier::Static,
            endpoints: None,
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
            endpoints: self.endpoints,
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

/// Run the ladder using a warm [`Tier2Pool`](crate::Tier2Pool) as the Tier 2
/// capture seam instead of spawning a fresh child per scrape. This is what the
/// daemon ([`crate::extract_with_pool`]) calls; the network + static seams are
/// the same production adapters as [`run`].
pub(crate) async fn run_with_pool(
    url: &str,
    config: &Config,
    pool: &crate::tier2::Tier2Pool,
) -> ExtractionResult {
    run_ladder(url, config, &NetFetcher, &ProdStatic, pool).await
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

        // A page needs the render pass when its static extraction is either
        // **thin** (almost no main content) OR an **incomplete client-side
        // render** — a skeleton screen whose real content has not loaded (many
        // `Loading…` placeholders). The skeleton case is length-independent: a
        // retail homepage carries enough nav/promo chrome to clear the thin bar
        // while its product rails are still `Loading…`, so char-count alone would
        // wrongly return the skeleton. When Tier 2 is permitted this triggers the
        // render-then-Markdown escalation below; when capped out (`--tier-max < 2`)
        // we return the (placeholder-stripped) shell and say so.
        let thin = draco_static::content::is_thin_content(&scraped.markdown, THIN_CONTENT_CHARS);
        let incomplete = scraped.incomplete;
        let needs_render = thin || incomplete;
        let reason = if incomplete {
            "incomplete render: skeleton/loading shell"
        } else {
            "thin client-rendered shell"
        };
        run.record(
            SourceTier::Static,
            "static.markdown",
            StepOutcome::Matched,
            md_ms,
            Bucket::Parse,
            Some(if needs_render && tier_max >= 2 {
                format!(
                    "{} chars ({reason} — escalating to render)",
                    scraped.markdown.len()
                )
            } else if needs_render {
                format!(
                    "{} chars ({reason} — render skipped, tier_max={tier_max})",
                    scraped.markdown.len()
                )
            } else {
                format!("{} chars", scraped.markdown.len())
            }),
        );

        run.markdown = Some(scraped.markdown);
        run.metadata = Some(scraped.metadata);

        // Render-then-Markdown escalation: a thin or skeleton client-rendered
        // shell has almost no real static content, but the same Tier 2 isolate
        // that leaks JSON endpoints also hydrates the DOM. When a render is needed
        // and Tier 2 is permitted, hydrate, serialize the live DOM, and re-scrape
        // it — the isolate is the browser stand-in Firecrawl uses, feeding the
        // identical HTML→Markdown transform. Upgrades `run.markdown`/`metadata` in
        // place and records `runtime.render`; leaves them untouched if it can't do
        // better.
        if needs_render && tier_max >= 2 {
            try_render_markdown(
                &mut run,
                url,
                &body,
                resp.meta.status,
                &content_type,
                incomplete,
                config,
                &opts,
                fetcher,
                capture,
            )
            .await;
        }

        if config.format == OutputFormat::Markdown && !config.discover_endpoints {
            // Terminal for the default path (Static, or RuntimeInterception if
            // the render escalation upgraded the Markdown). When discovery is
            // also requested we fall through to the discovery branch below,
            // carrying the staged Markdown into the final result.
            let tier = run.md_tier;
            return run.finish(Status::Success, Some(tier), None, None);
        }
        // `Both`: continue into the JSON ladder, carrying markdown+metadata.
    }

    // ---- API discovery (the `endpoints` format / `/v1/discover`) ----------
    // Discovery needs the Tier 2 isolate to observe the page's `fetch`/XHR, so
    // it runs its own capture here — *before* the Tier 0/1 JSON ladder, whose
    // cheap-tier early returns would otherwise preempt it. Terminal: it attaches
    // the ranked endpoint catalog (and, for `json`/`both`, the replayed winner
    // as `data`) plus any staged Markdown.
    if config.discover_endpoints {
        if tier_max >= 2 {
            return try_discover(&mut run, url, &body, config, &opts, fetcher, capture).await;
        }
        // Discovery is meaningless without the isolate; the caller capped the
        // ladder below Tier 2. Say so and finish with whatever content ran.
        run.record(
            SourceTier::RuntimeInterception,
            "runtime.discover",
            StepOutcome::Skipped,
            0,
            Bucket::None,
            Some(format!(
                "endpoint discovery needs tier_max>=2 (tier_max={tier_max})"
            )),
        );
        let tier = run.md_tier;
        return run.finish(Status::Success, Some(tier), None, None);
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
        let tier = run.md_tier;
        return run.finish(Status::Success, Some(tier), None, None);
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
/// Prefetch script subresources, run the Tier 2 capture, and record the
/// `runtime.spawn` / `runtime.sandbox` / `runtime.capture` trace steps. Shared
/// by the JSON-replay path ([`try_tier2`]) and the discovery path
/// ([`try_discover`]). On a jail/IPC failure returns `Err(terminal Error
/// result)` for the caller to return as-is; otherwise the [`CaptureResult`].
#[cfg(feature = "tier2")]
async fn run_tier2_capture<F, T>(
    run: &mut Run,
    url: &str,
    body: &str,
    config: &Config,
    opts: &SessionOpts,
    fetcher: &F,
    capture: &T,
) -> Result<crate::tier2::CaptureResult, ExtractionResult>
where
    F: PageFetcher + ?Sized,
    T: Tier2Capture + ?Sized,
{
    // The air-gapped isolate can't fetch, so the supervisor pre-fetches the
    // page's scripts (external `<script src>`, module graph) and hands them to
    // the child.
    let t_cap = Instant::now();
    let resources = prefetch_scripts(url, body, opts, fetcher).await;
    let capture_result = match capture
        .capture(url, body.as_bytes(), &resources, config)
        .await
    {
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
            return Err(owned.finish(Status::Error, None, None, Some(e)));
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
    Ok(capture_result)
}

/// API-discovery branch (`endpoints` format / `/v1/discover`): capture the
/// page's `fetch`/XHR, attach the ranked endpoint catalog to the result, and —
/// for the `json`/`both` content formats — also replay the winner as `data`
/// (discovery *and* replay). Terminal: always finishes `Success` with the
/// catalog (plus any staged Markdown), even when nothing was replayable, since
/// the discovery listing itself is the product.
#[cfg(feature = "tier2")]
async fn try_discover<F, T>(
    run: &mut Run,
    url: &str,
    body: &str,
    config: &Config,
    opts: &SessionOpts,
    fetcher: &F,
    capture: &T,
) -> ExtractionResult
where
    F: PageFetcher + ?Sized,
    T: Tier2Capture + ?Sized,
{
    use crate::tier2::{discover_endpoints, rank_and_replay};

    let capture_result =
        match run_tier2_capture(run, url, body, config, opts, fetcher, capture).await {
            Ok(c) => c,
            Err(term) => return term,
        };

    // The ranked catalog — the discovery product.
    let endpoints = discover_endpoints(&capture_result, url, config.allow_unsafe_replay);
    let n = endpoints.len();
    let replayable = endpoints.iter().filter(|e| e.replayable).count();
    run.endpoints = Some(endpoints);
    run.record(
        SourceTier::RuntimeInterception,
        "runtime.discover",
        StepOutcome::Matched,
        0,
        Bucket::None,
        Some(format!("{n} endpoint(s), {replayable} replayable")),
    );

    // For json/both, also replay the winner so `data` carries the API payload
    // (discovery + replay). Pure `endpoints`/markdown discovery skips the hop.
    let data = if matches!(config.format, OutputFormat::Json | OutputFormat::Both) {
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
                run.record(
                    SourceTier::RuntimeInterception,
                    "runtime.replay",
                    StepOutcome::Matched,
                    t_rank.elapsed().as_millis() as u64,
                    Bucket::Network,
                    Some(detail),
                );
                Some(data)
            }
            Ok(None) => None,
            Err(e) => {
                // A transport failure on the *bonus* replay must not sink the
                // discovery result — record it and still return the catalog.
                run.record(
                    SourceTier::RuntimeInterception,
                    "runtime.replay",
                    StepOutcome::Failed,
                    t_rank.elapsed().as_millis() as u64,
                    Bucket::Network,
                    Some(error_summary(&e)),
                );
                None
            }
        }
    } else {
        None
    };

    let owned = run.take_for_finish();
    owned.finish(
        Status::Success,
        Some(SourceTier::RuntimeInterception),
        data,
        None,
    )
}

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

    // Prefetch subresources, spawn/reuse the isolate, capture — shared with the
    // discovery path.
    let capture_result =
        match run_tier2_capture(run, url, body, config, opts, fetcher, capture).await {
            Ok(c) => c,
            Err(term) => return Some(term),
        };

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

/// Discovery branch for the **lean** build (no `tier2` feature): there is no
/// isolate to observe the page's `fetch`/XHR, so record a "built without tier2"
/// note and finish `Success` with no endpoint catalog (plus any staged
/// Markdown). Signature mirrors the tier2 version so the call site is
/// feature-agnostic.
#[cfg(not(feature = "tier2"))]
async fn try_discover<F, T>(
    run: &mut Run,
    _url: &str,
    _body: &str,
    _config: &Config,
    _opts: &SessionOpts,
    _fetcher: &F,
    _capture: &T,
) -> ExtractionResult
where
    F: PageFetcher + ?Sized,
    T: Tier2Capture + ?Sized,
{
    run.record(
        SourceTier::RuntimeInterception,
        "runtime.discover",
        StepOutcome::Skipped,
        0,
        Bucket::None,
        Some("built without tier2: endpoint discovery not compiled in".to_string()),
    );
    let tier = run.md_tier;
    run.take_for_finish()
        .finish(Status::Success, Some(tier), None, None)
}

/// Non-whitespace character count — the metric the thin-shell / render-gain
/// checks are expressed in (a page's real "content mass", robust to reflowed
/// whitespace).
fn nonws_len(s: &str) -> usize {
    s.chars().filter(|c| !c.is_whitespace()).count()
}

/// Pre-fetch the page's script subresources so the (air-gapped) Tier 2 isolate
/// can run external `<script src>` and resolve `import`/`import()` for
/// `type="module"` apps without ever touching the network itself.
///
/// Seeds from every `<script src>` in the HTML, then BFS-crawls the ES-module
/// graph (static + dynamic import specifiers) via `draco-net`, resolving each
/// against its importer. Bounded by a file count and total-byte cap so a
/// pathological graph can't blow up. Bare/unresolvable specifiers (npm bare
/// names, `data:` URLs) and non-2xx fetches are skipped. Best-effort: any fetch
/// error just omits that resource (the isolate degrades gracefully).
#[cfg(feature = "tier2")]
async fn prefetch_scripts<F>(
    page_url: &str,
    html: &str,
    opts: &SessionOpts,
    fetcher: &F,
) -> Vec<crate::tier2::ScriptResource>
where
    F: PageFetcher + ?Sized,
{
    use crate::tier2::ScriptResource;
    use std::collections::{HashSet, VecDeque};

    const MAX_FILES: usize = 64;
    const MAX_TOTAL_BYTES: usize = 12 * 1024 * 1024;

    let Ok(base) = url::Url::parse(page_url) else {
        return Vec::new();
    };

    let mut queue: VecDeque<String> = VecDeque::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut out: Vec<ScriptResource> = Vec::new();
    let mut total = 0usize;

    for src in scan_script_srcs(html) {
        if let Ok(u) = base.join(&src) {
            if u.scheme() == "http" || u.scheme() == "https" {
                queue.push_back(u.to_string());
            }
        }
    }

    while let Some(u) = queue.pop_front() {
        if out.len() >= MAX_FILES || total >= MAX_TOTAL_BYTES {
            break;
        }
        if !visited.insert(u.clone()) {
            continue;
        }
        let resp = match fetcher.fetch(&u, opts).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        if !(200..300).contains(&resp.meta.status) {
            continue;
        }
        let bytes = resp.body.to_vec();
        total = total.saturating_add(bytes.len());

        // Crawl this module's imports (resolved against its own URL).
        if let Ok(mod_url) = url::Url::parse(&u) {
            let src = String::from_utf8_lossy(&bytes);
            for spec in extract_module_imports(&src) {
                if let Ok(child) = mod_url.join(&spec) {
                    if (child.scheme() == "http" || child.scheme() == "https")
                        && !visited.contains(child.as_str())
                    {
                        queue.push_back(child.to_string());
                    }
                }
            }
        }

        out.push(ScriptResource {
            url: u,
            source: bytes,
        });
    }

    out
}

/// Extract every `<script … src="…">` URL from HTML, in document order.
#[cfg(feature = "tier2")]
fn scan_script_srcs(html: &str) -> Vec<String> {
    use std::sync::LazyLock;
    static RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"(?is)<script\b[^>]*?\bsrc\s*=\s*["']([^"']+)["']"#).unwrap()
    });
    RE.captures_iter(html)
        .filter_map(|c| c.get(1).map(|m| m.as_str().trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect()
}

/// Extract ES-module import/export specifiers from JS source via the **Oxc**
/// parse-once AST — static `import … from`, side-effect `import "…"`, re-export
/// `export … from` / `export * from`, and dynamic `import("…")` with a string
/// literal. A real parse (vs. regex) means specifiers inside strings/comments
/// are never matched and computed `import(expr)` is correctly ignored. Oxc
/// recovers from syntax errors, so a partial/odd bundle still yields whatever
/// specifiers it could parse.
#[cfg(feature = "tier2")]
fn extract_module_imports(src: &str) -> Vec<String> {
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, src, SourceType::mjs()).parse();
    let mr = &ret.module_record;

    let mut out = Vec::new();
    // Static import/export module requests (the map keys are the specifiers).
    for (spec, _) in mr.requested_modules.iter() {
        out.push(spec.as_str().to_string());
    }
    // Dynamic `import("…")`: the span points at the argument; keep it only when
    // it is a string literal (skip `import(dynamicExpr)`).
    for di in mr.dynamic_imports.iter() {
        let (start, end) = (
            di.module_request.start as usize,
            di.module_request.end as usize,
        );
        if let Some(slice) = src.get(start..end) {
            let t = slice.trim();
            let bytes = t.as_bytes();
            if bytes.len() >= 2
                && (bytes[0] == b'"' || bytes[0] == b'\'')
                && bytes[bytes.len() - 1] == bytes[0]
            {
                out.push(t[1..t.len() - 1].to_string());
            }
        }
    }
    out
}

/// Render-then-Markdown escalation (feature-on). Hydrate the thin shell in the
/// Tier 2 isolate, serialize the live DOM, merge it with the shell's real
/// `<head>`, and re-run the content engine. On a material content gain it
/// upgrades `run.markdown`/`run.metadata` and marks `run.md_tier` as
/// [`SourceTier::RuntimeInterception`]; otherwise it leaves the static Markdown
/// untouched. Always records a `runtime.render` trace step (and `runtime.sandbox`
/// when the child reported a posture). Never returns a result — the Markdown path
/// finalizes at its call site.
#[cfg(feature = "tier2")]
#[allow(clippy::too_many_arguments)]
async fn try_render_markdown<F, T>(
    run: &mut Run,
    url: &str,
    body: &str,
    status: u16,
    content_type: &str,
    shell_incomplete: bool,
    config: &Config,
    opts: &SessionOpts,
    fetcher: &F,
    capture: &T,
) where
    F: PageFetcher + ?Sized,
    T: Tier2Capture + ?Sized,
{
    let t_cap = Instant::now();
    let resources = prefetch_scripts(url, body, opts, fetcher).await;
    let capture_result = match capture
        .capture(url, body.as_bytes(), &resources, config)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            // A jail/IPC failure is not fatal to the Markdown path: we already
            // have the static shell Markdown staged. Record the miss and keep it.
            run.record(
                SourceTier::RuntimeInterception,
                "runtime.render",
                StepOutcome::Failed,
                t_cap.elapsed().as_millis() as u64,
                Bucket::Runtime,
                Some(error_summary(&e)),
            );
            return;
        }
    };
    let cap_ms = t_cap.elapsed().as_millis() as u64;

    // Surface the achieved sandbox posture (as the JSON Tier 2 path does).
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

    let Some(rendered) = capture_result.rendered_html.as_deref() else {
        run.record(
            SourceTier::RuntimeInterception,
            "runtime.render",
            StepOutcome::Missed,
            cap_ms,
            Bucket::Runtime,
            Some(format!("{:?}, no DOM serialized", capture_result.outcome)),
        );
        return;
    };

    // Merge the shell's real <head> (title, OG, canonical, <base>) with the
    // hydrated <body>, then re-run the identical Firecrawl-parity content engine.
    let merged = draco_static::content::merge_rendered_document(body, rendered);
    let rescraped = draco_static::content::scrape(&merged, url, status, content_type, true);

    let prev_len = run.markdown.as_deref().map(nonws_len).unwrap_or(0);
    let new_len = nonws_len(&rescraped.markdown);

    // Decide whether the rendered pass is an improvement. Two ways to win:
    //   1. It resolved a skeleton: the shell was an incomplete render and the
    //      hydrated re-scrape is no longer one (even if not longer — real content
    //      replacing `Loading…` placeholders is the win, not raw length).
    //   2. It added material content: strictly more than the shell and past the
    //      thin-shell bar (guards against hydration that produced nothing/chrome).
    // A hydration that is *still* a skeleton, or that adds nothing, is never
    // preferred — we keep the (placeholder-stripped) static shell.
    let resolved_skeleton = shell_incomplete && !rescraped.incomplete;
    let added_content = new_len > prev_len
        && !draco_static::content::is_thin_content(&rescraped.markdown, THIN_CONTENT_CHARS);

    if !rescraped.incomplete && (resolved_skeleton || added_content) {
        run.markdown = Some(rescraped.markdown);
        run.metadata = Some(rescraped.metadata);
        run.md_tier = SourceTier::RuntimeInterception;
        let why = if resolved_skeleton {
            "resolved skeleton"
        } else {
            "recovered content"
        };
        run.record(
            SourceTier::RuntimeInterception,
            "runtime.render",
            StepOutcome::Matched,
            cap_ms,
            Bucket::Runtime,
            Some(format!(
                "hydrated DOM re-scraped to {new_len} chars ({why}; shell had {prev_len})"
            )),
        );
    } else {
        let detail = if rescraped.incomplete {
            format!("hydration still a skeleton ({new_len} chars); kept static shell")
        } else {
            format!("hydration added no usable content ({new_len} vs {prev_len} chars); kept static shell")
        };
        run.record(
            SourceTier::RuntimeInterception,
            "runtime.render",
            StepOutcome::Missed,
            cap_ms,
            Bucket::Runtime,
            Some(detail),
        );
    }
}

/// Render-then-Markdown escalation (lean build, no `tier2` feature): there is no
/// isolate linked, so record that the render pass was skipped and keep the static
/// Markdown. Signature mirrors the feature-on version so the call site is
/// feature-agnostic.
#[cfg(not(feature = "tier2"))]
#[allow(clippy::too_many_arguments)]
async fn try_render_markdown<F, T>(
    run: &mut Run,
    _url: &str,
    _body: &str,
    _status: u16,
    _content_type: &str,
    _shell_incomplete: bool,
    _config: &Config,
    _opts: &SessionOpts,
    _fetcher: &F,
    _capture: &T,
) where
    F: PageFetcher + ?Sized,
    T: Tier2Capture + ?Sized,
{
    run.record(
        SourceTier::RuntimeInterception,
        "runtime.render",
        StepOutcome::Skipped,
        0,
        Bucket::None,
        Some("built without tier2: render-then-markdown not compiled in".to_string()),
    );
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
            md_tier: self.md_tier,
            endpoints: self.endpoints.take(),
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

    // ---- ES-module subresource prefetch ------------------------------

    #[test]
    fn scan_script_srcs_finds_external_scripts_only() {
        let html = r#"<html><head>
            <script src="/a.js"></script>
            <script type="module" src="https://cdn.example/b.mjs"></script>
            <script>inline(); // no src</script>
            <script src='c.js' defer></script>
          </head></html>"#;
        let srcs = scan_script_srcs(html);
        assert_eq!(srcs, vec!["/a.js", "https://cdn.example/b.mjs", "c.js"]);
    }

    #[test]
    fn extract_module_imports_covers_static_dynamic_reexport() {
        let src = r#"
            import a from "./a.js";
            import { b } from '/x/b.js';
            import "./side-effect.js";
            export { c } from "./c.js";
            export * from "./star.js";
            const p = import("./lazy.js");
            const dyn = import(computedSpecifier);
            // Oxc parses, so these decoys must NOT be extracted:
            const decoy = "import x from './evil.js'";
            // import ./commented-out.js
        "#;
        let mut got = extract_module_imports(src);
        got.sort();
        got.dedup();
        for want in [
            "./a.js",
            "/x/b.js",
            "./side-effect.js",
            "./c.js",
            "./star.js",
            "./lazy.js",
        ] {
            assert!(got.contains(&want.to_string()), "missing {want}: {got:?}");
        }
        // A real parse ignores specifiers inside string literals / comments and
        // computed dynamic imports — the whole point of using Oxc over regex.
        assert!(
            !got.iter().any(|s| s.contains("evil")),
            "false match: {got:?}"
        );
        assert!(
            !got.iter().any(|s| s.contains("commented")),
            "false match: {got:?}"
        );
        assert!(
            !got.iter().any(|s| s == "computedSpecifier"),
            "computed import matched: {got:?}"
        );
    }

    #[tokio::test]
    async fn prefetch_scripts_fetches_external_seed_scripts() {
        // Each fetched script (fixed mock body) is returned as a ScriptResource,
        // keyed by its URL resolved against the page.
        let html = r#"<html><head>
            <script src="/static/app.js"></script>
            <script type="module" src="/static/entry.mjs"></script>
          </head><body></body></html>"#;
        let fetcher = MockFetcher::ok_html(200, "console.log('bundle');");
        let opts = SessionOpts::default();
        let res = prefetch_scripts("https://shop.example.com/p", html, &opts, &fetcher).await;
        let urls: Vec<&str> = res.iter().map(|r| r.url.as_str()).collect();
        assert!(
            urls.contains(&"https://shop.example.com/static/app.js"),
            "{urls:?}"
        );
        assert!(
            urls.contains(&"https://shop.example.com/static/entry.mjs"),
            "{urls:?}"
        );
        assert!(res.iter().all(|r| !r.source.is_empty()));
    }

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
    async fn discovery_attaches_ranked_endpoint_catalog() {
        // With discover_endpoints set, the run captures the page's fetch/XHR and
        // attaches the ranked catalog — regardless of whether any winner is
        // replayable — via the dedicated discovery branch (before Tier 0/1).
        let fetcher = MockFetcher::ok_html(200, "<html>spa</html>");
        let statics = MockStatic::miss_no_build_id();
        let capture = MockCapture::with_candidates(vec![
            Candidate::get("https://x.com/api/items?page=1", InterceptVia::Fetch),
            Candidate::get(
                "https://www.google-analytics.com/collect",
                InterceptVia::Xhr,
            ),
        ]);
        let config = Config {
            format: OutputFormat::Json,
            tier_max: 2,
            discover_endpoints: true,
            ..Config::default()
        };
        let r = run_ladder("https://x.com/p", &config, &fetcher, &statics, &capture).await;

        assert_eq!(r.status, Status::Success);
        assert_eq!(r.source_tier, Some(SourceTier::RuntimeInterception));
        let eps = r.endpoints.expect("catalog attached");
        assert_eq!(eps.len(), 2);
        // Ranked: the same-origin JSON API outranks the analytics beacon.
        assert_eq!(eps[0].url, "https://x.com/api/items?page=1");
        assert!(eps[0].score >= eps[1].score);
        assert!(eps[0].replayable && !eps[1].replayable);
        // The discovery step is recorded in the trace.
        assert!(r.trace.iter().any(|t| t.action == "runtime.discover"));
    }

    #[tokio::test]
    async fn discovery_capped_below_tier2_is_noted_without_catalog() {
        // Discovery needs the isolate; when the ladder is capped below Tier 2 it
        // records a skip and returns no catalog rather than pretending.
        let fetcher = MockFetcher::ok_html(200, "<html>spa</html>");
        let statics = MockStatic::miss_no_build_id();
        let capture = MockCapture::empty();
        let config = Config {
            format: OutputFormat::Json,
            tier_max: 1,
            discover_endpoints: true,
            ..Config::default()
        };
        let r = run_ladder("https://x.com/p", &config, &fetcher, &statics, &capture).await;
        assert!(r.endpoints.is_none(), "no catalog when capped below tier 2");
        let step = r
            .trace
            .iter()
            .find(|t| t.action == "runtime.discover")
            .unwrap();
        assert_eq!(step.outcome, StepOutcome::Skipped);
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
        // A normal (non-thin) content page: static extraction already found the
        // article, so the render escalation must not fire.
        let article = "# Hi\n\n".to_string() + &"Some real body text here. ".repeat(20);
        let statics = MockStatic::default().with_markdown(&article);
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
        assert_eq!(r.markdown.as_deref(), Some(article.as_str()));
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
    async fn markdown_thin_spa_escalates_to_render() {
        // A thin shell triggers the render escalation. With a capture double that
        // returns no serialized DOM, the run still succeeds on the shell markdown
        // but the trace shows the attempted render pass (Missed, no DOM).
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
        // Still Static: no DOM came back, so the shell markdown is retained.
        assert_eq!(r.source_tier, Some(SourceTier::Static));

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
                .contains("escalating to render"),
            "thin content should note the render escalation: {:?}",
            md_step.detail
        );
        // The render pass was attempted and recorded as a miss (no DOM serialized).
        let render_step = r
            .trace
            .iter()
            .find(|t| t.action == "runtime.render")
            .expect("a runtime.render step should be recorded");
        assert_eq!(render_step.outcome, StepOutcome::Missed);
    }

    #[tokio::test]
    async fn markdown_render_escalation_upgrades_thin_shell() {
        // The full render-then-Markdown win: a thin shell whose Tier 2 hydration
        // returns a content-rich serialized DOM. The engine re-scrapes it, the
        // Markdown is upgraded, and the source tier becomes RuntimeInterception.
        let shell = "<html><head><title>Docs</title>\
            <meta property=\"og:title\" content=\"Realtime Docs\"></head>\
            <body><div id=app></div></body></html>";
        let fetcher = MockFetcher::ok_html(200, shell)
            .with_header("content-type", "text/html; charset=utf-8");
        let statics = MockStatic::default().with_markdown("Loading…"); // thin shell

        // The hydrated DOM the isolate "serialized": a real article body.
        let hydrated = format!(
            "<html><head></head><body><main><article><h1>Realtime Docs</h1>{}</article></main></body></html>",
            "<p>Draco hydrates the SPA in a jitless V8 isolate, serializes the live DOM, \
             and re-runs the Firecrawl-parity content engine over it to produce Markdown.</p>"
                .repeat(3)
        );
        let capture = MockCapture::rendered(hydrated);

        let r = run_ladder(
            "https://docs.example/guide",
            &cfg_markdown(),
            &fetcher,
            &statics,
            &capture,
        )
        .await;

        assert_eq!(r.status, Status::Success);
        assert_eq!(
            r.source_tier,
            Some(SourceTier::RuntimeInterception),
            "a successful render escalation should be attributed to Tier 2"
        );
        let md = r.markdown.expect("markdown present");
        assert!(
            md.contains("Realtime Docs") && md.contains("Firecrawl-parity content engine"),
            "upgraded markdown should carry the hydrated article: {md:?}"
        );
        assert!(
            !draco_static::content::is_thin_content(&md, THIN_CONTENT_CHARS),
            "upgraded markdown should no longer be thin"
        );
        // Metadata is recovered from the shell's real <head> via the merge.
        let meta = r.metadata.expect("metadata present");
        assert_eq!(meta["title"], "Docs");
        assert_eq!(meta["og:title"], "Realtime Docs");

        let render_step = r
            .trace
            .iter()
            .find(|t| t.action == "runtime.render")
            .expect("a runtime.render step should be recorded");
        assert_eq!(render_step.outcome, StepOutcome::Matched);
    }

    #[tokio::test]
    async fn markdown_thin_shell_render_skipped_when_tier_capped() {
        // With `--tier-max 1`, a thin shell must NOT boot the isolate: the render
        // escalation is gated on tier_max >= 2. The shell markdown is returned and
        // the trace says the render was skipped.
        let fetcher = MockFetcher::ok_html(200, "<html><body><div id=root></div></body></html>");
        let statics = MockStatic::default().with_markdown("x"); // thin
        let capture = MockCapture::failing(DracoError::Jail {
            reason: draco_types::JailKind::Killed,
            detail: "must not be reached when tier-capped".into(),
        });
        let mut cfg = cfg_markdown();
        cfg.tier_max = 1;
        let r = run_ladder("https://spa.example/", &cfg, &fetcher, &statics, &capture).await;

        assert_eq!(r.status, Status::Success);
        assert_eq!(r.source_tier, Some(SourceTier::Static));
        assert_eq!(capture.calls(), 0, "isolate must not boot when tier-capped");
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
                .contains("render skipped"),
            "tier-capped thin shell should note render skipped: {:?}",
            md_step.detail
        );
    }

    #[tokio::test]
    async fn markdown_skeleton_shell_escalates_even_when_not_thin() {
        // The Target.com failure mode: a chrome-heavy shell that is NOT thin (lots
        // of nav/promo copy) but is an incomplete render (skeleton `Loading…`
        // rails). It must still escalate to the render pass — char count alone
        // would have wrongly returned the skeleton.
        let fetcher = MockFetcher::ok_html(200, "<html><body><div id=root></div></body></html>");
        // Long, non-thin chrome + flagged incomplete (as the real engine would).
        let chrome = "# Store\n\n".to_string() + &"Featured category link. ".repeat(30);
        let statics = MockStatic::default()
            .with_markdown(&chrome)
            .with_incomplete(true);

        // A hydration that resolves the skeleton into real content.
        let hydrated = format!(
            "<html><head></head><body><main>{}</main></body></html>",
            "<p>A real product rail with items that only appear after hydration completes.</p>"
                .repeat(3)
        );
        let capture = MockCapture::rendered(hydrated);

        let r = run_ladder(
            "https://shop.example/",
            &cfg_markdown(),
            &fetcher,
            &statics,
            &capture,
        )
        .await;

        assert_eq!(r.status, Status::Success);
        assert_eq!(capture.calls(), 1, "a skeleton shell must boot the isolate");
        assert_eq!(
            r.source_tier,
            Some(SourceTier::RuntimeInterception),
            "resolved skeleton should be attributed to the render pass"
        );
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
                .contains("incomplete render"),
            "skeleton shell should note the incomplete render: {:?}",
            md_step.detail
        );
        assert!(r.markdown.unwrap().contains("real product rail"));
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
