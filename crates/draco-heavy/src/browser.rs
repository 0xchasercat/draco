use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use chaser_oxide::cdp::browser_protocol::browser::{
    Bounds, GetWindowForTargetParams, SetWindowBoundsParams, WindowState,
};
use chaser_oxide::handler::viewport::Viewport;
use chaser_oxide::{Browser, BrowserConfig, ChaserPage};
use futures::StreamExt;

use crate::discovery::DetectedBrowser;

pub type DriverFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

const VIEWPORT_WIDTH: u32 = 1365;
const VIEWPORT_HEIGHT: u32 = 768;
const MINIMAL_TELL_REMOVAL: &str = r#"
(() => {
  const proto = Object.getPrototypeOf(navigator);
  const descriptor = Object.getOwnPropertyDescriptor(proto, 'webdriver');
  if (!descriptor || descriptor.configurable) {
    Object.defineProperty(proto, 'webdriver', { get: () => false, configurable: true });
  }
  for (const key of Object.getOwnPropertyNames(globalThis)) {
    if (/^(cdc_|\$cdc_|__webdriver|__selenium|__driver|\$chrome_)/.test(key)) {
      try { delete globalThis[key]; } catch (_) {}
    }
  }
})();
"#;

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
        Self::ChallengeSettle(Duration::from_secs(5))
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

/// Pure-Rust CDP driver backed by chaser-oxide.
///
/// It relies on chaser-oxide's protocol-level execution behavior and installs
/// only tell-removal. It deliberately never applies ChaserProfile or native
/// profile scripts because those fabricate OS, plugin, and WebGL identity.
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
        Self::new(Duration::from_secs(45))
    }
}

impl BrowserDriver for CommandBrowserDriver {
    fn launch(
        &self,
        browser: &DetectedBrowser,
        mode: LaunchMode,
    ) -> Result<Box<dyn BrowserSession>, BrowserError> {
        let viewport = Viewport {
            width: VIEWPORT_WIDTH,
            height: VIEWPORT_HEIGHT,
            device_scale_factor: None,
            emulating_mobile: false,
            is_landscape: true,
            has_touch: false,
        };
        let builder = BrowserConfig::builder()
            .chrome_executable(&browser.path)
            .window_size(VIEWPORT_WIDTH, VIEWPORT_HEIGHT)
            .viewport(viewport)
            .launch_timeout(self.timeout)
            .request_timeout(self.timeout);
        let builder = match mode {
            LaunchMode::HeadlessNew => builder.new_headless_mode(),
            LaunchMode::HeadedMinimized | LaunchMode::Headed => builder.with_head(),
        };
        let config = builder.build().map_err(BrowserError::Launch)?;
        Ok(Box::new(ChaserSession {
            config: Some(config),
            mode,
            timeout: self.timeout,
        }))
    }
}

struct ChaserSession {
    config: Option<BrowserConfig>,
    mode: LaunchMode,
    timeout: Duration,
}

impl BrowserSession for ChaserSession {
    fn drive<'a>(
        &'a mut self,
        url: &'a str,
        wait_strategy: &'a WaitStrategy,
    ) -> DriverFuture<'a, Result<DriveOutput, BrowserError>> {
        Box::pin(async move {
            let config = self
                .config
                .take()
                .ok_or_else(|| BrowserError::Drive("browser session already consumed".into()))?;
            tokio::time::timeout(
                self.timeout,
                drive_once(config, self.mode, url, wait_strategy),
            )
            .await
            .map_err(|_| BrowserError::Timeout)?
        })
    }
}

async fn drive_once(
    config: BrowserConfig,
    mode: LaunchMode,
    url: &str,
    wait_strategy: &WaitStrategy,
) -> Result<DriveOutput, BrowserError> {
    let (mut browser, mut handler) = Browser::launch(config)
        .await
        .map_err(|error| BrowserError::Launch(error.to_string()))?;
    let handler_task = tokio::spawn(async move {
        while let Some(event) = handler.next().await {
            if event.is_err() {
                break;
            }
        }
    });

    let result = drive_page(&browser, mode, url, wait_strategy).await;
    let _ = browser.close().await;
    let _ = browser.wait().await;
    handler_task.abort();
    result
}

async fn drive_page(
    browser: &Browser,
    mode: LaunchMode,
    url: &str,
    wait_strategy: &WaitStrategy,
) -> Result<DriveOutput, BrowserError> {
    let page = browser
        .new_page("about:blank")
        .await
        .map_err(|error| BrowserError::Drive(error.to_string()))?;
    install_minimal_tell_removal(&page).await?;
    if mode == LaunchMode::HeadedMinimized {
        minimize_window(browser, &page).await?;
    }

    let chaser = ChaserPage::new(page);
    chaser
        .goto(url)
        .await
        .map_err(|error| BrowserError::Drive(error.to_string()))?;
    let html = settled_content(&chaser, wait_strategy).await?;
    let final_url = chaser
        .url()
        .await
        .map_err(|error| BrowserError::Drive(error.to_string()))?
        .unwrap_or_else(|| url.to_owned());
    let cookies = browser
        .get_cookies()
        .await
        .map_err(|error| BrowserError::Drive(error.to_string()))?
        .into_iter()
        .map(|cookie| (cookie.name, cookie.value))
        .collect();

    Ok(DriveOutput {
        final_url,
        html,
        cookies,
    })
}

async fn install_minimal_tell_removal(page: &chaser_oxide::Page) -> Result<(), BrowserError> {
    page.evaluate_on_new_document(MINIMAL_TELL_REMOVAL)
        .await
        .map_err(|error| BrowserError::Drive(error.to_string()))?;
    let user_agent = page
        .user_agent()
        .await
        .map_err(|error| BrowserError::Drive(error.to_string()))?;
    if user_agent.contains("HeadlessChrome") {
        page.set_user_agent(user_agent.replace("HeadlessChrome", "Chrome").as_str())
            .await
            .map_err(|error| BrowserError::Drive(error.to_string()))?;
    }
    Ok(())
}

async fn minimize_window(browser: &Browser, page: &chaser_oxide::Page) -> Result<(), BrowserError> {
    let window = browser
        .execute(
            GetWindowForTargetParams::builder()
                .target_id(page.target_id().clone())
                .build(),
        )
        .await
        .map_err(|error| BrowserError::Drive(error.to_string()))?
        .result;
    browser
        .execute(SetWindowBoundsParams::new(
            window.window_id,
            Bounds::builder()
                .window_state(WindowState::Minimized)
                .build(),
        ))
        .await
        .map_err(|error| BrowserError::Drive(error.to_string()))?;
    Ok(())
}

async fn settled_content(
    chaser: &ChaserPage,
    wait_strategy: &WaitStrategy,
) -> Result<String, BrowserError> {
    match wait_strategy {
        WaitStrategy::Delay(duration) => {
            tokio::time::sleep(*duration).await;
            chaser
                .content()
                .await
                .map_err(|error| BrowserError::Drive(error.to_string()))
        }
        WaitStrategy::NetworkIdle => {
            tokio::time::sleep(Duration::from_millis(500)).await;
            chaser
                .content()
                .await
                .map_err(|error| BrowserError::Drive(error.to_string()))
        }
        WaitStrategy::ChallengeSettle(duration) => {
            let deadline = tokio::time::Instant::now() + *duration;
            loop {
                // Isolated-world evaluation: does not trigger Runtime.enable.
                let _ = chaser.evaluate("document.readyState").await;
                let html = chaser
                    .content()
                    .await
                    .map_err(|error| BrowserError::Drive(error.to_string()))?;
                if !html.trim().is_empty() && !crate::local::looks_like_wall(&html) {
                    return Ok(html);
                }
                if tokio::time::Instant::now() >= deadline {
                    return Ok(html);
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
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
    fn default_challenge_settle_budget_is_five_seconds() {
        assert_eq!(
            WaitStrategy::default(),
            WaitStrategy::ChallengeSettle(Duration::from_secs(5))
        );
    }

    #[test]
    fn minimal_tell_removal_does_not_spoof_fingerprints() {
        assert!(MINIMAL_TELL_REMOVAL.contains("webdriver"));
        assert!(MINIMAL_TELL_REMOVAL.contains("cdc_"));
        for forbidden in ["WebGL", "plugins", "deviceMemory", "hardwareConcurrency"] {
            assert!(!MINIMAL_TELL_REMOVAL.contains(forbidden));
        }
    }
}
