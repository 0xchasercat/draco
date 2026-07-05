//! The `PageFetcher` seam.
//!
//! The escalation state machine never calls [`draco_net`] directly. Instead it
//! talks to a [`PageFetcher`], and the production wiring is a thin adapter
//! ([`NetFetcher`]) that delegates to `draco_net::fetch_target` / `replay`.
//!
//! This indirection exists for one reason: **offline testability**. In WS-C the
//! `draco-net` crate is still a `todo!()` stub, so calling it from a unit test
//! would panic. By routing every fetch through this trait, tests can supply a
//! mock fetcher that returns fixture HTML, and the whole ladder — challenge
//! detection, tier sequencing, trace/timing assembly — becomes exercisable
//! without a network or a working `draco-net`.

use async_trait::async_trait;
use draco_net::{HtmlResponse, SessionOpts};
use draco_types::{DracoError, HttpRequestSpec};

/// Abstraction over the network layer used by the escalation state machine.
///
/// Two operations, mirroring the frozen `draco-net` surface:
/// - [`fetch`](PageFetcher::fetch): the Tier 0 GET of the target URL.
/// - [`replay`](PageFetcher::replay): re-issue a constructed (Tier 1) or
///   intercepted (Tier 2) [`HttpRequestSpec`].
///
/// The trait is object-safe (via `async_trait`) so the machine can hold a
/// `&dyn PageFetcher` and be driven by either the production adapter or a test
/// double.
#[async_trait]
pub trait PageFetcher: Send + Sync {
    /// Fetch a page (Tier 0 entry point).
    async fn fetch(&self, url: &str, opts: &SessionOpts) -> Result<HtmlResponse, DracoError>;

    /// Replay a request spec (Tier 1 build-id URL, or a Tier 2 winner).
    async fn replay(
        &self,
        spec: &HttpRequestSpec,
        opts: &SessionOpts,
    ) -> Result<HtmlResponse, DracoError>;
}

/// Production [`PageFetcher`] — delegates straight to `draco-net`.
///
/// This is the only place in `draco-core` that names the concrete network
/// functions, keeping the rest of the crate independent of `draco-net`'s
/// (currently stubbed) implementation.
#[derive(Debug, Default, Clone, Copy)]
pub struct NetFetcher;

#[async_trait]
impl PageFetcher for NetFetcher {
    async fn fetch(&self, url: &str, opts: &SessionOpts) -> Result<HtmlResponse, DracoError> {
        draco_net::fetch_target(url, opts).await
    }

    async fn replay(
        &self,
        spec: &HttpRequestSpec,
        opts: &SessionOpts,
    ) -> Result<HtmlResponse, DracoError> {
        draco_net::replay(spec, opts).await
    }
}
