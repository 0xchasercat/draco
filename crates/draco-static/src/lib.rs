//! # draco-static (STUB — WS-B)
//!
//! Tier 0 static extraction + Tier 1 build-id URL construction. Implement against
//! canonical spec §10. Pure and synchronous: bytes in, structured data out.
//!
//! **Frozen public API** — fill in the bodies; do not change the signatures.
#![allow(dead_code, unused_variables)]

use draco_types::ExtractedData;

/// Result of a Tier 0 static extraction attempt.
#[derive(Debug, Clone)]
pub enum StaticOutcome {
    /// A paradigm matched.
    Hit(ExtractedData),
    /// Nothing matched; caller should escalate.
    Miss,
}

/// Tier 0: scan raw HTML for `__NEXT_DATA__`, JSON-LD, and object-literal `window.__NUXT__`.
pub fn extract_static(html: &str) -> StaticOutcome {
    todo!("WS-B: implement Tier 0 matchers per canonical spec §10")
}

/// Tier 1: discover a Next.js build id from the HTML, if present.
pub fn discover_build_id(html: &str) -> Option<String> {
    todo!("WS-B: implement build-id discovery per canonical spec §10")
}

/// Tier 1: construct the `_next/data/<build_id><pathname>.json` URL for a route.
pub fn next_data_url(build_id: &str, pathname: &str, query: &[(String, String)]) -> String {
    todo!("WS-B: implement _next/data URL construction per canonical spec §10")
}

/// Detect Next.js **app-router** (RSC) pages, which are NOT Tier-1 eligible in v0.1.
pub fn is_app_router(html: &str) -> bool {
    todo!("WS-B: implement app-router detection per canonical spec §10")
}
