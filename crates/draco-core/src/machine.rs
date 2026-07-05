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
use draco_static::StaticOutcome;
use draco_types::{
    DracoError, ExtractionResult, SourceTier, Status, StepOutcome, Timing, TraceStep,
};

use crate::challenge::detect_challenge;
use crate::fetcher::{NetFetcher, PageFetcher};
use crate::Config;

// ---------------------------------------------------------------------------
// Tier ceilings (spec §11 `tier_max`)
// ---------------------------------------------------------------------------

/// Highest tier index Draco implements (2 = runtime interception).
pub const TIER_CEILING: u8 = 2;

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
pub(crate) async fn run(url: &str, config: &Config) -> ExtractionResult {
    run_ladder(url, config, &NetFetcher, &ProdStatic).await
}

/// The escalation ladder, generic over its two effect seams so it can be
/// exercised offline. See module docs.
pub(crate) async fn run_ladder<F, S>(
    url: &str,
    config: &Config,
    fetcher: &F,
    statics: &S,
) -> ExtractionResult
where
    F: PageFetcher + ?Sized,
    S: StaticEngine + ?Sized,
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
        // TODO(Slice 4): boot the jail (`draco_jail::spawn_jail`), Hydrate the
        // captured `body`, collect `JailToSupervisor::Intercept` frames into
        // `ranking::Candidate`s, pick `ranking::best_candidate`, replay it via
        // `fetcher.replay`, and (on a JSON body) finalize
        // `Status::Success` / `SourceTier::RuntimeInterception`. The ranking
        // policy and replay seam are already in place; this hook wires them.
        #[cfg(feature = "tier2")]
        {
            compile_error!("Tier 2 not implemented in WS-C; enable in Slice 4");
        }
        run.record(
            SourceTier::RuntimeInterception,
            "runtime.capture",
            StepOutcome::Skipped,
            0,
            Bucket::None,
            Some("tier 2 not implemented (Slice 4)".to_string()),
        );
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
    run.finish(Status::Unsupported, None, None, None)
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
    use crate::testutil::{err_fetcher, MockFetcher, MockStatic};
    use draco_types::{ExtractOrigin, ExtractedData, NetKind};
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

    fn cfg(tier_max: u8) -> Config {
        Config {
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
        let r = run_ladder("https://x.com", &cfg(2), &fetcher, &statics).await;
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
        let r = run_ladder("https://x.com/p", &cfg(2), &fetcher, &statics).await;
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
        let r = run_ladder("https://x.com/p", &cfg(2), &fetcher, &statics).await;
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
        let r = run_ladder("https://shop.example.com/p/1", &cfg(2), &fetcher, &statics).await;
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
        let r = run_ladder("https://x.com/p", &cfg(2), &fetcher, &statics).await;
        // No build-id attempt; falls through to unsupported (tier 2 not impl).
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
        let r = run_ladder("https://x.com/p", &cfg(0), &fetcher, &statics).await;
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
        let r = run_ladder("https://x.com/p", &cfg(1), &fetcher, &statics).await;
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
    async fn tier2_reached_but_unimplemented_is_unsupported() {
        let fetcher = MockFetcher::ok_html(200, "<html>spa</html>");
        let statics = MockStatic::miss_no_build_id();
        let r = run_ladder("https://x.com/p", &cfg(2), &fetcher, &statics).await;
        assert_eq!(r.status, Status::Unsupported);
        let t2 = r
            .trace
            .iter()
            .find(|t| t.action == "runtime.capture")
            .unwrap();
        assert_eq!(t2.outcome, StepOutcome::Skipped);
        assert!(t2.detail.as_deref().unwrap().contains("Slice 4"));
    }

    #[tokio::test]
    async fn timing_buckets_are_attributed() {
        // Tier 0 hit: one network hop (the mock reports elapsed_ms = 1) and one
        // parse step. total_ms is stamped from the wall clock independently and
        // may be 0 for a sub-ms mock run, so it is *not* comparable to the mock's
        // fabricated network_ms — assert each bucket on its own terms.
        let fetcher = MockFetcher::ok_html(200, "<html>x</html>");
        let statics = MockStatic::hit_next_data();
        let r = run_ladder("https://x.com", &cfg(0), &fetcher, &statics).await;
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
        let r = run_ladder("https://x.com/p", &cfg(2), &fetcher, &statics).await;
        assert_eq!(r.status, Status::Unsupported);
        let replay = r.trace.iter().find(|t| t.action == "tier1.replay").unwrap();
        assert_eq!(replay.outcome, StepOutcome::Missed);
    }
}
