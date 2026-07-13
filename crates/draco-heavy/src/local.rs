use std::sync::OnceLock;
use std::time::Duration;

use draco_types::{ExtractionResult, SourceTier, StepOutcome, TraceStep};

use crate::browser::{BrowserDriver, BrowserError, LaunchMode, WaitStrategy};
use crate::discovery::{default_cache_path, resolve, HostConfig, ResolvedHostConfig};
use crate::double_tap::extract_rendered_html;

const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
static LOCAL_HOST: OnceLock<ResolvedHostConfig> = OnceLock::new();

#[derive(Debug, Clone)]
pub struct LocalMintConfig {
    pub launch_modes: Vec<LaunchMode>,
    pub wait_strategy: WaitStrategy,
}

impl Default for LocalMintConfig {
    fn default() -> Self {
        Self {
            launch_modes: vec![
                LaunchMode::HeadlessNew,
                LaunchMode::HeadedMinimized,
                LaunchMode::Headed,
            ],
            wait_strategy: WaitStrategy::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptFailure {
    pub mode: LaunchMode,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalMintError {
    NoBrowser,
    NoSupportedLaunchMode,
    WallNotCleared(Vec<AttemptFailure>),
}

impl std::fmt::Display for LocalMintError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoBrowser => write!(formatter, "no supported local browser was found"),
            Self::NoSupportedLaunchMode => {
                write!(
                    formatter,
                    "the host supports none of the configured launch modes"
                )
            }
            Self::WallNotCleared(attempts) => {
                write!(formatter, "local browser did not clear the wall")?;
                for attempt in attempts {
                    write!(formatter, "; {:?}: {}", attempt.mode, attempt.detail)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for LocalMintError {}

/// Resolve the cached host/browser configuration once and use the portable
/// default driver. CLI and serve callers share this in-process fast path.
pub async fn mint_local(url: &str) -> Result<ExtractionResult, LocalMintError> {
    let host = LOCAL_HOST.get_or_init(|| resolve(&default_cache_path(), DEFAULT_CACHE_TTL, false));
    let driver = crate::browser::CommandBrowserDriver::default();
    mint_local_with(&driver, &host.config, &LocalMintConfig::default(), url).await
}

/// Run the local launch ladder with an injected driver. This is the policy seam
/// used by tests and by future owner-selected CDP implementations.
pub async fn mint_local_with<D: BrowserDriver + ?Sized>(
    driver: &D,
    host: &HostConfig,
    config: &LocalMintConfig,
    url: &str,
) -> Result<ExtractionResult, LocalMintError> {
    let browser = host.browser.as_ref().ok_or(LocalMintError::NoBrowser)?;
    let modes = supported_modes(host, &config.launch_modes);
    if modes.is_empty() {
        return Err(LocalMintError::NoSupportedLaunchMode);
    }

    let mut attempts = Vec::new();
    for mode in modes {
        let mut session = match driver.launch(browser, mode) {
            Ok(session) => session,
            Err(error) => {
                attempts.push(failed_attempt(mode, error));
                continue;
            }
        };
        let output = match session.drive(url, &config.wait_strategy).await {
            Ok(output) => output,
            Err(error) => {
                attempts.push(failed_attempt(mode, error));
                continue;
            }
        };

        let mut result = extract_rendered_html(&output.final_url, &output.html);
        let cleared = !looks_like_wall(&output.html)
            && result
                .markdown
                .as_deref()
                .is_some_and(|markdown| !markdown.trim().is_empty());
        result.trace.push(TraceStep {
            tier: SourceTier::Static,
            action: "browser.local_mint".into(),
            outcome: if cleared {
                StepOutcome::Matched
            } else {
                StepOutcome::Missed
            },
            elapsed_ms: 0,
            detail: Some(format!("mode={mode:?}; cookies={}", output.cookies.len())),
        });
        if cleared {
            return Ok(result);
        }
        attempts.push(AttemptFailure {
            mode,
            detail: "rendered DOM still looks challenged or empty".into(),
        });
    }

    Err(LocalMintError::WallNotCleared(attempts))
}

fn supported_modes(host: &HostConfig, configured: &[LaunchMode]) -> Vec<LaunchMode> {
    configured
        .iter()
        .copied()
        .filter(|mode| match mode {
            LaunchMode::HeadlessNew => host.launch_capabilities.headless_new,
            LaunchMode::HeadedMinimized | LaunchMode::Headed => host.launch_capabilities.headed,
        })
        .collect()
}

fn failed_attempt(mode: LaunchMode, error: BrowserError) -> AttemptFailure {
    AttemptFailure {
        mode,
        detail: error.to_string(),
    }
}

fn looks_like_wall(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    [
        "cf-chl-",
        "cf-turnstile",
        "just a moment",
        "datadome",
        "captcha-delivery",
        "verify you are human",
        "checking your browser",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::browser::{BrowserSession, DriveOutput, DriverFuture};
    use crate::discovery::{
        BrowserChannel, DetectedBrowser, LaunchCapabilities, RenderMode, RenderTier,
    };

    struct FakeDriver {
        seen: Arc<Mutex<Vec<LaunchMode>>>,
    }

    impl BrowserDriver for FakeDriver {
        fn launch(
            &self,
            _browser: &DetectedBrowser,
            mode: LaunchMode,
        ) -> Result<Box<dyn BrowserSession>, BrowserError> {
            self.seen.lock().unwrap().push(mode);
            Ok(Box::new(FakeSession { mode }))
        }
    }

    struct FakeSession {
        mode: LaunchMode,
    }

    impl BrowserSession for FakeSession {
        fn drive<'a>(
            &'a mut self,
            url: &'a str,
            _wait_strategy: &'a WaitStrategy,
        ) -> DriverFuture<'a, Result<DriveOutput, BrowserError>> {
            Box::pin(async move {
                let html = if self.mode == LaunchMode::Headed {
                    "<html><head><title>Solved</title></head><body><main><h1>Real content</h1><p>Done.</p></main></body></html>"
                } else {
                    "<html><title>Just a moment</title><body><div id=cf-chl-test>Checking your browser</div></body></html>"
                };
                Ok(DriveOutput {
                    final_url: url.into(),
                    html: html.into(),
                    cookies: HashMap::new(),
                })
            })
        }
    }

    fn host() -> HostConfig {
        HostConfig {
            schema_version: 2,
            discovered_at_unix: 0,
            fingerprint: "test".into(),
            render_mode: RenderMode::Gpu,
            render_tier: RenderTier::NativeDisplayGpu,
            display: Some(":0".into()),
            command_prefix: Vec::new(),
            chrome_args: Vec::new(),
            gpu_present: true,
            xvfb_path: None,
            vglrun_path: None,
            browser: Some(DetectedBrowser {
                channel: BrowserChannel::Chrome,
                path: "chrome".into(),
                version: Some("Google Chrome 126".into()),
                preferred: false,
            }),
            launch_capabilities: LaunchCapabilities {
                headless_new: true,
                headed: true,
                display: Some(":0".into()),
            },
        }
    }

    #[tokio::test]
    async fn ladder_uses_locked_order_and_stops_when_solved() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let driver = FakeDriver {
            seen: Arc::clone(&seen),
        };
        let result = mint_local_with(&driver, &host(), &LocalMintConfig::default(), "https://x")
            .await
            .unwrap();
        assert_eq!(result.status, draco_types::Status::Success);
        assert!(result.markdown.unwrap().contains("Real content"));
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                LaunchMode::HeadlessNew,
                LaunchMode::HeadedMinimized,
                LaunchMode::Headed
            ]
        );
    }

    #[tokio::test]
    async fn capabilities_skip_unavailable_headed_modes() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let driver = FakeDriver {
            seen: Arc::clone(&seen),
        };
        let mut host = host();
        host.launch_capabilities.headed = false;
        let error = mint_local_with(&driver, &host, &LocalMintConfig::default(), "https://x")
            .await
            .unwrap_err();
        assert!(matches!(error, LocalMintError::WallNotCleared(_)));
        assert_eq!(*seen.lock().unwrap(), vec![LaunchMode::HeadlessNew]);
    }
}
