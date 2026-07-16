use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use chaser_oxide::auth::Credentials;
use chaser_oxide::cdp::browser_protocol::browser::{
    Bounds, GetWindowForTargetParams, SetWindowBoundsParams, WindowState,
};
use chaser_oxide::handler::viewport::Viewport;
use chaser_oxide::handler::HandlerConfig;
use chaser_oxide::{Browser, ChaserPage};
use futures::StreamExt;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::discovery::DetectedBrowser;
use crate::proxy::PreparedBrowserProxy;

pub type DriverFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type LaunchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Box<dyn BrowserSession>, BrowserError>> + Send + 'a>>;
#[cfg(test)]
type CleanupFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

const VIEWPORT_WIDTH: u32 = 1365;
#[cfg(any(target_os = "linux", test))]
fn linux_status_uid(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|uids| uids.split_whitespace().next())
        .and_then(|uid| uid.parse().ok())
}

fn should_disable_sandbox() -> bool {
    if std::env::var_os("DRACO_BROWSER_NO_SANDBOX").is_some() {
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| linux_status_uid(&status))
            .is_some_and(|uid| uid == 0)
    }
    #[cfg(not(target_os = "linux"))]
    false
}
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
    Cleanup {
        primary: Option<Box<BrowserError>>,
        detail: String,
    },
    Unsupported(String),
}

impl std::fmt::Display for BrowserError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Launch(detail) => write!(formatter, "browser launch failed: {detail}"),
            Self::Drive(detail) => write!(formatter, "browser drive failed: {detail}"),
            Self::Timeout => write!(formatter, "browser drive timed out"),
            Self::Cleanup {
                primary: Some(primary),
                detail,
            } => write!(formatter, "{primary}; browser cleanup failed: {detail}"),
            Self::Cleanup {
                primary: None,
                detail,
            } => write!(formatter, "browser cleanup failed: {detail}"),
            Self::Unsupported(detail) => write!(formatter, "browser driver unsupported: {detail}"),
        }
    }
}

impl std::error::Error for BrowserError {}

pub trait BrowserDriver: Send + Sync {
    fn launch(&self, browser: &DetectedBrowser, mode: LaunchMode) -> LaunchFuture<'_>;
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
#[derive(Debug)]
pub struct CommandBrowserDriver {
    timeout: Duration,
    proxy: Option<PreparedBrowserProxy>,
}

impl CommandBrowserDriver {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            proxy: None,
        }
    }

    pub(crate) fn proxied(proxy: PreparedBrowserProxy) -> Self {
        Self {
            timeout: Duration::from_secs(45),
            proxy: Some(proxy),
        }
    }
}

impl Default for CommandBrowserDriver {
    fn default() -> Self {
        Self::new(Duration::from_secs(45))
    }
}

impl BrowserDriver for CommandBrowserDriver {
    fn launch(&self, browser: &DetectedBrowser, mode: LaunchMode) -> LaunchFuture<'_> {
        let executable = browser.path.clone();
        Box::pin(async move {
            let mut args: Vec<String> = Vec::new();
            if should_disable_sandbox() {
                args.push("--no-sandbox".into());
                args.push("--disable-setuid-sandbox".into());
            }
            args.push(match mode {
                LaunchMode::HeadlessNew => "--headless=new".into(),
                LaunchMode::HeadedMinimized | LaunchMode::Headed => "--headless".into(),
            });
            args.push("--remote-debugging-port=0".into());
            args.push("--no-first-run".into());
            if let Some(proxy) = &self.proxy {
                args.push(format!("--proxy-server={}", proxy.server()));
            }
            args.push("about:blank".into());

            let credentials = self.proxy.as_ref().and_then(|proxy| {
                proxy.credentials().map(|(username, password)| Credentials {
                    username: username.to_owned(),
                    password: password.to_owned(),
                })
            });

            let mut child = Command::new(&executable)
                .args(&args)
                .stderr(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .map_err(|error| BrowserError::Launch(error.to_string()))?;

            let stderr = child.stderr.take().expect("stderr piped");
            let mut lines = BufReader::new(stderr).lines();
            let ws_url = tokio::time::timeout(self.timeout, async {
                while let Some(line) = lines.next_line().await.transpose() {
                    let line = line.map_err(|e| BrowserError::Launch(e.to_string()))?;
                    if let Some(ws) = line.rsplit_once("listening on ").map(|(_, ws)| ws) {
                        if ws.starts_with("ws") && ws.contains("devtools/browser") {
                            return Ok(ws.trim().to_string());
                        }
                    }
                }
                Err(BrowserError::Launch(
                    "browser exited before WebSocket URL appeared".into(),
                ))
            })
            .await
            .map_err(|_| BrowserError::Launch("launch timeout".into()))??;

            let (browser, mut handler) = Browser::connect_with_config(
                &ws_url,
                HandlerConfig {
                    viewport: Some(Viewport {
                        width: VIEWPORT_WIDTH,
                        height: VIEWPORT_HEIGHT,
                        device_scale_factor: None,
                        emulating_mobile: false,
                        is_landscape: true,
                        has_touch: false,
                    }),
                    ..HandlerConfig::default()
                },
            )
            .await
            .map_err(|error| BrowserError::Launch(error.to_string()))?;

            let handler_task = tokio::spawn(async move {
                while let Some(event) = handler.next().await {
                    if event.is_err() {
                        break;
                    }
                }
            });

            Ok(Box::new(ChaserSession {
                browser,
                handler_task: Some(handler_task),
                mode,
                timeout: self.timeout,
                credentials,
            }) as Box<dyn BrowserSession>)
        })
    }
}

struct ChaserSession {
    browser: Browser,
    handler_task: Option<tokio::task::JoinHandle<()>>,
    mode: LaunchMode,
    timeout: Duration,
    credentials: Option<Credentials>,
}

impl BrowserSession for ChaserSession {
    fn drive<'a>(
        &'a mut self,
        url: &'a str,
        wait_strategy: &'a WaitStrategy,
    ) -> DriverFuture<'a, Result<DriveOutput, BrowserError>> {
        Box::pin(async move {
            let remaining = remaining_request_budget(self.timeout, Duration::ZERO);
            let completion = match tokio::time::timeout(
                remaining,
                drive_page(
                    &self.browser,
                    self.mode,
                    url,
                    wait_strategy,
                    self.credentials.as_ref(),
                ),
            )
            .await
            {
                Ok(result) => DriveCompletion::Completed(result),
                Err(_) => DriveCompletion::TimedOut,
            };
            if let Some(task) = self.handler_task.take() {
                task.abort();
                let _ = task.await;
            }
            match completion {
                DriveCompletion::Completed(result) => result,
                DriveCompletion::TimedOut => Err(BrowserError::Timeout),
            }
        })
    }
}

enum DriveCompletion {
    Completed(Result<DriveOutput, BrowserError>),
    TimedOut,
}

#[cfg(test)]
trait BrowserProcess {
    fn close_process(&mut self) -> CleanupFuture<'_>;
    fn kill_process(&mut self) -> CleanupFuture<'_>;
    fn wait_process(&mut self) -> CleanupFuture<'_>;
}

#[cfg(test)]
impl BrowserProcess for Browser {
    fn close_process(&mut self) -> CleanupFuture<'_> {
        Box::pin(async move {
            self.close()
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }

    fn kill_process(&mut self) -> CleanupFuture<'_> {
        Box::pin(async move {
            match self.kill().await {
                Some(result) => result.map_err(|error| error.to_string()),
                None => Ok(()),
            }
        })
    }

    fn wait_process(&mut self) -> CleanupFuture<'_> {
        Box::pin(async move {
            self.wait()
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum CleanupIssue {
    Error { step: &'static str, detail: String },
    Timeout { step: &'static str },
}

#[cfg(test)]
impl std::fmt::Display for CleanupIssue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Error { step, detail } => write!(formatter, "{step}: {detail}"),
            Self::Timeout { step } => write!(formatter, "{step} timed out"),
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct CleanupFailure {
    graceful_issues: Vec<CleanupIssue>,
    forced_issues: Vec<CleanupIssue>,
}

#[cfg(test)]
impl std::fmt::Display for CleanupFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        for (phase, issues) in [
            ("graceful", &self.graceful_issues),
            ("forced", &self.forced_issues),
        ] {
            for issue in issues {
                if !first {
                    write!(formatter, "; ")?;
                }
                first = false;
                write!(formatter, "{phase} {issue}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
async fn cleanup_step(
    step: &'static str,
    step_timeout: Duration,
    operation: CleanupFuture<'_>,
) -> Result<(), CleanupIssue> {
    match tokio::time::timeout(step_timeout, operation).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(detail)) => Err(CleanupIssue::Error { step, detail }),
        Err(_) => Err(CleanupIssue::Timeout { step }),
    }
}

#[cfg(test)]
async fn cleanup_browser<B: BrowserProcess>(
    browser: &mut B,
    force: bool,
    step_timeout: Duration,
) -> Result<(), CleanupFailure> {
    let mut graceful_issues = Vec::new();

    if !force {
        match cleanup_step("close", step_timeout, browser.close_process()).await {
            Ok(()) => match cleanup_step("wait", step_timeout, browser.wait_process()).await {
                Ok(()) => return Ok(()),
                Err(issue) => graceful_issues.push(issue),
            },
            Err(issue) => graceful_issues.push(issue),
        }
    }

    let mut forced_issues = Vec::new();
    if let Err(issue) = cleanup_step("kill", step_timeout, browser.kill_process()).await {
        forced_issues.push(issue);
    }
    if let Err(issue) = cleanup_step("wait", step_timeout, browser.wait_process()).await {
        forced_issues.push(issue);
    }

    if forced_issues.is_empty() {
        Ok(())
    } else {
        Err(CleanupFailure {
            graceful_issues,
            forced_issues,
        })
    }
}

#[cfg(test)]
async fn finalize_drive<B: BrowserProcess>(
    browser: &mut B,
    handler_task: tokio::task::JoinHandle<()>,
    completion: DriveCompletion,
    cleanup_timeout: Duration,
) -> Result<DriveOutput, BrowserError> {
    let cleanup = cleanup_browser(
        browser,
        matches!(completion, DriveCompletion::TimedOut),
        cleanup_timeout,
    )
    .await;

    handler_task.abort();
    let _ = handler_task.await;

    let primary = match completion {
        DriveCompletion::Completed(result) => result,
        DriveCompletion::TimedOut => Err(BrowserError::Timeout),
    };

    match (primary, cleanup) {
        (result, Ok(())) => result,
        (Ok(_), Err(cleanup)) => Err(BrowserError::Cleanup {
            primary: None,
            detail: cleanup.to_string(),
        }),
        (Err(primary), Err(cleanup)) => Err(BrowserError::Cleanup {
            primary: Some(Box::new(primary)),
            detail: cleanup.to_string(),
        }),
    }
}

fn remaining_request_budget(total: Duration, elapsed: Duration) -> Duration {
    total.saturating_sub(elapsed)
}

async fn drive_page(
    browser: &Browser,
    mode: LaunchMode,
    url: &str,
    wait_strategy: &WaitStrategy,
    credentials: Option<&Credentials>,
) -> Result<DriveOutput, BrowserError> {
    let page = browser
        .new_page("about:blank")
        .await
        .map_err(|error| BrowserError::Drive(error.to_string()))?;
    if let Some(credentials) = credentials {
        page.authenticate(credentials.clone())
            .await
            .map_err(|error| BrowserError::Drive(error.to_string()))?;
    }
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
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    struct FakeBrowserProcess {
        calls: Arc<Mutex<Vec<&'static str>>>,
        wait_completed: Arc<AtomicBool>,
        descendants_alive: Arc<AtomicBool>,
        close_succeeds: bool,
        close_delay: Option<Duration>,
        kill_succeeds: bool,
        kill_delay: Option<Duration>,
        wait_outcomes: VecDeque<(bool, Duration)>,
    }

    impl BrowserProcess for FakeBrowserProcess {
        fn close_process(&mut self) -> CleanupFuture<'_> {
            self.calls.lock().unwrap().push("close");
            let succeeds = self.close_succeeds;
            let delay = self.close_delay;
            Box::pin(async move {
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }
                if succeeds {
                    Ok(())
                } else {
                    Err("close failed".to_string())
                }
            })
        }

        fn kill_process(&mut self) -> CleanupFuture<'_> {
            self.calls.lock().unwrap().push("kill");
            let succeeds = self.kill_succeeds;
            let delay = self.kill_delay;
            Box::pin(async move {
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }
                if succeeds {
                    Ok(())
                } else {
                    Err("kill failed".to_string())
                }
            })
        }

        fn wait_process(&mut self) -> CleanupFuture<'_> {
            self.calls.lock().unwrap().push("wait");
            let completed = Arc::clone(&self.wait_completed);
            let descendants_alive = Arc::clone(&self.descendants_alive);
            let (succeeds, delay) = self
                .wait_outcomes
                .pop_front()
                .expect("missing fake wait outcome");
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                if succeeds {
                    completed.store(true, Ordering::Release);
                    descendants_alive.store(false, Ordering::Release);
                    Ok(())
                } else {
                    Err("wait failed".to_string())
                }
            })
        }
    }

    struct HandlerDrop(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for HandlerDrop {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    fn pending_handler() -> (
        tokio::task::JoinHandle<()>,
        tokio::sync::oneshot::Receiver<()>,
    ) {
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let guard = HandlerDrop(Some(dropped_tx));
        let task = tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        (task, dropped_rx)
    }

    fn fake_output() -> DriveOutput {
        DriveOutput {
            final_url: "https://example.test/".to_string(),
            html: "<html></html>".to_string(),
            cookies: HashMap::new(),
        }
    }

    #[test]
    fn linux_status_uid_uses_the_real_uid() {
        assert_eq!(
            linux_status_uid("Name:\tdraco\nUid:\t0\t1000\t1000\t1000\n"),
            Some(0)
        );
        assert_eq!(
            linux_status_uid("Name:\tdraco\nUid:\t1001\t1001\t1001\t1001\n"),
            Some(1001)
        );
        assert_eq!(linux_status_uid("Name:\tdraco\nUid:\tnot-a-number\n"), None);
    }

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

    #[tokio::test]
    async fn timeout_forces_kill_wait_and_joins_handler_before_returning() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let wait_completed = Arc::new(AtomicBool::new(false));
        let descendants_alive = Arc::new(AtomicBool::new(true));
        let mut browser = FakeBrowserProcess {
            calls: Arc::clone(&calls),
            wait_completed: Arc::clone(&wait_completed),
            descendants_alive: Arc::clone(&descendants_alive),
            close_succeeds: true,
            close_delay: None,
            kill_succeeds: true,
            kill_delay: None,
            wait_outcomes: VecDeque::from([(true, Duration::from_millis(10))]),
        };
        let (handler, handler_dropped) = pending_handler();

        let result = finalize_drive(
            &mut browser,
            handler,
            DriveCompletion::TimedOut,
            Duration::from_millis(100),
        )
        .await;

        assert_eq!(result, Err(BrowserError::Timeout));
        assert_eq!(*calls.lock().unwrap(), ["kill", "wait"]);
        assert!(wait_completed.load(Ordering::Acquire));
        assert!(!descendants_alive.load(Ordering::Acquire));
        assert!(handler_dropped.await.is_ok(), "handler was not joined");
    }

    #[tokio::test]
    async fn normal_completion_closes_waits_and_never_kills() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let wait_completed = Arc::new(AtomicBool::new(false));
        let descendants_alive = Arc::new(AtomicBool::new(true));
        let mut browser = FakeBrowserProcess {
            calls: Arc::clone(&calls),
            wait_completed: Arc::clone(&wait_completed),
            descendants_alive: Arc::clone(&descendants_alive),
            close_succeeds: true,
            close_delay: None,
            kill_succeeds: true,
            kill_delay: None,
            wait_outcomes: VecDeque::from([(true, Duration::from_millis(10))]),
        };
        let (handler, handler_dropped) = pending_handler();

        let result = finalize_drive(
            &mut browser,
            handler,
            DriveCompletion::Completed(Ok(fake_output())),
            Duration::from_millis(100),
        )
        .await;

        assert_eq!(result, Ok(fake_output()));
        assert_eq!(*calls.lock().unwrap(), ["close", "wait"]);
        assert!(wait_completed.load(Ordering::Acquire));
        assert!(!descendants_alive.load(Ordering::Acquire));
        assert!(handler_dropped.await.is_ok(), "handler was not joined");
    }

    #[tokio::test]
    async fn failed_graceful_close_falls_back_to_kill_and_wait() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let wait_completed = Arc::new(AtomicBool::new(false));
        let descendants_alive = Arc::new(AtomicBool::new(true));
        let mut browser = FakeBrowserProcess {
            calls: Arc::clone(&calls),
            wait_completed: Arc::clone(&wait_completed),
            descendants_alive: Arc::clone(&descendants_alive),
            close_succeeds: false,
            close_delay: None,
            kill_succeeds: true,
            kill_delay: None,
            wait_outcomes: VecDeque::from([(true, Duration::from_millis(10))]),
        };
        let (handler, handler_dropped) = pending_handler();
        let original = BrowserError::Drive("page failed".to_string());

        let result = finalize_drive(
            &mut browser,
            handler,
            DriveCompletion::Completed(Err(original.clone())),
            Duration::from_millis(100),
        )
        .await;

        assert_eq!(result, Err(original));
        assert_eq!(*calls.lock().unwrap(), ["close", "kill", "wait"]);
        assert!(wait_completed.load(Ordering::Acquire));
        assert!(!descendants_alive.load(Ordering::Acquire));
        assert!(handler_dropped.await.is_ok());
    }

    #[tokio::test]
    async fn bounded_graceful_close_falls_back_to_kill_and_wait() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let wait_completed = Arc::new(AtomicBool::new(false));
        let descendants_alive = Arc::new(AtomicBool::new(true));
        let mut browser = FakeBrowserProcess {
            calls: Arc::clone(&calls),
            wait_completed: Arc::clone(&wait_completed),
            descendants_alive: Arc::clone(&descendants_alive),
            close_succeeds: true,
            close_delay: Some(Duration::from_secs(1)),
            kill_succeeds: true,
            kill_delay: None,
            wait_outcomes: VecDeque::from([(true, Duration::from_millis(10))]),
        };
        let (handler, handler_dropped) = pending_handler();
        let started = tokio::time::Instant::now();

        let result = finalize_drive(
            &mut browser,
            handler,
            DriveCompletion::Completed(Ok(fake_output())),
            Duration::from_millis(25),
        )
        .await;

        assert_eq!(result, Ok(fake_output()));
        assert_eq!(*calls.lock().unwrap(), ["close", "kill", "wait"]);
        assert!(started.elapsed() < Duration::from_millis(250));
        assert!(wait_completed.load(Ordering::Acquire));
        assert!(!descendants_alive.load(Ordering::Acquire));
        assert!(handler_dropped.await.is_ok());
    }

    #[tokio::test]
    async fn kill_error_is_reported_with_primary_timeout_and_handler_joined() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let wait_completed = Arc::new(AtomicBool::new(false));
        let descendants_alive = Arc::new(AtomicBool::new(true));
        let mut browser = FakeBrowserProcess {
            calls: Arc::clone(&calls),
            wait_completed: Arc::clone(&wait_completed),
            descendants_alive: Arc::clone(&descendants_alive),
            close_succeeds: true,
            close_delay: None,
            kill_succeeds: false,
            kill_delay: None,
            wait_outcomes: VecDeque::from([(false, Duration::from_millis(1))]),
        };
        let (handler, handler_dropped) = pending_handler();

        let result = finalize_drive(
            &mut browser,
            handler,
            DriveCompletion::TimedOut,
            Duration::from_millis(25),
        )
        .await;

        let error = result.expect_err("cleanup failure must replace false Timeout success");
        assert!(matches!(
            error,
            BrowserError::Cleanup {
                primary: Some(ref primary),
                ref detail,
            } if **primary == BrowserError::Timeout
                && detail.contains("kill")
                && detail.contains("wait")
        ));
        assert!(descendants_alive.load(Ordering::Acquire));
        assert!(handler_dropped.await.is_ok());
    }

    #[tokio::test]
    async fn kill_timeout_is_bounded_and_reported() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let wait_completed = Arc::new(AtomicBool::new(false));
        let descendants_alive = Arc::new(AtomicBool::new(true));
        let mut browser = FakeBrowserProcess {
            calls,
            wait_completed,
            descendants_alive: Arc::clone(&descendants_alive),
            close_succeeds: true,
            close_delay: None,
            kill_succeeds: true,
            kill_delay: Some(Duration::from_secs(1)),
            wait_outcomes: VecDeque::from([(false, Duration::from_millis(1))]),
        };
        let (handler, handler_dropped) = pending_handler();
        let started = tokio::time::Instant::now();

        let result = finalize_drive(
            &mut browser,
            handler,
            DriveCompletion::TimedOut,
            Duration::from_millis(25),
        )
        .await;

        assert!(started.elapsed() < Duration::from_millis(250));
        let detail = result.unwrap_err().to_string();
        assert!(detail.contains("kill timed out"), "{detail}");
        assert!(descendants_alive.load(Ordering::Acquire));
        assert!(handler_dropped.await.is_ok());
    }

    #[tokio::test]
    async fn final_wait_error_returns_structured_cleanup_error() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let wait_completed = Arc::new(AtomicBool::new(false));
        let descendants_alive = Arc::new(AtomicBool::new(true));
        let mut browser = FakeBrowserProcess {
            calls: Arc::clone(&calls),
            wait_completed,
            descendants_alive: Arc::clone(&descendants_alive),
            close_succeeds: true,
            close_delay: None,
            kill_succeeds: true,
            kill_delay: None,
            wait_outcomes: VecDeque::from([
                (false, Duration::from_millis(1)),
                (false, Duration::from_millis(1)),
            ]),
        };
        let (handler, handler_dropped) = pending_handler();

        let result = finalize_drive(
            &mut browser,
            handler,
            DriveCompletion::Completed(Ok(fake_output())),
            Duration::from_millis(25),
        )
        .await;

        assert!(matches!(
            result,
            Err(BrowserError::Cleanup { primary: None, ref detail })
                if detail.contains("graceful wait") && detail.contains("forced wait")
        ));
        assert_eq!(*calls.lock().unwrap(), ["close", "wait", "kill", "wait"]);
        assert!(descendants_alive.load(Ordering::Acquire));
        assert!(handler_dropped.await.is_ok());
    }

    #[tokio::test]
    async fn graceful_wait_error_recovers_after_forced_kill_and_wait() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let wait_completed = Arc::new(AtomicBool::new(false));
        let descendants_alive = Arc::new(AtomicBool::new(true));
        let mut browser = FakeBrowserProcess {
            calls: Arc::clone(&calls),
            wait_completed: Arc::clone(&wait_completed),
            descendants_alive: Arc::clone(&descendants_alive),
            close_succeeds: true,
            close_delay: None,
            kill_succeeds: true,
            kill_delay: None,
            wait_outcomes: VecDeque::from([
                (false, Duration::from_millis(1)),
                (true, Duration::from_millis(1)),
            ]),
        };
        let (handler, handler_dropped) = pending_handler();

        let result = finalize_drive(
            &mut browser,
            handler,
            DriveCompletion::Completed(Ok(fake_output())),
            Duration::from_millis(25),
        )
        .await;

        assert_eq!(result, Ok(fake_output()));
        assert_eq!(*calls.lock().unwrap(), ["close", "wait", "kill", "wait"]);
        assert!(wait_completed.load(Ordering::Acquire));
        assert!(!descendants_alive.load(Ordering::Acquire));
        assert!(handler_dropped.await.is_ok());
    }

    #[tokio::test]
    async fn graceful_wait_timeout_recovers_after_forced_kill_and_wait() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let wait_completed = Arc::new(AtomicBool::new(false));
        let descendants_alive = Arc::new(AtomicBool::new(true));
        let mut browser = FakeBrowserProcess {
            calls: Arc::clone(&calls),
            wait_completed: Arc::clone(&wait_completed),
            descendants_alive: Arc::clone(&descendants_alive),
            close_succeeds: true,
            close_delay: None,
            kill_succeeds: true,
            kill_delay: None,
            wait_outcomes: VecDeque::from([
                (true, Duration::from_secs(1)),
                (true, Duration::from_millis(1)),
            ]),
        };
        let (handler, handler_dropped) = pending_handler();
        let started = tokio::time::Instant::now();

        let result = finalize_drive(
            &mut browser,
            handler,
            DriveCompletion::Completed(Ok(fake_output())),
            Duration::from_millis(25),
        )
        .await;

        assert_eq!(result, Ok(fake_output()));
        assert_eq!(*calls.lock().unwrap(), ["close", "wait", "kill", "wait"]);
        assert!(started.elapsed() < Duration::from_millis(250));
        assert!(wait_completed.load(Ordering::Acquire));
        assert!(!descendants_alive.load(Ordering::Acquire));
        assert!(handler_dropped.await.is_ok());
    }

    #[tokio::test]
    async fn wait_timeout_attaches_primary_drive_error() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let wait_completed = Arc::new(AtomicBool::new(false));
        let descendants_alive = Arc::new(AtomicBool::new(true));
        let mut browser = FakeBrowserProcess {
            calls,
            wait_completed,
            descendants_alive: Arc::clone(&descendants_alive),
            close_succeeds: true,
            close_delay: None,
            kill_succeeds: true,
            kill_delay: None,
            wait_outcomes: VecDeque::from([
                (true, Duration::from_secs(1)),
                (true, Duration::from_secs(1)),
            ]),
        };
        let (handler, handler_dropped) = pending_handler();
        let primary = BrowserError::Drive("page failed".to_string());
        let started = tokio::time::Instant::now();

        let result = finalize_drive(
            &mut browser,
            handler,
            DriveCompletion::Completed(Err(primary.clone())),
            Duration::from_millis(25),
        )
        .await;

        assert!(started.elapsed() < Duration::from_millis(250));
        assert!(matches!(
            result,
            Err(BrowserError::Cleanup {
                primary: Some(ref attached),
                ref detail,
            }) if **attached == primary && detail.contains("wait timed out")
        ));
        assert!(descendants_alive.load(Ordering::Acquire));
        assert!(handler_dropped.await.is_ok());
    }

    #[test]
    fn launch_elapsed_time_is_subtracted_from_drive_budget() {
        assert_eq!(
            remaining_request_budget(Duration::from_millis(100), Duration::from_millis(35)),
            Duration::from_millis(65)
        );
        assert_eq!(
            remaining_request_budget(Duration::from_millis(100), Duration::from_millis(150)),
            Duration::ZERO
        );
    }
}
