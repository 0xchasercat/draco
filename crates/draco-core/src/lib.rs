//! # draco-core (STUB — WS-C + Slice 4)
//!
//! The escalation state machine. Implement against canonical spec §11:
//! `Fetch → Tier0 → Tier1 → Tier2 → Finalize`, with the challenge short-circuit,
//! the intercept ranking policy, replay via `draco-net`, and trace/timing
//! assembly. WS-C delivers Tiers 0/1 (no jail); Slice 4 wires Tier 2.
//!
//! `extract` returns a valid (stub) `ExtractionResult` today so the CLI runs
//! end-to-end; replace the body with the real ladder.
#![allow(dead_code, unused_variables)]

use draco_types::{DracoError, ExtractionResult, Status, Timing};

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
pub async fn extract(url: &str, config: &Config) -> ExtractionResult {
    // STUB (WS-C): implement Fetch → Tier0 → Tier1 → Tier2 → Finalize per spec §11.
    ExtractionResult {
        url: url.to_string(),
        status: Status::Error,
        source_tier: None,
        data: None,
        timing: Timing::default(),
        trace: Vec::new(),
        error: Some(DracoError::Config {
            detail: "draco-core::extract is a stub (WS-C) — implement the ladder per spec §11"
                .to_string(),
        }),
    }
}
