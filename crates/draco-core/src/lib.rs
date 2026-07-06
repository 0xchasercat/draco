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
/// The warm Tier 2 worker pool for the daemon (real under `tier2`, a
/// finalizes-`Unsupported` stub in the lean build). Paired with
/// [`extract_with_pool`].
pub use tier2::Tier2Pool;

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
    best_candidate, best_replayable, is_read_style_post, is_safe_method, score_request, Candidate,
    MIN_VIABLE_SCORE, PENALTY_ANALYTICS, PENALTY_STATIC_ASSET, SCORE_API_PATH, SCORE_JSON,
    SCORE_SAME_ORIGIN,
};

/// What `draco extract` should produce.
///
/// The default is [`OutputFormat::Markdown`]: Draco is first a Firecrawl-style
/// scraper (URL → clean Markdown + metadata), and that path is the fast one — it
/// never touches V8/the jail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    /// Clean Markdown + metadata of the page's main content (the fast,
    /// static-only path). **Default.**
    #[default]
    Markdown,
    /// The tiered JSON-API extraction (Tier 0 embedded state → Tier 1 build-id
    /// → Tier 2 runtime interception), populating `data`.
    Json,
    /// Both: Markdown + metadata AND the JSON-API extraction.
    Both,
}

/// Orchestration configuration, assembled by the CLI from flags/env/config file.
#[derive(Debug, Clone)]
pub struct Config {
    /// What to produce (Markdown / JSON / both). Default: Markdown.
    pub format: OutputFormat,
    pub proxy: Option<String>,
    pub delay_ms: u64,
    pub timeout_ms: u64,
    pub respect_robots: bool,
    /// Cap the escalation ladder: 0 = static only, 1 = +build-id, 2 = +runtime.
    pub tier_max: u8,
    pub capture_window_ms: u64,
    /// Skip OS-level sandbox hardening for Tier 2. The V8 isolate still runs with
    /// no host-capability bindings (page JS cannot reach the network, filesystem,
    /// or processes); only the defense-in-depth OS sandbox (seccomp/netns/
    /// Landlock) is skipped. On non-Linux this is a no-op (there is no OS sandbox
    /// to skip). Set via the CLI `--no-jail` flag.
    pub no_jail: bool,
    /// Use the strict default-deny seccomp allowlist instead of the default robust
    /// denylist (Linux, jailed path only). Maximum hardening but may need per-host
    /// tuning. Set via the CLI `--strict-sandbox` flag. Off by default.
    pub strict_sandbox: bool,
    /// Allow Tier 2 to replay a state-changing request (an unsafe HTTP method
    /// that is not a GraphQL/JSON-RPC read) that the ranking picked. Off by
    /// default: mutation-safety withholds such a request from replay and the run
    /// falls through to `Unsupported` (see [`ranking::best_replayable`]). Set via
    /// the CLI `--allow-unsafe-replay` flag when the side effect is intended.
    pub allow_unsafe_replay: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            format: OutputFormat::Markdown,
            proxy: None,
            delay_ms: 0,
            timeout_ms: 30_000,
            respect_robots: true,
            tier_max: 2,
            capture_window_ms: 2_000,
            no_jail: false,
            strict_sandbox: false,
            allow_unsafe_replay: false,
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

/// Like [`extract`], but routes the Tier 2 capture through a warm
/// [`Tier2Pool`] instead of spawning a fresh jailed child per scrape. Intended
/// for the long-lived daemon, where the pool amortizes the process + sandbox
/// setup across requests; the CLI keeps using [`extract`]. Same guarantees:
/// never panics, never returns `Err` — every outcome is in the result.
///
/// Each capture still runs in a fresh isolate inside a reused worker process, so
/// there is no cross-scrape state bleed (see [`Tier2Pool`]). A request whose
/// sandbox posture differs from the pool's transparently falls back to a
/// one-shot spawn.
pub async fn extract_with_pool(url: &str, config: &Config, pool: &Tier2Pool) -> ExtractionResult {
    machine::run_with_pool(url, config, pool).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use draco_types::{Status, Timing};

    #[test]
    fn config_default_is_markdown_first_with_full_ladder_available() {
        let c = Config::default();
        // Default output is Markdown (Firecrawl-style scrape).
        assert_eq!(c.format, OutputFormat::Markdown);
        // The JSON ladder ceiling is still fully available for --format json/both.
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
