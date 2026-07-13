use std::time::Instant;

use draco_types::{ExtractionResult, SourceTier, Status, StepOutcome, Timing, TraceStep};

/// Convert a browser-solved DOM into Draco's frozen extraction contract.
///
/// The browser layer supplies rendered HTML; the Double Tap remains a pure,
/// synchronous reuse of Draco's static content engine.
pub fn extract_rendered_html(url: &str, html: &str) -> ExtractionResult {
    let started = Instant::now();
    let scraped = draco_static::content::scrape(html, url, 200, "text/html; charset=utf-8", true);
    let parse_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;

    ExtractionResult {
        url: url.to_owned(),
        status: Status::Success,
        // The frozen enum has no browser tier. Avoid mislabeling the result as
        // Tier 2 runtime interception; the static Double Tap is recorded below.
        source_tier: None,
        data: None,
        extract: None,
        markdown: Some(scraped.markdown),
        metadata: Some(scraped.metadata),
        html: Some(html.to_owned()),
        // Browser DOM is rendered output, not the unmodified network response.
        raw_html: None,
        links: None,
        endpoints: None,
        timing: Timing {
            network_ms: 0,
            parse_ms,
            runtime_ms: 0,
            total_ms: parse_ms,
        },
        trace: vec![TraceStep {
            tier: SourceTier::Static,
            action: "browser.double_tap".into(),
            outcome: StepOutcome::Matched,
            elapsed_ms: parse_ms,
            detail: None,
        }],
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_dom_uses_frozen_result_contract() {
        let result = extract_rendered_html(
            "https://example.com/article",
            "<html><head><title>T</title></head><body><main><h1>Hello</h1><p>World</p></main></body></html>",
        );
        assert_eq!(result.status, Status::Success);
        assert!(result.markdown.as_deref().unwrap().contains("Hello"));
        assert!(result.html.as_deref().unwrap().contains("<main>"));
        assert!(result.source_tier.is_none());
        assert!(result.raw_html.is_none());
    }
}
