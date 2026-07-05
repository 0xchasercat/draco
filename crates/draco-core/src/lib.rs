//! # draco-core — escalation state machine (WS-C: Tiers 0/1)
//!
//! The orchestrator. [`extract`] runs a single URL through the tiered ladder of
//! spec §11 — `Fetch → Tier0 → Tier1 → Tier2 → Finalize` — stopping at the
//! cheapest tier that yields data:
//!
//! - **Fetch** — one Tier 0 GET (via the [`PageFetcher`] seam), then a
//!   [challenge short-circuit](challenge): a recognized bot-wall finalizes
//!   [`Status::NeedsBrowser`] without spending further compute.
//! - **Tier 0** — static embedded state (`__NEXT_DATA__`, JSON-LD, `__NUXT__`)
//!   via `draco-static`.
//! - **Tier 1** — Next.js build-id `_next/data` replay.
//! - **Tier 2** — runtime interception + [ranked](ranking) replay. The ranking
//!   policy and replay seam ship now; the isolate wiring lands in **Slice 4**
//!   (a marked hook in [`machine`]).
//! - **Finalize** — assemble the [`Timing`] breakdown and the
//!   [`TraceStep`](draco_types::TraceStep) list into an [`ExtractionResult`].
//!
//! ## Effect seams (offline testability)
//!
//! The machine touches the network only through [`PageFetcher`] and the static
//! extractors only through [`StaticEngine`](machine::StaticEngine). In WS-C
//! both `draco-net` and `draco-static` are still `todo!()` stubs, so the whole
//! ladder is unit-tested against mock implementations of these two traits —
//! the crate's own tests never call the stubs.
//!
//! [`extract`] returns a well-formed [`ExtractionResult`] for every input, so
//! the CLI runs end-to-end even though live Tier 0/1 needs the sibling crates.
#![allow(dead_code, unused_variables)]

use draco_types::ExtractionResult;

mod challenge;
mod fetcher;
mod machine;
mod ranking;
#[cfg(test)]
mod testutil;
/// Tier 2 supervisor wiring (jail-hosted V8 capture → ranked replay). Always
/// compiled: the capture *seam* + rank/replay logic are V8-free. Only the
/// production capture seam that actually spawns the jail is behind the `tier2`
/// feature — the lean build uses a disabled seam that reports "built without
/// tier2" and finalizes `Unsupported`.
mod tier2;

// ---- Public API -----------------------------------------------------------

pub use challenge::{detect_challenge, ChallengeKind};
pub use fetcher::{NetFetcher, PageFetcher};
pub use machine::{clamp_tier_max, session_opts, ProdStatic, StaticEngine, TIER_CEILING};

/// Re-export the jailed-child entry so the CLI's `__jail` re-exec hook can call
/// it without depending on `draco-jail` directly. Only present with `tier2` on;
/// the lean CLI build has no `__jail` hook and never references this.
///
/// `run_jail_child` is the `draco __jail` child entry (arms the sandbox, hosts
/// the V8 capture, and never returns). `spawn_jail` is the supervisor-side spawn,
/// re-exported for completeness / external drivers.
#[cfg(feature = "tier2")]
pub use draco_jail::{run_jail_child, spawn_jail, JailHandle};
pub use ranking::{
    best_candidate, score_request, Candidate, MIN_VIABLE_SCORE, PENALTY_ANALYTICS,
    PENALTY_STATIC_ASSET, SCORE_API_PATH, SCORE_JSON, SCORE_SAME_ORIGIN,
};

/// Orchestration configuration, assembled by the CLI from flags/env/config file.
#[derive(Debug, Clone)]
pub struct Config {
    pub proxy: Option<String>,
    pub delay_ms: u64,
    pub timeout_ms: u64,
    pub respect_robots: bool,
    /// Cap the escalation ladder: 0 = static only, 1 = +build-id, 2 = +runtime.
    pub tier_max: u8,
    pub capture_window_ms: u64,
    /// Dev-only: run Tier 2 un-jailed.
    pub no_jail: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            proxy: None,
            delay_ms: 0,
            timeout_ms: 30_000,
            respect_robots: true,
            tier_max: 2,
            capture_window_ms: 2_000,
            no_jail: false,
        }
    }
}

/// Top-level entry: run the escalation ladder for a single URL.
///
/// Never panics and never returns `Err`: every outcome — success, unsupported,
/// challenge, or hard failure — is encoded in the returned [`ExtractionResult`]
/// (see its `status`/`error` fields). This is the sole public entry point; the
/// tier sequencing lives in [`machine`].
pub async fn extract(url: &str, config: &Config) -> ExtractionResult {
    machine::run(url, config).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use draco_types::{Status, Timing};

    #[test]
    fn config_default_runs_full_ladder() {
        let c = Config::default();
        assert_eq!(c.tier_max, 2);
        assert!(c.respect_robots);
    }

    #[test]
    fn timing_default_is_zeroed() {
        let t = Timing::default();
        assert_eq!(t.total_ms, 0);
    }

    // The production `extract` path drives the real (stubbed) draco-net, which
    // panics. It is validated end-to-end after integration.
    #[tokio::test]
    #[ignore = "runs after integration: production extract() calls draco-net (todo! stub)"]
    async fn extract_smoke() {
        let r = extract("https://example.com", &Config::default()).await;
        assert_ne!(r.status, Status::Error);
    }
}
