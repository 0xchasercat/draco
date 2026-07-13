use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::discovery::DetectedBrowser;

pub type DriverFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    HeadlessNew,
    HeadedMinimized,
    Headed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitStrategy {
    NetworkIdle,
    ChallengeSettle(Duration),
    Delay(Duration),
}

impl Default for WaitStrategy {
    fn default() -> Self {
        Self::ChallengeSettle(Duration::from_millis(1_500))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveOutput {
    pub final_url: String,
    pub html: String,
    pub cookies: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserError {
    Launch(String),
    Drive(String),
    Timeout,
    Unsupported(String),
}

impl std::fmt::Display for BrowserError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Launch(detail) => write!(formatter, "browser launch failed: {detail}"),
            Self::Drive(detail) => write!(formatter, "browser drive failed: {detail}"),
            Self::Timeout => write!(formatter, "browser drive timed out"),
            Self::Unsupported(detail) => write!(formatter, "browser driver unsupported: {detail}"),
        }
    }
}

impl std::error::Error for BrowserError {}

/// Browser-driving seam. The local ladder owns detection and escalation policy;
/// implementations own process/CDP details and can be replaced without changing
/// the open-core orchestration.
pub trait BrowserDriver: Send + Sync {
    fn launch(
        &self,
        browser: &DetectedBrowser,
        mode: LaunchMode,
    ) -> Result<Box<dyn BrowserSession>, BrowserError>;
}

pub trait BrowserSession: Send {
    fn drive<'a>(
        &'a mut self,
        url: &'a str,
        wait_strategy: &'a WaitStrategy,
    ) -> DriverFuture<'a, Result<DriveOutput, BrowserError>>;
}

/// Portable minimum driver: Chrome's own `--headless=new --dump-dom` path.
///
/// TODO(owner seam): replace or extend this with the selected CDP implementation
/// for headed navigation, cookie extraction, persistent profiles, and exact
/// Chrome flag/profile tuning. The trait intentionally does not choose a Rust,
/// Node, or Python CDP stack.
#[derive(Debug, Clone)]
pub struct CommandBrowserDriver {
    timeout: Duration,
}

impl CommandBrowserDriver {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl Default for CommandBrowserDriver {
    fn default() -> Self {
        Self::new(Duration::from_secs(30))
    }
}

impl BrowserDriver for CommandBrowserDriver {
    fn launch(
        &self,
        browser: &DetectedBrowser,
        mode: LaunchMode,
    ) -> Result<Box<dyn BrowserSession>, BrowserError> {
        Ok(Box::new(CommandBrowserSession {
            browser: browser.clone(),
            mode,
            timeout: self.timeout,
        }))
    }
}

struct CommandBrowserSession {
    browser: DetectedBrowser,
    mode: LaunchMode,
    timeout: Duration,
}

impl BrowserSession for CommandBrowserSession {
    fn drive<'a>(
        &'a mut self,
        url: &'a str,
        wait_strategy: &'a WaitStrategy,
    ) -> DriverFuture<'a, Result<DriveOutput, BrowserError>> {
        Box::pin(async move {
            if self.mode != LaunchMode::HeadlessNew {
                return Err(BrowserError::Unsupported(
                    "headed driving awaits the owner-selected CDP implementation".into(),
                ));
            }

            let mut command = Command::new(&self.browser.path);
            command
                .arg("--headless=new")
                .arg("--dump-dom")
                .arg("--no-first-run")
                .arg("--no-default-browser-check")
                .arg("--disable-background-networking")
                .arg("--disable-component-update")
                .arg("--disable-sync")
                .arg("--hide-scrollbars")
                .arg(virtual_time_budget(wait_strategy))
                .arg(url)
                .stdin(Stdio::null())
                .stderr(Stdio::piped())
                .stdout(Stdio::piped());

            let output = tokio::time::timeout(self.timeout, command.output())
                .await
                .map_err(|_| BrowserError::Timeout)?
                .map_err(|error| BrowserError::Launch(error.to_string()))?;
            if !output.status.success() {
                let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
                return Err(BrowserError::Drive(if detail.is_empty() {
                    format!("browser exited with {}", output.status)
                } else {
                    detail
                }));
            }
            let html = String::from_utf8(output.stdout)
                .map_err(|error| BrowserError::Drive(format!("DOM was not UTF-8: {error}")))?;
            if html.trim().is_empty() {
                return Err(BrowserError::Drive("browser returned an empty DOM".into()));
            }

            Ok(DriveOutput {
                final_url: url.to_owned(),
                html,
                // `--dump-dom` has no cookie API. The CDP implementation at the
                // owner seam will return the real cookie jar.
                cookies: HashMap::new(),
            })
        })
    }
}

fn virtual_time_budget(wait_strategy: &WaitStrategy) -> String {
    let duration = match wait_strategy {
        WaitStrategy::NetworkIdle => Duration::from_millis(500),
        WaitStrategy::ChallengeSettle(duration) | WaitStrategy::Delay(duration) => *duration,
    };
    format!("--virtual-time-budget={}", duration.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_modes_are_distinct_and_orderable_by_policy() {
        assert_ne!(LaunchMode::HeadlessNew, LaunchMode::HeadedMinimized);
        assert_ne!(LaunchMode::HeadedMinimized, LaunchMode::Headed);
    }

    #[test]
    fn wait_strategy_maps_to_virtual_time() {
        assert_eq!(
            virtual_time_budget(&WaitStrategy::Delay(Duration::from_millis(750))),
            "--virtual-time-budget=750"
        );
    }
}
