use draco_core::Config;
use draco_types::ExtractionResult;

#[cfg(feature = "heavy-local")]
pub(crate) async fn maybe_escalate(
    url: &str,
    config: &Config,
    mut result: ExtractionResult,
) -> ExtractionResult {
    if result.status != draco_types::Status::NeedsBrowser {
        return result;
    }
    // The open-core local path intentionally uses the user's own IP. A caller
    // that explicitly selected a proxy keeps the original terminal result;
    // proxy-bound browser routing belongs to the separate cloud gateway.
    if config.proxy.is_some() {
        return result;
    }

    match draco_heavy::mint_local(url).await {
        Ok(heavy) => heavy,
        Err(error) => {
            result.trace.push(draco_types::TraceStep {
                tier: draco_types::SourceTier::Static,
                action: "browser.local_fallback".into(),
                outcome: draco_types::StepOutcome::Failed,
                elapsed_ms: 0,
                detail: Some(error.to_string()),
            });
            result
        }
    }
}

#[cfg(not(feature = "heavy-local"))]
pub(crate) async fn maybe_escalate(
    _url: &str,
    _config: &Config,
    result: ExtractionResult,
) -> ExtractionResult {
    result
}

#[cfg(test)]
mod tests {
    use draco_types::{Status, Timing};

    use super::*;

    fn result(status: Status) -> ExtractionResult {
        ExtractionResult {
            url: "https://example.com".into(),
            status,
            source_tier: None,
            data: None,
            extract: None,
            markdown: None,
            metadata: None,
            html: None,
            raw_html: None,
            links: None,
            endpoints: None,
            timing: Timing::default(),
            trace: Vec::new(),
            error: None,
        }
    }

    #[tokio::test]
    async fn non_browser_result_is_unchanged() {
        let original = result(Status::Unsupported);
        assert_eq!(
            maybe_escalate("https://example.com", &Config::default(), original.clone()).await,
            original
        );
    }

    #[cfg(not(feature = "heavy-local"))]
    #[tokio::test]
    async fn feature_off_preserves_needs_browser_exactly() {
        let original = result(Status::NeedsBrowser);
        assert_eq!(
            maybe_escalate("https://example.com", &Config::default(), original.clone()).await,
            original
        );
    }
}
