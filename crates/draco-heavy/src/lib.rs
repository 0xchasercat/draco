#![forbid(unsafe_code)]

pub mod browser;
pub mod config;
pub mod discovery;
pub mod double_tap;
pub mod local;
#[cfg(feature = "pipe")]
pub mod pipe;
#[cfg(feature = "pipe")]
pub mod server;
#[cfg(feature = "pipe")]
pub mod slots;
pub mod wire;

pub use browser::{BrowserDriver, BrowserSession, LaunchMode};
pub use config::Config;
pub use discovery::{BrowserChannel, DetectedBrowser, HostConfig, LaunchCapabilities, RenderMode};
pub use local::{mint_local, mint_local_with, LocalMintConfig, LocalMintError};
#[cfg(feature = "pipe")]
pub use server::serve;
