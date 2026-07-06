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

/// The set of outputs a scrape should produce — the multi-select `formats` of
/// the Firecrawl-style request, replacing the old coarse three-way enum.
///
/// Each flag is an independent output; a request may ask for any combination.
/// The default is `markdown` alone: Draco is first a Firecrawl-style scraper
/// (URL → clean Markdown + metadata), and that path is the fast one — it never
/// touches V8/the jail.
///
/// - `markdown` — clean Markdown of the main content (+ `metadata`).
/// - `html` — cleaned, absolutized main-content HTML.
/// - `raw_html` — the unmodified fetched HTML.
/// - `links` — every absolutized `<a href>` on the page.
/// - `json` — the tiered JSON-API extraction (Tier 0 → 1 → 2), populating `data`.
/// - `endpoints` — the ranked catalog of API endpoints the page's JS called
///   (the `endpoints` format / `/v1/discover`); forces the Tier 2 capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatSet {
    pub markdown: bool,
    pub html: bool,
    pub raw_html: bool,
    pub links: bool,
    pub json: bool,
    pub endpoints: bool,
}

impl Default for FormatSet {
    /// Markdown-only, matching Firecrawl's default `["markdown"]`.
    fn default() -> Self {
        Self {
            markdown: true,
            html: false,
            raw_html: false,
            links: false,
            json: false,
            endpoints: false,
        }
    }
}

impl FormatSet {
    /// The empty set (no outputs). Building block for the constructors below.
    pub fn none() -> Self {
        Self {
            markdown: false,
            html: false,
            raw_html: false,
            links: false,
            json: false,
            endpoints: false,
        }
    }

    /// Markdown only — the default scrape.
    pub fn markdown_only() -> Self {
        Self::default()
    }

    /// The JSON-API extraction only.
    pub fn json_only() -> Self {
        Self {
            json: true,
            ..Self::none()
        }
    }

    /// Any output derived from the fetched/rendered HTML (markdown / html /
    /// links) is requested — i.e. the static scrape + DOM pre-processing must run.
    pub fn wants_static_content(&self) -> bool {
        self.markdown || self.html || self.links
    }

    /// The tiered JSON-API extraction (populating `data`) is requested.
    pub fn wants_data(&self) -> bool {
        self.json
    }

    /// Only HTML-derived content was asked for (no `data`, no `endpoints`), so
    /// the run can return after the static scrape without entering the JSON
    /// ladder. When `false`, the ladder (and/or discovery) still has work to do.
    pub fn is_static_terminal(&self) -> bool {
        self.wants_static_content() && !self.json && !self.endpoints
    }
}

/// Orchestration configuration, assembled by the CLI from flags/env/config file.
#[derive(Debug, Clone)]
pub struct Config {
    /// What to produce — the set of requested output formats. Default: markdown.
    pub formats: FormatSet,
    /// Strip boilerplate (nav/header/footer/ads) to the main content —
    /// Firecrawl's `onlyMainContent`. Applies to the `markdown` and `html`
    /// formats (`rawHtml` is always the unmodified fetch). Default: `true`.
    pub only_main_content: bool,
    /// CSS selectors to keep — Firecrawl's `includeTags`. When non-empty, only
    /// matching subtrees survive into the `markdown`/`html` formats; empty means
    /// the whole page. Applied before `only_main_content`.
    pub include_tags: Vec<String>,
    /// CSS selectors to drop before extraction — Firecrawl's `excludeTags`.
    pub exclude_tags: Vec<String>,
    /// Extra request headers forwarded to every outbound fetch — Firecrawl's
    /// `headers` (custom UA, cookies, auth). Ordered; empty by default.
    pub headers: Vec<(String, String)>,
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
            formats: FormatSet::markdown_only(),
            only_main_content: true,
            include_tags: Vec::new(),
            exclude_tags: Vec::new(),
            headers: Vec::new(),
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
        assert_eq!(c.formats, FormatSet::markdown_only());
        assert!(c.formats.markdown && !c.formats.json && !c.formats.endpoints);
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
