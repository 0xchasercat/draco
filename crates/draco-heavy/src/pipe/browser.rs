use std::time::Duration;

use crate::browser::{
    BrowserDriver, BrowserError, BrowserSession, CommandBrowserDriver, LaunchMode,
};
use crate::discovery::DetectedBrowser;

/// Production-box wrapper around the portable command driver. New-headless is
/// functional inside the namespace; headed/CDP tuning remains the owner seam.
/// A TCP-only leak decision injects only the safety flag `--disable-quic`.
#[derive(Debug, Clone)]
pub struct NamespaceBrowserDriver {
    pub namespace: String,
    pub quic_enabled: bool,
}

impl BrowserDriver for NamespaceBrowserDriver {
    fn launch(
        &self,
        browser: &DetectedBrowser,
        mode: LaunchMode,
    ) -> Result<Box<dyn BrowserSession>, BrowserError> {
        let wrapper = vec!["netns".into(), "exec".into(), self.namespace.clone()];
        let safety_args = (!self.quic_enabled)
            .then(|| "--disable-quic".to_string())
            .into_iter();
        CommandBrowserDriver::new(Duration::from_secs(45))
            .with_wrapper("ip", wrapper)
            .with_extra_args(safety_args)
            .launch(browser, mode)
    }
}
