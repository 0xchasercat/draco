//! Offline test doubles for the [`PageFetcher`](crate::fetcher::PageFetcher)
//! and [`StaticEngine`](crate::machine::StaticEngine) seams.
//!
//! Compiled only under `#[cfg(test)]`. These mocks return canned fixtures so
//! the escalation ladder can be driven without a network or a live
//! `draco-static` / `draco-net` (both are `todo!()` stubs in WS-C). Keeping
//! them in their own module lets every `#[cfg(test)] mod tests` share one set.

#![cfg(test)]

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use draco_net::{HtmlResponse, SessionOpts};
use draco_static::StaticOutcome;
use draco_types::{DracoError, ExtractedData, HttpRequestSpec, HttpResponseMeta};

use crate::fetcher::PageFetcher;
use crate::machine::StaticEngine;
use crate::ranking::Candidate;
use crate::tier2::{CaptureResult, Tier2Capture};
use draco_types::RuntimeOutcome;

/// A scripted [`PageFetcher`]: `fetch` returns one canned response (or error),
/// `replay` returns another. Records call counts so tests can assert a tier was
/// (or was not) exercised.
pub struct MockFetcher {
    fetch_result: Result<HtmlResponse, DracoError>,
    replay_result: Result<HtmlResponse, DracoError>,
    fetch_calls: AtomicUsize,
    replay_calls: AtomicUsize,
}

impl MockFetcher {
    /// A fetcher whose `fetch` yields `status` + `body`, and whose `replay`
    /// (until overridden) 404s — most ladder tests never reach replay, and the
    /// ones that do set it explicitly.
    pub fn ok_html(status: u16, body: &str) -> Self {
        Self {
            fetch_result: Ok(html_response(status, body.as_bytes(), &[])),
            replay_result: Ok(html_response(404, b"", &[])),
            fetch_calls: AtomicUsize::new(0),
            replay_calls: AtomicUsize::new(0),
        }
    }

    /// Add a response header to the canned `fetch` result.
    pub fn with_header(mut self, k: &str, v: &str) -> Self {
        if let Ok(resp) = &mut self.fetch_result {
            resp.meta.headers.push((k.to_string(), v.to_string()));
        }
        self
    }

    /// Make `replay` return `status` + a JSON body.
    pub fn with_replay_json(mut self, status: u16, value: serde_json::Value) -> Self {
        let body = serde_json::to_vec(&value).unwrap();
        self.replay_result = Ok(html_response(
            status,
            &body,
            &[("content-type", "application/json")],
        ));
        self
    }

    /// Make `replay` return an arbitrary status with an empty body.
    pub fn with_replay_status(mut self, status: u16) -> Self {
        self.replay_result = Ok(html_response(status, b"", &[]));
        self
    }

    pub fn fetch_calls(&self) -> usize {
        self.fetch_calls.load(Ordering::SeqCst)
    }
    pub fn replay_calls(&self) -> usize {
        self.replay_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl PageFetcher for MockFetcher {
    async fn fetch(&self, _url: &str, _opts: &SessionOpts) -> Result<HtmlResponse, DracoError> {
        self.fetch_calls.fetch_add(1, Ordering::SeqCst);
        self.fetch_result.clone()
    }

    async fn replay(
        &self,
        _spec: &HttpRequestSpec,
        _opts: &SessionOpts,
    ) -> Result<HtmlResponse, DracoError> {
        self.replay_calls.fetch_add(1, Ordering::SeqCst);
        self.replay_result.clone()
    }
}

/// A fetcher whose `fetch` always errors — for the fetch-failure path.
pub fn err_fetcher(e: DracoError) -> MockFetcher {
    MockFetcher {
        fetch_result: Err(e),
        replay_result: Ok(html_response(404, b"", &[])),
        fetch_calls: AtomicUsize::new(0),
        replay_calls: AtomicUsize::new(0),
    }
}

/// A fetcher whose `fetch` succeeds (empty 200) but whose `replay` always errors
/// — for exercising the Tier 2 replay-failure path.
pub fn err_replay_fetcher(e: DracoError) -> MockFetcher {
    MockFetcher {
        fetch_result: Ok(html_response(200, b"", &[])),
        replay_result: Err(e),
        fetch_calls: AtomicUsize::new(0),
        replay_calls: AtomicUsize::new(0),
    }
}

fn html_response(status: u16, body: &[u8], headers: &[(&str, &str)]) -> HtmlResponse {
    HtmlResponse {
        meta: HttpResponseMeta {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            final_url: "https://x.com/".to_string(),
            elapsed_ms: 1,
        },
        body: Bytes::copy_from_slice(body),
    }
}

/// A scripted [`StaticEngine`]. Each field controls one frozen operation so a
/// test can shape the exact Tier 0/1 path it wants without touching the real
/// (stubbed) extractors.
#[derive(Default)]
pub struct MockStatic {
    static_outcome: Option<ExtractedData>,
    build_id: Option<String>,
    app_router: bool,
    /// Canned Markdown for the `scrape` seam; `None` → a small default body.
    markdown: Option<String>,
    /// Canned `incomplete` (skeleton) flag the `scrape` seam reports.
    incomplete: bool,
}

impl MockStatic {
    /// Tier 0 hits with the given extraction.
    pub fn hit(data: ExtractedData) -> Self {
        Self {
            static_outcome: Some(data),
            ..Self::default()
        }
    }

    /// Tier 0 hits via `__NEXT_DATA__` with a trivial payload.
    pub fn hit_next_data() -> Self {
        Self::hit(ExtractedData {
            tier: draco_types::SourceTier::Static,
            origin: draco_types::ExtractOrigin::NextData,
            data: serde_json::json!({ "ok": true }),
        })
    }

    /// Tier 0 misses; Tier 1 discovers `build_id`.
    pub fn miss_then_build_id(build_id: &str) -> Self {
        Self {
            build_id: Some(build_id.to_string()),
            ..Self::default()
        }
    }

    /// Tier 0 misses; Tier 1 finds no build id.
    pub fn miss_no_build_id() -> Self {
        Self::default()
    }

    /// Tier 0 misses; page is app-router (Tier 1 ineligible).
    pub fn miss_app_router() -> Self {
        Self {
            app_router: true,
            ..Self::default()
        }
    }

    /// Override the Markdown the `scrape` seam returns (for exercising the
    /// Markdown / Both paths and the thin-SPA note).
    pub fn with_markdown(mut self, md: &str) -> Self {
        self.markdown = Some(md.to_string());
        self
    }

    /// Make the `scrape` seam report an incomplete (skeleton) render, so tests can
    /// exercise the render escalation triggered by a non-thin skeleton page.
    pub fn with_incomplete(mut self, incomplete: bool) -> Self {
        self.incomplete = incomplete;
        self
    }
}

impl StaticEngine for MockStatic {
    fn scrape(
        &self,
        _html: &str,
        url: &str,
        status: u16,
        content_type: &str,
        _only_main_content: bool,
    ) -> draco_static::content::ScrapeResult {
        // A deterministic stand-in for the real content engine: a tiny Markdown
        // body plus the always-present synthetic metadata keys. Tests that care
        // about real readability/markdown drive `draco-static` directly.
        draco_static::content::ScrapeResult {
            markdown: self
                .markdown
                .clone()
                .unwrap_or_else(|| "# Mock\n\nbody".to_string()),
            metadata: serde_json::json!({
                "sourceURL": url,
                "url": url,
                "statusCode": status,
                "contentType": content_type,
            }),
            incomplete: self.incomplete,
        }
    }
    fn extract_static(&self, _html: &str) -> StaticOutcome {
        match &self.static_outcome {
            Some(d) => StaticOutcome::Hit(d.clone()),
            None => StaticOutcome::Miss,
        }
    }
    fn discover_build_id(&self, _html: &str) -> Option<String> {
        self.build_id.clone()
    }
    fn next_data_url(&self, build_id: &str, pathname: &str, query: &[(String, String)]) -> String {
        // Deterministic stand-in for the real constructor; shape mirrors spec §10.
        let mut u = format!("/_next/data/{build_id}{pathname}.json");
        if !query.is_empty() {
            let qs: Vec<String> = query.iter().map(|(k, v)| format!("{k}={v}")).collect();
            u.push('?');
            u.push_str(&qs.join("&"));
        }
        u
    }
    fn is_app_router(&self, _html: &str) -> bool {
        self.app_router
    }
}

// ---------------------------------------------------------------------------
// Tier 2 capture doubles
// ---------------------------------------------------------------------------

/// A [`Tier2Capture`] double that returns a canned set of intercepts (never
/// spawns a real child). Lets ladder tests exercise the full Tier 2 rank/replay
/// path offline. Records a call count so a test can assert capture was reached.
pub struct MockCapture {
    result: Result<CaptureResult, DracoError>,
    calls: AtomicUsize,
}

impl MockCapture {
    /// A representative sandbox level the mock child "reports", so ladder tests
    /// can assert the `runtime.sandbox` trace step is recorded.
    pub const MOCK_LEVEL: &'static str = "hardened: seccomp+netns+landlock";

    /// Capture yields the given candidates (no request bodies) with a
    /// `Quiesced` outcome.
    pub fn with_candidates(candidates: Vec<Candidate>) -> Self {
        let bodies = vec![None; candidates.len()];
        Self {
            result: Ok(CaptureResult {
                candidates,
                bodies,
                outcome: RuntimeOutcome::Quiesced,
                sandbox_level: Some(Self::MOCK_LEVEL.to_string()),
                rendered_html: None,
                logs: Vec::new(),
            }),
            calls: AtomicUsize::new(0),
        }
    }

    /// Capture yields no intercepts (`NoIntercepts`) — the SPA never fetched.
    pub fn empty() -> Self {
        Self {
            result: Ok(CaptureResult {
                candidates: Vec::new(),
                bodies: Vec::new(),
                outcome: RuntimeOutcome::NoIntercepts,
                sandbox_level: Some(Self::MOCK_LEVEL.to_string()),
                rendered_html: None,
                logs: Vec::new(),
            }),
            calls: AtomicUsize::new(0),
        }
    }

    /// Capture yields no intercepts but *does* report page-side diagnostics
    /// (`CaptureResult::logs`) — the "page hydrated to nothing, and here is
    /// why" case. Drives the `runtime.log` trace-step tests without forking a
    /// child.
    pub fn with_logs(logs: Vec<String>) -> Self {
        Self {
            result: Ok(CaptureResult {
                candidates: Vec::new(),
                bodies: Vec::new(),
                outcome: RuntimeOutcome::NoIntercepts,
                sandbox_level: Some(Self::MOCK_LEVEL.to_string()),
                rendered_html: None,
                logs,
            }),
            calls: AtomicUsize::new(0),
        }
    }

    /// Capture yields no interceptable requests but *does* return a serialized
    /// hydrated DOM — the render-then-Markdown case (a thin shell that hydrates
    /// its content without any data fetch, or whose fetch was inlined). Drives the
    /// `runtime.render` escalation in ladder tests without forking a child.
    pub fn rendered(dom: impl Into<String>) -> Self {
        Self {
            result: Ok(CaptureResult {
                candidates: Vec::new(),
                bodies: Vec::new(),
                outcome: RuntimeOutcome::Quiesced,
                sandbox_level: Some(Self::MOCK_LEVEL.to_string()),
                rendered_html: Some(dom.into()),
                logs: Vec::new(),
            }),
            calls: AtomicUsize::new(0),
        }
    }

    /// Capture fails with the given jail error (spawn/protocol/killed).
    pub fn failing(e: DracoError) -> Self {
        Self {
            result: Err(e),
            calls: AtomicUsize::new(0),
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Tier2Capture for MockCapture {
    async fn capture(
        &self,
        _url: &str,
        _html: &[u8],
        _resources: &[crate::tier2::ScriptResource],
        _config: &crate::Config,
        _opts: &draco_net::SessionOpts,
    ) -> Result<CaptureResult, DracoError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.result.clone()
    }
}

/// The default capture double for ladder tests that don't care about Tier 2:
/// reaching Tier 2 with this seam yields no intercepts, so the ladder falls
/// through to `Unsupported` exactly as the pre-Slice-4 skip did — without forking
/// a child.
pub fn noop_capture() -> MockCapture {
    MockCapture::empty()
}
