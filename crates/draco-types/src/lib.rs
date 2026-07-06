//! # draco-types
//!
//! The **frozen** v0.1 wire & result contract for Draco. Every other crate codes
//! against these types; this crate depends on nothing internal and performs no I/O.
//!
//! Serialization conventions:
//! - All enums are `snake_case` on the wire.
//! - Tagged unions use an explicit tag (`kind` for errors, `t` for IPC frames).
//! - Ranking of intercepted requests is intentionally **not** on the wire — it
//!   lives in `draco-core` so policy can change without touching the sandbox.
#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ===================================================================
// Public result contract (draco-cli emits `ExtractionResult` as JSON)
// ===================================================================

/// Terminal status of an extraction run.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// A tier succeeded; `data` is populated.
    Success,
    /// The pipeline ran to completion but no extractor matched.
    Unsupported,
    /// A JS challenge / bot-wall was detected; a real browser is required.
    NeedsBrowser,
    /// Internal failure; see `error`.
    Error,
}

/// Which tier produced a successful extraction.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceTier {
    /// Tier 0: static embedded state.
    Static,
    /// Tier 1: Next.js build-id `_next/data` replay.
    HeuristicApiReplay,
    /// Tier 2: runtime fetch/XHR interception.
    RuntimeInterception,
}

/// Millisecond timing breakdown.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Timing {
    /// Sum of all `draco-net` wall time.
    pub network_ms: u64,
    /// Tier 0/1 AST parse work.
    pub parse_ms: u64,
    /// Tier 2 isolate wall time (0 if never reached).
    pub runtime_ms: u64,
    /// End-to-end total.
    pub total_ms: u64,
}

/// Outcome of a single escalation step, recorded in the trace.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    Matched,
    Missed,
    Skipped,
    Failed,
}

/// One entry in the escalation trace.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TraceStep {
    pub tier: SourceTier,
    /// Dotted action name, e.g. `"net.fetch"`, `"static.next_data"`, `"tier1.build_id"`, `"runtime.capture"`.
    pub action: String,
    pub outcome: StepOutcome,
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The complete, machine-parseable result of `draco extract`.
///
/// Historically this carried only `data` (the tiered JSON-API extraction). The
/// Markdown-scrape flow (Firecrawl-style `URL → Markdown + metadata`) adds two
/// **additive** fields, `markdown` and `metadata`; both are `Option` and elided
/// from the wire when absent, so every pre-existing JSON consumer of the old
/// shape keeps working.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtractionResult {
    pub url: String,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_tier: Option<SourceTier>,
    /// Present iff `status == Success`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// Clean Markdown of the page's main content (the default Markdown-scrape
    /// output). Present when the Markdown path ran (`format` = markdown / both).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    /// Flat page metadata (title, description, `og:*`/`twitter:*`, canonical,
    /// favicon, language, plus `sourceURL`/`url`/`statusCode`/`contentType`).
    /// Populated alongside `markdown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    /// Cleaned, absolutized HTML of the page's main content — the `html` format.
    /// Script/style/chrome stripped and relative URLs resolved (the same DOM
    /// pre-processing that feeds the Markdown transform). `None` unless requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    /// The unmodified fetched HTML — the `rawHtml` format. `None` unless requested.
    #[serde(default, rename = "rawHtml", skip_serializing_if = "Option::is_none")]
    pub raw_html: Option<String>,
    /// Every absolutized `<a href>` found on the page — the `links` format.
    /// `None` unless requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub links: Option<Vec<String>>,
    /// The ranked catalog of JSON/XHR API endpoints the page's own JavaScript
    /// called, discovered during the Tier 2 capture (the `endpoints` format /
    /// `/v1/discover`). `Some` only when discovery was requested and the isolate
    /// ran; elided from the wire otherwise so existing consumers are unaffected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoints: Option<Vec<DiscoveredEndpoint>>,
    pub timing: Timing,
    pub trace: Vec<TraceStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<DracoError>,
}

/// One API endpoint discovered by the Tier 2 isolate — a `fetch`/XHR the page's
/// JavaScript issued, surfaced with its ranking so a caller can see (and choose
/// to replay) the JSON APIs behind a client-rendered page. Serialized camelCase
/// to match the Firecrawl-style wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredEndpoint {
    /// HTTP method the page used (`GET`, `POST`, …).
    pub method: String,
    /// Absolute request URL.
    pub url: String,
    /// Transport the page used to issue it.
    pub via: InterceptVia,
    /// Draco's replay-desirability score (higher = more likely the real data
    /// API); see `draco_core::score_request`.
    pub score: i32,
    /// Whether Draco would replay this endpoint: score clears the viability bar
    /// and the method is replay-safe (or unsafe replay was explicitly allowed).
    pub replayable: bool,
    /// Request headers the page sent, in order (fingerprint-relevant for a
    /// faithful replay).
    pub headers: Vec<(String, String)>,
}

// ===================================================================
// Error taxonomy
// ===================================================================

/// Structured error surfaced in `ExtractionResult::error`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DracoError {
    Network {
        reason: NetKind,
        detail: String,
    },
    Parse {
        detail: String,
    },
    Jail {
        reason: JailKind,
        detail: String,
    },
    /// Sanitized V8 exception summary.
    Runtime {
        detail: String,
    },
    Ipc {
        detail: String,
    },
    Config {
        detail: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetKind {
    Dns,
    Tls,
    Timeout,
    Status,
    TooManyRedirects,
    Proxy,
    Body,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JailKind {
    Spawn,
    SeccompInstall,
    NamespaceSetup,
    Killed,
    Timeout,
    Protocol,
}

// ===================================================================
// Shared HTTP descriptors (draco-net & Tier 2 replay)
// ===================================================================

/// A request to be issued (Tier 1 constructed URL, or a Tier 2 intercepted request).
///
/// Header order is preserved because it is fingerprint-relevant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HttpRequestSpec {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// Base64 request body (JSON-header-safe). Large bodies ride the IPC frame body instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_b64: Option<String>,
}

/// Response metadata (the raw body is carried out-of-band as `bytes::Bytes`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HttpResponseMeta {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// URL after following redirects.
    pub final_url: String,
    pub elapsed_ms: u64,
}

// ===================================================================
// Tier 0/1 extraction output
// ===================================================================

/// A successful Tier 0/1 extraction, tagged with the paradigm that produced it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtractedData {
    pub tier: SourceTier,
    pub origin: ExtractOrigin,
    pub data: Value,
}

/// Which embedded-state paradigm produced an extraction.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExtractOrigin {
    /// `<script id="__NEXT_DATA__">`
    NextData,
    /// `<script type="application/ld+json">`
    JsonLd,
    /// `window.__NUXT__ = { ... }` (object-literal form)
    NuxtWindow,
    /// Tier 1 `_next/data/<build_id>/…​.json`
    NextBuildApi,
}

// ===================================================================
// IPC: supervisor (draco-core) <-> jail child (`draco __jail`)
// One bidirectional AF_UNIX socketpair inherited as fd 3 in the child.
// Framing is defined in the canonical spec (§6); these enums are the JSON header.
// ===================================================================

/// Frame headers sent supervisor → jailed child.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum SupervisorToJail {
    /// A pre-fetched script subresource for the upcoming `Hydrate`. The frame
    /// **body** carries the raw source bytes; `url` is its absolute URL (the key
    /// the isolate's module loader resolves against). Sent zero or more times
    /// *before* `Hydrate`; the child accumulates them so `<script src>` and
    /// `import`/`import()` for `type="module"` apps resolve without the
    /// (air-gapped) child ever touching the network.
    Resource {
        url: String,
    },
    /// Evaluate a page. The frame **body** carries the raw HTML bytes.
    Hydrate {
        url: String,
        /// Hard cap on the interception window.
        capture_window_ms: u64,
        /// Close the window early if the event loop is idle this long.
        quiesce_ms: u64,
        /// Safety cap on captured requests.
        max_intercepts: u32,
        /// JSON `op_raze_fetch` resolves with, to keep hydration going (default `"{}"`).
        stub_response_json: String,
    },
    Shutdown,
}

/// Frame headers sent jailed child → supervisor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum JailToSupervisor {
    /// Child booted: isolate created, snapshot loaded, filters armed.
    Ready {
        snapshot_restore_ms: u64,
    },
    /// One captured request. Optional request body rides the frame body.
    Intercept {
        seq: u32,
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        has_body: bool,
        via: InterceptVia,
    },
    /// Terminal report for a `Hydrate`.
    Result {
        outcome: RuntimeOutcome,
        intercept_count: u32,
    },
    Log {
        level: LogLevel,
        msg: String,
    },
    Error {
        reason: JailKind,
        detail: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InterceptVia {
    Fetch,
    Xhr,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeOutcome {
    /// Event loop went idle — clean close.
    Quiesced,
    /// Hit `capture_window_ms`.
    WindowClosed,
    /// Isolate force-terminated.
    Terminated,
    /// Ran, but the SPA never fetched.
    NoIntercepts,
    /// Uncaught JS error before any capture.
    Threw,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

// ===================================================================
// Tests: serde round-trips + wire-shape (tag) assertions.
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roundtrip<T>(v: &T)
    where
        T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let s = serde_json::to_string(v).expect("serialize");
        let back: T = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(v, &back, "round-trip mismatch for {s}");
    }

    #[test]
    fn extraction_result_roundtrips() {
        let r = ExtractionResult {
            url: "https://example.com/p/123".into(),
            status: Status::Success,
            source_tier: Some(SourceTier::RuntimeInterception),
            data: Some(json!({ "price": 42, "title": "Widget" })),
            markdown: Some("# Widget\n\nA great widget.".into()),
            metadata: Some(json!({ "title": "Widget", "statusCode": 200 })),
            endpoints: None,
            timing: Timing {
                network_ms: 210,
                parse_ms: 4,
                runtime_ms: 380,
                total_ms: 601,
            },
            trace: vec![
                TraceStep {
                    tier: SourceTier::Static,
                    action: "static.next_data".into(),
                    outcome: StepOutcome::Missed,
                    elapsed_ms: 5,
                    detail: None,
                },
                TraceStep {
                    tier: SourceTier::RuntimeInterception,
                    action: "runtime.capture".into(),
                    outcome: StepOutcome::Matched,
                    elapsed_ms: 380,
                    detail: Some("/api/products (score 23)".into()),
                },
            ],
            error: None,
        };
        roundtrip(&r);
    }

    #[test]
    fn error_is_internally_tagged() {
        let e = DracoError::Network {
            reason: NetKind::Timeout,
            detail: "connect timeout".into(),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"kind\":\"network\""), "got {s}");
        assert!(s.contains("\"reason\":\"timeout\""), "got {s}");
        roundtrip(&e);
    }

    #[test]
    fn hydrate_frame_shape() {
        let h = SupervisorToJail::Hydrate {
            url: "https://example.com".into(),
            capture_window_ms: 2000,
            quiesce_ms: 300,
            max_intercepts: 64,
            stub_response_json: "{}".into(),
        };
        let s = serde_json::to_string(&h).unwrap();
        assert!(s.contains("\"t\":\"hydrate\""), "got {s}");
        roundtrip(&h);
        roundtrip(&SupervisorToJail::Shutdown);
    }

    #[test]
    fn intercept_and_result_frames_roundtrip() {
        roundtrip(&JailToSupervisor::Ready {
            snapshot_restore_ms: 7,
        });
        roundtrip(&JailToSupervisor::Intercept {
            seq: 0,
            method: "GET".into(),
            url: "/api/products".into(),
            headers: vec![("accept".into(), "application/json".into())],
            has_body: false,
            via: InterceptVia::Fetch,
        });
        roundtrip(&JailToSupervisor::Result {
            outcome: RuntimeOutcome::Quiesced,
            intercept_count: 2,
        });
    }

    #[test]
    fn extracted_data_roundtrips() {
        let d = ExtractedData {
            tier: SourceTier::Static,
            origin: ExtractOrigin::NextData,
            data: json!({ "props": { "pageProps": { "ok": true } } }),
        };
        roundtrip(&d);
    }

    #[test]
    fn optional_fields_are_omitted_when_none() {
        let r = ExtractionResult {
            url: "https://x".into(),
            status: Status::Unsupported,
            source_tier: None,
            data: None,
            markdown: None,
            metadata: None,
            endpoints: None,
            timing: Timing::default(),
            trace: vec![],
            error: None,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(
            !s.contains("source_tier"),
            "None fields should be omitted: {s}"
        );
        assert!(!s.contains("\"data\""), "None data should be omitted: {s}");
        assert!(!s.contains("error"), "None error should be omitted: {s}");
        assert!(
            !s.contains("markdown"),
            "None markdown should be omitted: {s}"
        );
        assert!(
            !s.contains("metadata"),
            "None metadata should be omitted: {s}"
        );
    }
}
