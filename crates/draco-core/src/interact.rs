//! Production driver for resumable Tier 2 interact sessions.
//!
//! The runtime owns the thread-bound V8 actor; this module supplies Draco's
//! network posture around it. One operation-scoped cookie jar is shared by the
//! initial document fetch, script/module loads, page API requests, and explicit
//! navigations, so the session behaves like one browser tab without widening
//! the isolate's no-host-bindings boundary.

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use draco_types::{DracoError, ExtractionResult, JailKind, SourceTier, Status, Timing};

use crate::chunk_cache::ChunkCache;
use crate::tier2::prod::{capture_config, NetApiFetcher, NetScriptFetcher};
use crate::tier2::{jail_error, subresource_opts, CaptureMode};
use crate::{Config, FormatSet};

/// Cookie-aware top-level document fetcher used by explicit session navigation.
struct NetPageFetcher {
    opts: draco_net::SessionOpts,
}

impl draco_runtime::session::PageFetcher for NetPageFetcher {
    fn fetch_page<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<(String, String)>> + 'a>> {
        Box::pin(async move {
            match draco_net::fetch_target(url, &self.opts).await {
                Ok(resp) if (200..300).contains(&resp.meta.status) => Some((
                    resp.meta.final_url.clone(),
                    String::from_utf8_lossy(&resp.body).into_owned(),
                )),
                _ => None,
            }
        })
    }
}

/// Fetch, hydrate, and hold one live interact session.
///
/// Transport failure on the initial document is returned directly. HTTP error
/// pages are still valid documents and therefore hydrate normally. The returned
/// handle is `Send`; the isolate and its `Rc` fetchers stay on the dedicated
/// session thread created by [`draco_runtime::session::Session::open`].
pub async fn open_interact_session(
    url: &str,
    config: &Config,
) -> Result<draco_runtime::session::Session, DracoError> {
    let mut opts = crate::session_opts(config);
    if opts.cookie_jar.is_none() {
        opts.cookie_jar = Some(draco_net::SharedCookieJar::new());
    }

    let resp = draco_net::fetch_target(url, &opts).await?;
    let html = String::from_utf8_lossy(&resp.body).into_owned();
    let final_url = resp.meta.final_url.clone();

    let network_opts = subresource_opts(&opts);
    let page_opts = opts.clone();
    let cache = ChunkCache::shared();
    let allow_unsafe = config.allow_unsafe_replay;
    let factory: draco_runtime::session::FetcherFactory = Box::new(move || {
        draco_runtime::session::SessionFetchers {
            scripts: Rc::new(NetScriptFetcher {
                opts: network_opts.clone(),
                cache,
            }),
            api: Some(Rc::new(NetApiFetcher {
                opts: network_opts,
                allow_unsafe,
            })),
            page: Some(Rc::new(NetPageFetcher { opts: page_opts })),
        }
    });

    let capture = capture_config(config, CaptureMode::Render);
    draco_runtime::session::Session::open(
        draco_runtime::session::SessionConfig {
            url: final_url,
            html,
            capture,
        },
        factory,
    )
    .await
    .map_err(|e| jail_error(JailKind::Spawn, e.to_string()))
}

/// Project a live session's serialized DOM through Draco's existing content
/// engine and return the standard extraction envelope.
///
/// Interact serialization is already the current full document, so no shell
/// merge is needed. Only DOM-derived formats are meaningful here; callers reject
/// `json` and `endpoints` before invoking this helper.
pub fn scrape_interact_html(
    url: &str,
    html: &str,
    formats: FormatSet,
    only_main_content: bool,
) -> ExtractionResult {
    let scraped = draco_static::content::scrape(
        html,
        url,
        200,
        "text/html; charset=utf-8",
        only_main_content,
    );
    ExtractionResult {
        url: url.to_string(),
        status: Status::Success,
        source_tier: Some(SourceTier::RuntimeInterception),
        data: None,
        markdown: formats.markdown.then_some(scraped.markdown),
        metadata: Some(scraped.metadata),
        html: formats
            .html
            .then(|| draco_static::content::clean_html(html, url, only_main_content)),
        raw_html: formats.raw_html.then(|| html.to_string()),
        links: formats
            .links
            .then(|| draco_static::content::extract_links(html, url)),
        endpoints: None,
        timing: Timing::default(),
        trace: Vec::new(),
        error: None,
    }
}
