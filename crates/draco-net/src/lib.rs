//! # draco-net (STUB — WS-A)
//!
//! Stealth TLS/JA4 HTTP client. Implement against canonical spec §9:
//! wreq-backed fetch with a faithful Chrome JA4/HTTP-2 fingerprint, cookie jar,
//! `--proxy`, `--delay`, robots.txt, and 429/503 backoff.
//!
//! **Frozen public API** — fill in the bodies; do not change the signatures.
#![allow(dead_code, unused_variables)]

use bytes::Bytes;
use draco_types::{DracoError, HttpRequestSpec, HttpResponseMeta};

/// In-process HTTP response. Not serialized; the raw body is carried as bytes.
#[derive(Debug, Clone)]
pub struct HtmlResponse {
    pub meta: HttpResponseMeta,
    pub body: Bytes,
}

/// Per-session network options.
#[derive(Debug, Clone)]
pub struct SessionOpts {
    pub proxy: Option<String>,
    pub delay_ms: u64,
    pub respect_robots: bool,
    pub timeout_ms: u64,
}

impl Default for SessionOpts {
    fn default() -> Self {
        Self {
            proxy: None,
            delay_ms: 0,
            respect_robots: true,
            timeout_ms: 30_000,
        }
    }
}

/// Tier 0 entry: fetch a page with a browser-faithful fingerprint.
pub async fn fetch_target(url: &str, opts: &SessionOpts) -> Result<HtmlResponse, DracoError> {
    todo!("WS-A: implement wreq-backed fetch_target per canonical spec §9")
}

/// Replay a constructed (Tier 1) or intercepted (Tier 2) request with the same client.
pub async fn replay(
    spec: &HttpRequestSpec,
    opts: &SessionOpts,
) -> Result<HtmlResponse, DracoError> {
    todo!("WS-A: implement replay per canonical spec §9")
}
