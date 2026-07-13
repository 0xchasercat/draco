#![forbid(unsafe_code)]

pub mod config;
pub mod discovery;
pub mod double_tap;
pub mod server;
pub mod slots;
pub mod stubs;
pub mod wire;

pub use config::Config;
pub use discovery::{HostConfig, RenderMode};
pub use server::serve;
