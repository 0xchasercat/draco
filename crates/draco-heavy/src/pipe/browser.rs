use crate::browser::{BrowserDriver, BrowserError, BrowserSession, LaunchMode};
use crate::discovery::DetectedBrowser;

/// Production-box browser seam. The network lifecycle and leak gate are real;
/// the owner-selected Chrome/CDP implementation must launch inside `namespace`
/// and honor `quic_enabled` (adding `--disable-quic` only on TCP-only degrade).
#[derive(Debug, Clone)]
pub struct NamespaceBrowserDriver {
    pub namespace: String,
    pub quic_enabled: bool,
}

impl BrowserDriver for NamespaceBrowserDriver {
    fn launch(
        &self,
        _browser: &DetectedBrowser,
        _mode: LaunchMode,
    ) -> Result<Box<dyn BrowserSession>, BrowserError> {
        Err(BrowserError::Unsupported(format!(
            "owner CDP seam must launch Chrome inside netns {} (quic_enabled={})",
            self.namespace, self.quic_enabled
        )))
    }
}
