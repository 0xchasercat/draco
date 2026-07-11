//! Firecrawl-compatible metasearch core for `POST /v1/search`.
//!
//! This module deliberately contains no axum handler. It owns the reusable
//! search machinery only: engine request construction, defensive SERP parsing,
//! concurrent fan-out, URL canonicalization, and reciprocal-rank consensus.
//! The REST, CLI, and MCP surfaces can all call the same public API.
//!
//! Search engines are independent failure domains. A timeout, transport error,
//! non-2xx response, or parse miss from one engine is recorded in
//! [`EngineOutcome`] and never prevents useful results from the others. Every
//! request uses `draco-net`; DuckDuckGo's HTML endpoint is the sole POST engine
//! and therefore uses `draco_net::replay`, while the GET engines use
//! `draco_net::fetch_target`.
//!
//! SERP parsing intentionally uses a small defensive scanner rather than a DOM
//! dependency. Engine pages are external, unstable input: missing attributes or
//! malformed markup skip a hit instead of panicking.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use draco_net::{fetch_target, replay, SessionOpts};
use draco_types::HttpRequestSpec;
use url::{form_urlencoded, Url};

/// Recommended independent timeout for each built-in engine fetch.
pub const DEFAULT_PER_ENGINE_TIMEOUT: Duration = Duration::from_secs(15);

/// One result returned by an individual engine or the consensus merger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub description: String,
    pub url: String,
    pub engine: &'static str,
    /// One-based position in `engine`'s result list.
    pub rank: usize,
    /// Every `(engine, rank)` that contributed to this canonical result.
    /// Parsers populate one entry; [`consensus`] replaces it with the full,
    /// engine-deduplicated contribution set for Draco diagnostics.
    pub contributors: Vec<(&'static str, usize)>,
}

/// Engine-neutral search parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchParams {
    pub query: String,
    /// Requested consensus result count. The caller owns the public API clamp
    /// (Firecrawl default 5, maximum 100).
    pub limit: usize,
    /// Firecrawl/Google-style time filter. Reserved for engines that can map it
    /// without inventing semantics; currently best-effort and not forwarded.
    pub tbs: Option<String>,
    /// Best-effort location hint. Engines disagree on its format, so adapters
    /// may leave it unused rather than silently reinterpret it.
    pub location: Option<String>,
}

impl Default for SearchParams {
    fn default() -> Self {
        Self {
            query: String::new(),
            limit: 5,
            tbs: None,
            location: None,
        }
    }
}

/// HTTP method required by a search engine request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
}

/// A swappable search-engine adapter.
///
/// Implementations construct one request and parse one response. Network I/O
/// stays in [`search_all`] so every adapter gets identical timeout and failure
/// handling.
pub trait SearchEngine: Send + Sync {
    fn name(&self) -> &'static str;
    fn build_url(&self, params: &SearchParams) -> String;

    fn method(&self) -> HttpMethod {
        HttpMethod::Get
    }

    fn body(&self, _params: &SearchParams) -> Option<String> {
        None
    }

    /// Parse an engine response. `base_url` is the final response URL when the
    /// transport supplied one, and is used to absolutize relative result URLs.
    fn parse(&self, html: &str, base_url: &str) -> Vec<SearchHit>;
}

/// DuckDuckGo's non-JavaScript HTML endpoint.
#[derive(Debug, Default, Clone, Copy)]
pub struct DuckDuckGo;

/// Bing's server-rendered web SERP.
#[derive(Debug, Default, Clone, Copy)]
pub struct Bing;

/// Brave Search's server-rendered web SERP.
#[derive(Debug, Default, Clone, Copy)]
pub struct Brave;

/// Baidu's server-rendered web SERP.
#[derive(Debug, Default, Clone, Copy)]
pub struct Baidu;

/// ZapMeta's server-rendered metasearch SERP.
#[derive(Debug, Default, Clone, Copy)]
pub struct ZapMeta;

/// Yandex's server-rendered web SERP.
#[derive(Debug, Default, Clone, Copy)]
pub struct Yandex;

/// Mojeek's server-rendered web SERP.
///
/// Kept implemented but intentionally OUT of [`default_engines`] — its live
/// endpoint gates automated queries behind an Altcha challenge (HTTP 403), so it
/// serves as the canonical failure-path fixture (exercised by the parser tests),
/// not a default engine. `dead_code` is allowed because a bin crate does not
/// treat test-only construction as a use.
#[allow(dead_code)]
#[derive(Debug, Default, Clone, Copy)]
pub struct Mojeek;

/// Per-engine terminal status for one fan-out operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineStatus {
    Ok(usize),
    Timeout,
    Http(u16),
    Error(String),
    Empty,
}

/// Diagnostic outcome for one engine. Failures are intentionally data, not a
/// top-level error: callers decide whether an all-engine failure should become
/// an HTTP error response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineOutcome {
    pub name: &'static str,
    pub status: EngineStatus,
}

/// The six live v0.16.0 engines in stable diagnostic order.
///
/// [`Mojeek`] remains implemented for compatibility and failure-path tests but
/// is excluded here: its supplied datacenter capture is a hard 403, so running
/// it by default would add a predictably empty request rather than redundancy.
pub fn default_engines() -> Vec<Box<dyn SearchEngine + Send + Sync>> {
    vec![
        Box::new(DuckDuckGo),
        Box::new(Bing),
        Box::new(Brave),
        Box::new(Baidu),
        Box::new(ZapMeta),
        Box::new(Yandex),
    ]
}

impl SearchEngine for DuckDuckGo {
    fn name(&self) -> &'static str {
        "duckduckgo"
    }

    fn build_url(&self, _params: &SearchParams) -> String {
        "https://html.duckduckgo.com/html/".to_string()
    }

    fn method(&self) -> HttpMethod {
        HttpMethod::Post
    }

    fn body(&self, params: &SearchParams) -> Option<String> {
        let mut serializer = form_urlencoded::Serializer::new(String::new());
        serializer.append_pair("q", &params.query);
        serializer.append_pair("b", "");
        Some(serializer.finish())
    }

    fn parse(&self, html: &str, base_url: &str) -> Vec<SearchHit> {
        // DuckDuckGo's HTML endpoint contract uses `a.result__a` followed by a
        // `result__snippet` node. The supplied 2026-07-10 capture is an anomaly
        // challenge generated by an incorrect GET request and contains neither;
        // the fixture-positive test is therefore ignored and this selector
        // contract is covered with representative static markup instead.
        let starts = tag_ranges(html, "a")
            .into_iter()
            .filter(|(start, end)| has_class_token(&html[*start..*end], "result__a"))
            .collect::<Vec<_>>();
        let mut hits = Vec::new();

        for (position, (start, end)) in starts.iter().copied().enumerate() {
            let block_end = starts
                .get(position + 1)
                .map(|(next, _)| *next)
                .unwrap_or(html.len());
            let tag = &html[start..end];
            let Some(raw_href) = attr_value(tag, "href") else {
                continue;
            };
            let Some(title_html) = element_inner(html, end, "a") else {
                continue;
            };
            let title = clean_text(title_html);
            if title.is_empty() {
                continue;
            }
            let Some(url) = absolutize_result_url(raw_href, base_url, true) else {
                continue;
            };
            let block = &html[start..block_end];
            let description =
                first_text_by_classes(block, &["result__snippet"]).unwrap_or_default();
            let rank = position + 1;
            hits.push(engine_hit(self.name(), rank, title, description, url));
        }
        hits
    }
}

impl SearchEngine for Bing {
    fn name(&self) -> &'static str {
        "bing"
    }

    fn build_url(&self, params: &SearchParams) -> String {
        build_query_url("https://www.bing.com/search", &params.query)
    }

    fn parse(&self, html: &str, base_url: &str) -> Vec<SearchHit> {
        // Stable Bing markup is `li.b_algo`, with the primary link under
        // `h2 > a` and copy in `.b_caption p` or `.b_algoSlug`. The supplied
        // 2026-07-10 datacenter capture has no `b_algo` or `h2`: it is a full
        // Cloudflare Turnstile challenge shell. Returning empty for that exact
        // variant is expected and lets the other engines continue.
        let starts = tag_ranges(html, "li")
            .into_iter()
            .filter(|(start, end)| has_class_token(&html[*start..*end], "b_algo"))
            .collect::<Vec<_>>();
        let mut hits = Vec::new();

        for (position, (start, _)) in starts.iter().copied().enumerate() {
            let block_end = starts
                .get(position + 1)
                .map(|(next, _)| *next)
                .unwrap_or(html.len());
            let block = &html[start..block_end];
            let Some((raw_href, title)) = first_anchor_in_tag(block, "h2") else {
                continue;
            };
            let Some(url) = absolutize_result_url(&raw_href, base_url, false) else {
                continue;
            };

            let description = first_class_start(block, &["b_caption"])
                .and_then(|caption| first_text_for_tag(&block[caption..], "p"))
                .or_else(|| first_text_by_classes(block, &["b_algoSlug"]))
                .unwrap_or_default();
            let rank = position + 1;
            hits.push(engine_hit(self.name(), rank, title, description, url));
        }
        hits
    }
}

impl SearchEngine for Brave {
    fn name(&self) -> &'static str {
        "brave"
    }

    fn build_url(&self, params: &SearchParams) -> String {
        build_query_url("https://search.brave.com/search", &params.query)
    }

    fn parse(&self, html: &str, base_url: &str) -> Vec<SearchHit> {
        // Verified against the rich 2026-07-10 fixture: 20 web results are
        // `div` nodes with the stable `snippet` class token and
        // `data-type="web"`. Svelte hash classes are intentionally ignored.
        // The primary link has stable class token `l1`, the title has `title`
        // plus `search-snippet-title`, and ordinary result copy has `content`,
        // `desktop-default-regular`, and `t-primary` tokens.
        let starts = tag_ranges(html, "div")
            .into_iter()
            .filter(|(start, end)| {
                let tag = &html[*start..*end];
                has_class_token(tag, "snippet")
                    && attr_value(tag, "data-type")
                        .is_some_and(|value| value.eq_ignore_ascii_case("web"))
            })
            .collect::<Vec<_>>();
        let mut hits = Vec::new();

        for (position, (start, _)) in starts.iter().copied().enumerate() {
            let block_end = starts
                .get(position + 1)
                .map(|(next, _)| *next)
                .unwrap_or(html.len());
            let block = &html[start..block_end];
            let Some((raw_href, anchor_title)) = first_anchor(block, Some("l1")) else {
                continue;
            };
            let title = first_text_by_classes(block, &["title", "search-snippet-title"])
                .filter(|value| !value.is_empty())
                .unwrap_or(anchor_title);
            if title.is_empty() {
                continue;
            }
            let Some(url) = absolutize_result_url(&raw_href, base_url, false) else {
                continue;
            };
            let description =
                first_text_by_classes(block, &["content", "desktop-default-regular", "t-primary"])
                    .unwrap_or_default();
            let rank = position + 1;
            hits.push(engine_hit(self.name(), rank, title, description, url));
        }
        hits
    }
}

impl SearchEngine for Baidu {
    fn name(&self) -> &'static str {
        "baidu"
    }

    fn build_url(&self, params: &SearchParams) -> String {
        match Url::parse("https://www.baidu.com/s") {
            Ok(mut url) => {
                url.query_pairs_mut()
                    .append_pair("wd", &params.query)
                    .append_pair("ie", "utf-8");
                url.to_string()
            }
            Err(_) => format!(
                "https://www.baidu.com/s?wd={}&ie=utf-8",
                form_urlencoded::byte_serialize(params.query.as_bytes()).collect::<String>()
            ),
        }
    }

    fn parse(&self, html: &str, base_url: &str) -> Vec<SearchHit> {
        // Verified against the 2026-07-10 fixture. Organic results are the nine
        // `div.result.c-container` nodes; the tenth `c-container` is a
        // `result-op` answer card and is intentionally excluded. Titles and
        // Baidu redirect URLs live under `h3 > a`. All nine fixture snippets
        // use `data-module="abstract"`; class-substring fallbacks cover Baidu's
        // documented `content-right*` and `*abstract*` variants.
        let starts = tag_ranges(html, "div")
            .into_iter()
            .filter(|(start, end)| {
                let tag = &html[*start..*end];
                has_class_token(tag, "result") && has_class_token(tag, "c-container")
            })
            .collect::<Vec<_>>();
        let mut hits = Vec::new();

        for (position, (start, _)) in starts.iter().copied().enumerate() {
            let block_end = starts
                .get(position + 1)
                .map(|(next, _)| *next)
                .unwrap_or(html.len());
            let block = &html[start..block_end];
            let Some((raw_href, title)) = first_anchor_in_tag(block, "h3") else {
                continue;
            };
            // Baidu title URLs are intentionally retained as proxied
            // `baidu.com/link?url=...` redirects. Resolving them requires a live
            // follow and belongs outside this pure parser.
            let Some(url) = absolutize_result_url(&raw_href, base_url, false) else {
                continue;
            };
            let description = first_text_by_class_substring(block, "content-right")
                .or_else(|| first_text_by_class_substring(block, "abstract"))
                .or_else(|| first_text_by_attr(block, "data-module", "abstract"))
                .unwrap_or_default();
            let rank = position + 1;
            hits.push(engine_hit(self.name(), rank, title, description, url));
        }
        hits
    }
}

impl SearchEngine for ZapMeta {
    fn name(&self) -> &'static str {
        "zapmeta"
    }

    fn build_url(&self, params: &SearchParams) -> String {
        build_query_url("https://www.zapmeta.com/search", &params.query)
    }

    fn parse(&self, html: &str, base_url: &str) -> Vec<SearchHit> {
        // Verified against the 2026-07-10 fixture: exactly nine organic
        // `<article>` results, each with a multiline-capable `h2 > a`, a `<p>`
        // snippet, and a separate `organic-results__display-url-link`. The
        // title link is canonical; the displayed-URL anchor is a fallback only.
        let starts = tag_ranges(html, "article");
        let mut hits = Vec::new();

        for (position, (start, _)) in starts.iter().copied().enumerate() {
            let block_end = starts
                .get(position + 1)
                .map(|(next, _)| *next)
                .unwrap_or(html.len());
            let block = &html[start..block_end];
            let (title, raw_href) = match first_anchor_in_tag(block, "h2") {
                Some((href, title)) => (Some(title), Some(href)),
                None => (
                    first_text_for_tag(block, "h2"),
                    first_href_by_classes(block, &["organic-results__display-url-link"]),
                ),
            };
            let (Some(title), Some(raw_href)) = (title, raw_href) else {
                continue;
            };
            let Some(url) = absolutize_result_url(&raw_href, base_url, false) else {
                continue;
            };
            let description = first_text_for_tag(block, "p").unwrap_or_default();
            let rank = position + 1;
            hits.push(engine_hit(self.name(), rank, title, description, url));
        }
        hits
    }
}

impl SearchEngine for Yandex {
    fn name(&self) -> &'static str {
        "yandex"
    }

    fn build_url(&self, params: &SearchParams) -> String {
        match Url::parse("https://yandex.com/search/") {
            Ok(mut url) => {
                url.query_pairs_mut().append_pair("text", &params.query);
                url.to_string()
            }
            Err(_) => format!(
                "https://yandex.com/search/?text={}",
                form_urlencoded::byte_serialize(params.query.as_bytes()).collect::<String>()
            ),
        }
    }

    fn parse(&self, html: &str, base_url: &str) -> Vec<SearchHit> {
        // Yandex's documented raw-HTML contract is `li.serp-item`, title under
        // `h2 a` / `a.OrganicTitle-Link` / `a.Link`, copy under
        // `.TextContainer` or an `OrganicText*` class, and URL paths under
        // `.Path` / `Path-Item*`. The datacenter probe redirected to a captcha,
        // so the successful shape is synthetic-tested rather than fixture-
        // claimed. A challenge body contains no `serp-item` and returns empty.
        let starts = tag_ranges(html, "li")
            .into_iter()
            .filter(|(start, end)| has_class_token(&html[*start..*end], "serp-item"))
            .collect::<Vec<_>>();
        let mut hits = Vec::new();

        for (position, (start, _)) in starts.iter().copied().enumerate() {
            let block_end = starts
                .get(position + 1)
                .map(|(next, _)| *next)
                .unwrap_or(html.len());
            let block = &html[start..block_end];
            let title_link = first_anchor_in_tag(block, "h2")
                .or_else(|| first_anchor(block, Some("OrganicTitle-Link")))
                .or_else(|| first_anchor(block, Some("Link")));
            let (title, raw_href) = match title_link {
                Some((href, title)) => (Some(title), Some(href)),
                None => (
                    first_text_for_tag(block, "h2"),
                    first_href_by_classes(block, &["Path"])
                        .or_else(|| first_href_by_class_substring(block, "Path-Item")),
                ),
            };
            let (Some(title), Some(raw_href)) = (title, raw_href) else {
                continue;
            };
            let Some(url) = absolutize_result_url(&raw_href, base_url, false) else {
                continue;
            };
            let description = first_text_by_classes(block, &["TextContainer"])
                .or_else(|| first_text_by_class_substring(block, "OrganicText"))
                .unwrap_or_default();
            let rank = position + 1;
            hits.push(engine_hit(self.name(), rank, title, description, url));
        }
        hits
    }
}

impl SearchEngine for Mojeek {
    fn name(&self) -> &'static str {
        "mojeek"
    }

    fn build_url(&self, params: &SearchParams) -> String {
        build_query_url("https://www.mojeek.com/search", &params.query)
    }

    fn parse(&self, html: &str, base_url: &str) -> Vec<SearchHit> {
        // Mojeek documents standard results under `ul.results-standard > li`,
        // with `a.title` (or `h2 > a`) and a following paragraph. The supplied
        // 2026-07-10 fixture is a 337-byte automated-query 403, so this positive
        // markup could not be fixture-verified; the block body must simply miss
        // the list and return empty without panicking.
        let Some(list_start) = first_class_start_for_tag(html, "ul", &["results-standard"]) else {
            return Vec::new();
        };
        let list_tail = &html[list_start..];
        let list_end = find_ascii_case_insensitive(list_tail, "</ul>").unwrap_or(list_tail.len());
        let list = &list_tail[..list_end];
        let starts = tag_ranges(list, "li");
        let mut hits = Vec::new();

        for (position, (start, _)) in starts.iter().copied().enumerate() {
            let block_end = starts
                .get(position + 1)
                .map(|(next, _)| *next)
                .unwrap_or(list.len());
            let block = &list[start..block_end];
            let title_link =
                first_anchor(block, Some("title")).or_else(|| first_anchor_in_tag(block, "h2"));
            let Some((raw_href, title)) = title_link else {
                continue;
            };
            let Some(url) = absolutize_result_url(&raw_href, base_url, false) else {
                continue;
            };
            let description = first_text_for_tag(block, "p").unwrap_or_default();
            let rank = position + 1;
            hits.push(engine_hit(self.name(), rank, title, description, url));
        }
        hits
    }
}

/// Canonical grouping key for consensus deduplication.
///
/// The key intentionally ignores the HTTP/HTTPS scheme so equivalent public
/// URLs returned with different schemes can merge. It lowercases the host,
/// drops default ports and fragments, removes a trailing path slash, strips
/// common tracking parameters (`utm_*`, `ref`, and `fbclid`), and preserves the
/// path plus every meaningful query pair.
pub fn canonical_key(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    let parsed = Url::parse(raw).or_else(|_| Url::parse(&format!("https://{raw}")));
    let Ok(url) = parsed else {
        return fallback_canonical_key(raw);
    };
    let Some(host) = url.host_str() else {
        return fallback_canonical_key(raw);
    };

    let mut key = host.to_ascii_lowercase();
    if let Some(port) = url.port() {
        key.push(':');
        key.push_str(&port.to_string());
    }

    let path = url.path().trim_end_matches('/');
    if !path.is_empty() {
        if !path.starts_with('/') {
            key.push('/');
        }
        key.push_str(path);
    }

    let meaningful = url
        .query_pairs()
        .filter(|(name, _)| !is_tracking_param(name))
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    if !meaningful.is_empty() {
        let mut serializer = form_urlencoded::Serializer::new(String::new());
        for (name, value) in meaningful {
            serializer.append_pair(&name, &value);
        }
        key.push('?');
        key.push_str(&serializer.finish());
    }
    key
}

/// Merge engine hits by [`canonical_key`] using reciprocal rank.
///
/// Each canonical group scores `sum(1 / rank)` across distinct engines. If an
/// engine emitted the same URL twice, only its best rank contributes. The
/// representative is selected by lowest rank, then richer title/description,
/// then deterministic URL/title/engine order. `limit` is applied only after all
/// groups have merged and sorted.
pub fn consensus(hits: Vec<SearchHit>, limit: usize) -> Vec<SearchHit> {
    #[derive(Debug)]
    struct Group {
        representative: SearchHit,
        contributions: BTreeMap<&'static str, usize>,
    }

    let mut groups: HashMap<String, Group> = HashMap::new();
    for hit in hits {
        let key = canonical_key(&hit.url);
        let key = if key.is_empty() {
            hit.url.trim().to_ascii_lowercase()
        } else {
            key
        };
        let source_contributions = if hit.contributors.is_empty() {
            vec![(hit.engine, hit.rank.max(1))]
        } else {
            hit.contributors
                .iter()
                .map(|(engine, rank)| (*engine, (*rank).max(1)))
                .collect()
        };

        match groups.get_mut(&key) {
            Some(group) => {
                for (engine, rank) in source_contributions {
                    group
                        .contributions
                        .entry(engine)
                        .and_modify(|current| *current = (*current).min(rank))
                        .or_insert(rank);
                }
                if better_representative(&hit, &group.representative) {
                    group.representative = hit;
                }
            }
            None => {
                let mut contributions = BTreeMap::new();
                for (engine, rank) in source_contributions {
                    contributions
                        .entry(engine)
                        .and_modify(|current: &mut usize| *current = (*current).min(rank))
                        .or_insert(rank);
                }
                groups.insert(
                    key,
                    Group {
                        representative: hit,
                        contributions,
                    },
                );
            }
        }
    }

    let mut merged = groups
        .into_values()
        .map(|group| {
            let score = group
                .contributions
                .values()
                .map(|rank| 1.0 / *rank as f64)
                .sum::<f64>();
            let best_rank = group
                .contributions
                .values()
                .copied()
                .min()
                .unwrap_or(usize::MAX);
            let mut representative = group.representative;
            representative.contributors = group.contributions.into_iter().collect();
            (score, best_rank, representative)
        })
        .collect::<Vec<_>>();

    merged.sort_by(|left, right| {
        right
            .0
            .total_cmp(&left.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.url.cmp(&right.2.url))
            .then_with(|| left.2.title.cmp(&right.2.title))
            .then_with(|| left.2.engine.cmp(right.2.engine))
    });
    merged
        .into_iter()
        .take(limit)
        .map(|(_, _, hit)| hit)
        .collect()
}

/// Fan out across all engines with default [`SessionOpts`], merge successful
/// results, and return one diagnostic outcome per engine.
///
/// The non-session convenience entry; the REST/CLI/MCP adapters all use
/// [`search_all_with_session`] to inherit Draco's proxy/robots/header posture, so
/// this is unused in the bin build today — kept as the documented simple entry.
#[allow(dead_code)]
pub async fn search_all(
    params: &SearchParams,
    engines: &[Box<dyn SearchEngine + Send + Sync>],
    per_engine_timeout: Duration,
) -> (Vec<SearchHit>, Vec<EngineOutcome>) {
    search_all_with_session(params, engines, per_engine_timeout, &SessionOpts::default()).await
}

/// Fan out with caller-provided network options. REST/CLI/MCP integration should
/// use this form to inherit Draco's proxy, robots posture, headers, and request
/// timeout while preserving the independent outer per-engine timeout.
pub async fn search_all_with_session(
    params: &SearchParams,
    engines: &[Box<dyn SearchEngine + Send + Sync>],
    per_engine_timeout: Duration,
    session: &SessionOpts,
) -> (Vec<SearchHit>, Vec<EngineOutcome>) {
    search_all_with_fetcher(
        params,
        engines,
        per_engine_timeout,
        session,
        Arc::new(NetFetcher),
    )
    .await
}

#[derive(Debug, Clone)]
struct EngineRequest {
    method: HttpMethod,
    url: String,
    body: Option<String>,
}

#[derive(Debug)]
struct EngineResponse {
    status: u16,
    final_url: String,
    body: String,
}

type FetchFuture = Pin<Box<dyn Future<Output = Result<EngineResponse, String>> + Send>>;

trait EngineFetcher: Send + Sync {
    fn fetch(self: Arc<Self>, request: EngineRequest, session: SessionOpts) -> FetchFuture;
}

#[derive(Debug)]
struct NetFetcher;

impl EngineFetcher for NetFetcher {
    fn fetch(self: Arc<Self>, request: EngineRequest, session: SessionOpts) -> FetchFuture {
        Box::pin(async move {
            let response = match request.method {
                HttpMethod::Get => fetch_target(&request.url, &session)
                    .await
                    .map_err(|error| format!("{error:?}"))?,
                HttpMethod::Post => {
                    let body = request.body.unwrap_or_default();
                    let spec = HttpRequestSpec {
                        method: "POST".to_string(),
                        url: request.url,
                        headers: vec![(
                            "content-type".to_string(),
                            "application/x-www-form-urlencoded".to_string(),
                        )],
                        body_b64: Some(
                            base64::engine::general_purpose::STANDARD.encode(body.as_bytes()),
                        ),
                    };
                    replay(&spec, &session)
                        .await
                        .map_err(|error| format!("{error:?}"))?
                }
            };
            Ok(EngineResponse {
                status: response.meta.status,
                final_url: response.meta.final_url,
                body: String::from_utf8_lossy(&response.body).into_owned(),
            })
        })
    }
}

async fn search_all_with_fetcher<F>(
    params: &SearchParams,
    engines: &[Box<dyn SearchEngine + Send + Sync>],
    per_engine_timeout: Duration,
    session: &SessionOpts,
    fetcher: Arc<F>,
) -> (Vec<SearchHit>, Vec<EngineOutcome>)
where
    F: EngineFetcher + 'static,
{
    let mut tasks = tokio::task::JoinSet::new();
    for (index, engine) in engines.iter().enumerate() {
        let request = EngineRequest {
            method: engine.method(),
            url: engine.build_url(params),
            body: engine.body(params),
        };
        let fetcher = fetcher.clone();
        let session = session.clone();
        tasks.spawn(async move {
            let result =
                tokio::time::timeout(per_engine_timeout, fetcher.fetch(request, session)).await;
            (index, result)
        });
    }

    let mut all_hits = Vec::new();
    let mut indexed_outcomes: Vec<Option<EngineOutcome>> = vec![None; engines.len()];
    while let Some(joined) = tasks.join_next().await {
        let Ok((index, timed)) = joined else {
            // A task panic/cancellation cannot identify its engine from the
            // JoinError alone. Missing slots are filled deterministically below.
            continue;
        };
        let engine = &engines[index];
        let status = match timed {
            Err(_) => EngineStatus::Timeout,
            Ok(Err(error)) => EngineStatus::Error(error),
            Ok(Ok(response)) if !(200..300).contains(&response.status) => {
                EngineStatus::Http(response.status)
            }
            Ok(Ok(response)) => {
                let base_url = if response.final_url.is_empty() {
                    engine.build_url(params)
                } else {
                    response.final_url
                };
                let hits = engine.parse(&response.body, &base_url);
                if hits.is_empty() {
                    EngineStatus::Empty
                } else {
                    let count = hits.len();
                    all_hits.extend(hits);
                    EngineStatus::Ok(count)
                }
            }
        };
        indexed_outcomes[index] = Some(EngineOutcome {
            name: engine.name(),
            status,
        });
    }

    let outcomes = indexed_outcomes
        .into_iter()
        .enumerate()
        .map(|(index, outcome)| {
            outcome.unwrap_or_else(|| EngineOutcome {
                name: engines[index].name(),
                status: EngineStatus::Error("engine task failed to join".to_string()),
            })
        })
        .collect();
    (consensus(all_hits, params.limit), outcomes)
}

fn engine_hit(
    engine: &'static str,
    rank: usize,
    title: String,
    description: String,
    url: String,
) -> SearchHit {
    SearchHit {
        title,
        description,
        url,
        engine,
        rank,
        contributors: vec![(engine, rank)],
    }
}

fn build_query_url(base: &str, query: &str) -> String {
    match Url::parse(base) {
        Ok(mut url) => {
            url.query_pairs_mut().append_pair("q", query);
            url.to_string()
        }
        Err(_) => {
            let encoded = form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>();
            format!("{base}?q={encoded}")
        }
    }
}

fn is_tracking_param(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("utm_") || matches!(name.as_str(), "ref" | "fbclid")
}

fn fallback_canonical_key(raw: &str) -> String {
    raw.split('#')
        .next()
        .unwrap_or(raw)
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

fn better_representative(candidate: &SearchHit, current: &SearchHit) -> bool {
    candidate.rank < current.rank
        || (candidate.rank == current.rank
            && (hit_richness(candidate) > hit_richness(current)
                || (hit_richness(candidate) == hit_richness(current)
                    && (&candidate.url, &candidate.title, candidate.engine)
                        < (&current.url, &current.title, current.engine))))
}

fn hit_richness(hit: &SearchHit) -> usize {
    hit.title.trim().len() + hit.description.trim().len()
}

fn absolutize_result_url(raw: &str, base_url: &str, unwrap_ddg: bool) -> Option<String> {
    let raw = decode_html_entities(raw).trim().to_string();
    if raw.is_empty() {
        return None;
    }
    let mut url = Url::parse(&raw)
        .or_else(|_| Url::parse(base_url)?.join(&raw))
        .ok()?;
    if unwrap_ddg
        && url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("duckduckgo.com")
                || host.eq_ignore_ascii_case("html.duckduckgo.com")
        })
        && url.path().starts_with("/l/")
    {
        if let Some(target) = url
            .query_pairs()
            .find(|(name, _)| name.eq_ignore_ascii_case("uddg"))
            .map(|(_, value)| value.into_owned())
            .and_then(|value| Url::parse(&value).ok())
        {
            url = target;
        }
    }
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    Some(url.to_string())
}

// ===================================================================
// Minimal defensive HTML scanner
// ===================================================================

/// Byte ranges for opening tags named `wanted`, including `<` and `>`.
fn tag_ranges(html: &str, wanted: &str) -> Vec<(usize, usize)> {
    let bytes = html.as_bytes();
    let wanted = wanted.as_bytes();
    let mut ranges = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        let Some(relative) = bytes[at..].iter().position(|byte| *byte == b'<') else {
            break;
        };
        let start = at + relative;
        let mut name_start = start + 1;
        if name_start >= bytes.len() || matches!(bytes[name_start], b'/' | b'!' | b'?' | b'%') {
            at = name_start;
            continue;
        }
        while name_start < bytes.len() && bytes[name_start].is_ascii_whitespace() {
            name_start += 1;
        }
        if !bytes_at_eq_ignore_ascii_case(bytes, name_start, wanted) {
            at = name_start.saturating_add(1);
            continue;
        }
        let after_name = name_start + wanted.len();
        if after_name >= bytes.len()
            || !(bytes[after_name].is_ascii_whitespace()
                || matches!(bytes[after_name], b'>' | b'/'))
        {
            at = after_name;
            continue;
        }
        let Some(end) = find_tag_end(bytes, after_name) else {
            break;
        };
        ranges.push((start, end + 1));
        at = end + 1;
    }
    ranges
}

fn bytes_at_eq_ignore_ascii_case(bytes: &[u8], start: usize, wanted: &[u8]) -> bool {
    bytes
        .get(start..start.saturating_add(wanted.len()))
        .is_some_and(|slice| slice.eq_ignore_ascii_case(wanted))
}

fn find_tag_end(bytes: &[u8], mut at: usize) -> Option<usize> {
    let mut quote = None;
    while at < bytes.len() {
        match (quote, bytes[at]) {
            (Some(open), byte) if byte == open => quote = None,
            (None, b'\'' | b'"') => quote = Some(bytes[at]),
            (None, b'>') => return Some(at),
            _ => {}
        }
        at += 1;
    }
    None
}

fn attr_value<'a>(tag: &'a str, wanted: &str) -> Option<&'a str> {
    let bytes = tag.as_bytes();
    let mut at = 1usize;
    while at < bytes.len() && !bytes[at].is_ascii_whitespace() && !matches!(bytes[at], b'>' | b'/')
    {
        at += 1;
    }

    while at < bytes.len() {
        while at < bytes.len() && (bytes[at].is_ascii_whitespace() || matches!(bytes[at], b'/')) {
            at += 1;
        }
        if at >= bytes.len() || bytes[at] == b'>' {
            break;
        }
        let name_start = at;
        while at < bytes.len()
            && (bytes[at].is_ascii_alphanumeric() || matches!(bytes[at], b'-' | b'_' | b':'))
        {
            at += 1;
        }
        if at == name_start {
            at += 1;
            continue;
        }
        let name = &tag[name_start..at];
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if at >= bytes.len() || bytes[at] != b'=' {
            continue;
        }
        at += 1;
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if at >= bytes.len() {
            break;
        }
        let (value_start, value_end) = if matches!(bytes[at], b'\'' | b'"') {
            let quote = bytes[at];
            at += 1;
            let start = at;
            while at < bytes.len() && bytes[at] != quote {
                at += 1;
            }
            let end = at;
            at = at.saturating_add(1);
            (start, end)
        } else {
            let start = at;
            while at < bytes.len()
                && !bytes[at].is_ascii_whitespace()
                && !matches!(bytes[at], b'>' | b'/')
            {
                at += 1;
            }
            (start, at)
        };
        if name.eq_ignore_ascii_case(wanted) {
            return tag.get(value_start..value_end);
        }
    }
    None
}

fn has_class_token(tag: &str, token: &str) -> bool {
    attr_value(tag, "class")
        .is_some_and(|classes| classes.split_ascii_whitespace().any(|item| item == token))
}

fn has_class_tokens(tag: &str, tokens: &[&str]) -> bool {
    tokens.iter().all(|token| has_class_token(tag, token))
}

fn has_class_substring(tag: &str, needle: &str) -> bool {
    attr_value(tag, "class").is_some_and(|classes| {
        classes
            .split_ascii_whitespace()
            .any(|token| token.contains(needle))
    })
}

fn first_class_start(html: &str, tokens: &[&str]) -> Option<usize> {
    let mut at = 0usize;
    while at < html.len() {
        let relative = html[at..].find('<')?;
        let start = at + relative;
        let end = find_tag_end(html.as_bytes(), start + 1)? + 1;
        let raw = &html[start..end];
        if !raw.starts_with("</") && has_class_tokens(raw, tokens) {
            return Some(start);
        }
        at = end;
    }
    None
}

fn first_class_substring_start(html: &str, needle: &str) -> Option<usize> {
    let mut at = 0usize;
    while at < html.len() {
        let relative = html[at..].find('<')?;
        let start = at + relative;
        let end = find_tag_end(html.as_bytes(), start + 1)? + 1;
        let raw = &html[start..end];
        if !raw.starts_with("</") && has_class_substring(raw, needle) {
            return Some(start);
        }
        at = end;
    }
    None
}

fn first_attr_start(html: &str, attribute: &str, value: &str) -> Option<usize> {
    let mut at = 0usize;
    while at < html.len() {
        let relative = html[at..].find('<')?;
        let start = at + relative;
        let end = find_tag_end(html.as_bytes(), start + 1)? + 1;
        let raw = &html[start..end];
        if !raw.starts_with("</") && attr_value(raw, attribute).is_some_and(|found| found == value)
        {
            return Some(start);
        }
        at = end;
    }
    None
}

// Only reached from `Mojeek::parse`, which is test-only in the bin build (Mojeek
// is not a default engine); allowed so the failure-path fixture keeps compiling.
#[allow(dead_code)]
fn first_class_start_for_tag(html: &str, tag: &str, tokens: &[&str]) -> Option<usize> {
    tag_ranges(html, tag)
        .into_iter()
        .find(|(start, end)| has_class_tokens(&html[*start..*end], tokens))
        .map(|(start, _)| start)
}

fn element_inner<'a>(html: &'a str, open_end: usize, tag: &str) -> Option<&'a str> {
    let tail = html.get(open_end..)?;
    let close = format!("</{tag}");
    let relative = find_ascii_case_insensitive(tail, &close)?;
    tail.get(..relative)
}

fn first_text_for_tag(html: &str, tag: &str) -> Option<String> {
    let (_, end) = tag_ranges(html, tag).into_iter().next()?;
    let inner = element_inner(html, end, tag)?;
    let text = clean_text(inner);
    (!text.is_empty()).then_some(text)
}

fn first_text_by_classes(html: &str, tokens: &[&str]) -> Option<String> {
    let start = first_class_start(html, tokens)?;
    text_at_tag_start(html, start)
}

fn first_text_by_class_substring(html: &str, needle: &str) -> Option<String> {
    let start = first_class_substring_start(html, needle)?;
    text_at_tag_start(html, start)
}

fn first_text_by_attr(html: &str, attribute: &str, value: &str) -> Option<String> {
    let start = first_attr_start(html, attribute, value)?;
    text_at_tag_start(html, start)
}

fn text_at_tag_start(html: &str, start: usize) -> Option<String> {
    let end = find_tag_end(html.as_bytes(), start + 1)? + 1;
    let tag = tag_name(&html[start..end])?;
    let inner = element_inner(html, end, tag)?;
    let text = clean_text(inner);
    (!text.is_empty()).then_some(text)
}

fn first_href_by_classes(html: &str, tokens: &[&str]) -> Option<String> {
    for (start, end) in tag_ranges(html, "a") {
        let raw_tag = &html[start..end];
        if !has_class_tokens(raw_tag, tokens) {
            continue;
        }
        let Some(href) = attr_value(raw_tag, "href") else {
            continue;
        };
        if !href.trim().is_empty() {
            return Some(decode_html_entities(href));
        }
    }
    None
}

fn first_href_by_class_substring(html: &str, needle: &str) -> Option<String> {
    for (start, end) in tag_ranges(html, "a") {
        let raw_tag = &html[start..end];
        if !has_class_substring(raw_tag, needle) {
            continue;
        }
        let Some(href) = attr_value(raw_tag, "href") else {
            continue;
        };
        if !href.trim().is_empty() {
            return Some(decode_html_entities(href));
        }
    }
    None
}

fn first_anchor_in_tag(html: &str, tag: &str) -> Option<(String, String)> {
    let (_, open_end) = tag_ranges(html, tag).into_iter().next()?;
    let inner = element_inner(html, open_end, tag)?;
    first_anchor(inner, None)
}

fn first_anchor(html: &str, class: Option<&str>) -> Option<(String, String)> {
    for (start, end) in tag_ranges(html, "a") {
        let raw_tag = &html[start..end];
        if class.is_some_and(|token| !has_class_token(raw_tag, token)) {
            continue;
        }
        let Some(href) = attr_value(raw_tag, "href") else {
            continue;
        };
        let Some(inner) = element_inner(html, end, "a") else {
            continue;
        };
        let title = clean_text(inner);
        if title.is_empty() || href.trim().is_empty() {
            continue;
        }
        return Some((decode_html_entities(href), title));
    }
    None
}

fn tag_name(tag: &str) -> Option<&str> {
    let bytes = tag.as_bytes();
    let mut start = 1usize;
    while start < bytes.len() && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    let mut end = start;
    while end < bytes.len() && bytes[end].is_ascii_alphanumeric() {
        end += 1;
    }
    (end > start).then(|| &tag[start..end])
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

fn clean_text(fragment: &str) -> String {
    let bytes = fragment.as_bytes();
    let mut plain = String::with_capacity(fragment.len());
    let mut at = 0usize;
    while at < bytes.len() {
        if bytes[at] == b'<' {
            match find_tag_end(bytes, at + 1) {
                Some(end) => {
                    at = end + 1;
                    continue;
                }
                None => break,
            }
        }
        let Some(ch) = fragment[at..].chars().next() else {
            break;
        };
        plain.push(ch);
        at += ch.len_utf8();
    }
    decode_html_entities(&plain)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn decode_html_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let entity_start = amp + 1;
        let Some(relative_end) = rest[entity_start..].find(';') else {
            out.push_str(&rest[amp..]);
            return out;
        };
        let entity_end = entity_start + relative_end;
        if relative_end > 16 {
            out.push('&');
            rest = &rest[entity_start..];
            continue;
        }
        let entity = &rest[entity_start..entity_end];
        match decode_entity(entity) {
            Some(ch) => out.push(ch),
            None => out.push_str(&rest[amp..=entity_end]),
        }
        rest = &rest[entity_end + 1..];
    }
    out.push_str(rest);
    out
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" | "#39" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            let value = entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|digits| u32::from_str_radix(digits, 16).ok())
                .or_else(|| {
                    entity
                        .strip_prefix('#')
                        .and_then(|digits| digits.parse::<u32>().ok())
                })?;
            char::from_u32(value)
        }
    }
}

// ===================================================================
// Offline tests
// ===================================================================

// ===================================================================
// REST surface — POST /v1/search
// ===================================================================
//
// The handler lives here (not in a separate file) so the whole search
// feature — core + surface — reads as one unit, mirroring `map.rs`. The CLI
// and MCP riders reuse the public core plus the two small item-shaping helpers
// below (`base_item` / `merge_scrape_fields`) so all three surfaces emit an
// identical result shape.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use draco_core::{extract_with_pool, session_opts, Config, FormatSet};
use draco_types::Status;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Semaphore;

use super::{error_body, parse_formats, AppState};

/// Overall search deadline (ms) when the caller sends no `timeout` — Firecrawl
/// parity (`/v1/search` defaults to 60000).
const DEFAULT_SEARCH_TIMEOUT_MS: u64 = 60_000;
/// Per-engine SERP fetch budget (ms) applied to the shared session. Independent
/// of, and shorter than, the overall deadline so one slow engine cannot consume
/// the whole request.
const SERP_FETCH_TIMEOUT_MS: u64 = 15_000;
/// Max concurrent result-page scrapes when `scrapeOptions.formats` is requested.
const SCRAPE_FANOUT: usize = 4;

/// `POST /v1/search` request. Firecrawl-shaped; unknown fields are ignored
/// (serde is non-strict here by design — the drop-in-friendliness ground rule).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchRequest {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
    /// Time filter (`qdr:d`, …). Preserved and passed to engines that support
    /// it; best-effort per engine.
    #[serde(default)]
    tbs: Option<String>,
    /// Free-text geo target. Best-effort per engine.
    #[serde(default)]
    location: Option<String>,
    /// Overall search deadline (ms). Defaults to 60000.
    #[serde(default)]
    timeout: Option<u64>,
    /// When present with a non-empty `formats`, each result URL is run through
    /// Draco's scrape ladder and the scrape fields are merged onto the hit.
    #[serde(default)]
    scrape_options: Option<ScrapeOptions>,
    // ---- Draco extensions: posture for the SERP fetches ----
    #[serde(default)]
    proxy: Option<String>,
    #[serde(default)]
    ignore_robots: Option<bool>,
}

/// Subset of Firecrawl `scrapeOptions` Draco honors for per-result scraping.
/// Mirrors the scrape handler's fields; unknown fields ignored.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScrapeOptions {
    #[serde(default)]
    formats: Vec<String>,
    #[serde(default)]
    only_main_content: Option<bool>,
    #[serde(default)]
    wait_for: Option<u64>,
    #[serde(default)]
    capture_window_ms: Option<u64>,
    #[serde(default)]
    tier_max: Option<u8>,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    include_tags: Option<Vec<String>>,
    #[serde(default)]
    exclude_tags: Option<Vec<String>>,
    #[serde(default)]
    headers: Option<HashMap<String, String>>,
    #[serde(default)]
    proxy: Option<String>,
    #[serde(default)]
    ignore_robots: Option<bool>,
}

pub(crate) async fn search_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SearchRequest>,
) -> (StatusCode, Json<Value>) {
    let query = req.query.trim();
    if query.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(error_body("\"query\" must be a non-empty string")),
        );
    }
    let limit = req.limit.unwrap_or(5).clamp(1, 100);

    // Validate scrapeOptions.formats up front so an unknown/unsupported format
    // fails the request before any network work (same 400/422 split as scrape).
    let scrape_formats = match &req.scrape_options {
        Some(opts) if !opts.formats.is_empty() => match parse_formats(&opts.formats) {
            Ok(f) => Some(f),
            Err(rej) => {
                let code = if rej.unsupported {
                    StatusCode::UNPROCESSABLE_ENTITY
                } else {
                    StatusCode::BAD_REQUEST
                };
                return (code, Json(error_body(&rej.message)));
            }
        },
        _ => None,
    };

    let params = SearchParams {
        query: query.to_string(),
        limit,
        tbs: req.tbs.clone(),
        location: req.location.clone(),
    };

    // SERP session: inherit proxy posture, a per-engine HTTP budget, and a fresh
    // operation-scoped cookie jar. robots is NOT respected for the SERP fetches
    // by default: search engines disallow `/search` in robots.txt, and a
    // metasearch fetches result pages like a browser (SearXNG does the same).
    // The caller can force respect via `ignoreRobots: false`.
    let serp_config = Config {
        proxy: req.proxy.clone().or_else(|| state.defaults.proxy.clone()),
        timeout_ms: SERP_FETCH_TIMEOUT_MS,
        respect_robots: req.ignore_robots == Some(false),
        force_render: false,
        ..state.defaults.clone()
    };
    let session = session_opts(&serp_config);

    let overall = Duration::from_millis(req.timeout.unwrap_or(DEFAULT_SEARCH_TIMEOUT_MS));
    let engines = default_engines();
    let fanout = search_all_with_session(&params, &engines, DEFAULT_PER_ENGINE_TIMEOUT, &session);
    let (hits, outcomes) = match tokio::time::timeout(overall, fanout).await {
        Ok(pair) => pair,
        Err(_) => {
            return (
                StatusCode::REQUEST_TIMEOUT,
                Json(error_body("search timed out before any engine returned")),
            );
        }
    };

    // Total engine failure → upstream error. Partial failure always returns the
    // surviving engines' consensus (the whole point of the fan-out).
    if !outcomes
        .iter()
        .any(|o| matches!(o.status, EngineStatus::Ok(_)))
    {
        let mut body = error_body("all search engines failed");
        body["draco"] = json!({ "engines": outcomes_json(&outcomes) });
        return (StatusCode::BAD_GATEWAY, Json(body));
    }

    let merged = consensus(hits, limit);

    let data: Vec<Value> = match (&scrape_formats, &req.scrape_options) {
        (Some(formats), Some(opts)) => scrape_results(&merged, *formats, opts, &state).await,
        _ => merged.iter().map(|h| Value::Object(base_item(h))).collect(),
    };

    let body = json!({
        "success": true,
        "data": data,
        "draco": { "engines": outcomes_json(&outcomes) },
    });
    (StatusCode::OK, Json(body))
}

/// Build a `Config` for scraping a single result URL from `scrapeOptions`,
/// seeded by the daemon defaults.
fn scrape_config(opts: &ScrapeOptions, formats: FormatSet, defaults: &Config) -> Config {
    Config {
        formats,
        only_main_content: opts.only_main_content.unwrap_or(defaults.only_main_content),
        include_tags: opts.include_tags.clone().unwrap_or_default(),
        exclude_tags: opts.exclude_tags.clone().unwrap_or_default(),
        headers: opts
            .headers
            .clone()
            .map(|m| m.into_iter().collect())
            .unwrap_or_default(),
        proxy: opts.proxy.clone().or_else(|| defaults.proxy.clone()),
        timeout_ms: opts.timeout.unwrap_or(defaults.timeout_ms),
        tier_max: opts.tier_max.unwrap_or(defaults.tier_max),
        capture_window_ms: opts
            .capture_window_ms
            .or(opts.wait_for)
            .unwrap_or(defaults.capture_window_ms),
        respect_robots: match opts.ignore_robots {
            Some(ignore) => !ignore,
            None => defaults.respect_robots,
        },
        force_render: false,
        ..defaults.clone()
    }
}

/// Scrape each result URL through the shared ladder, bounded by `SCRAPE_FANOUT`
/// and the daemon's global gate, merging scrape fields onto each hit. Order is
/// preserved; a per-URL scrape failure leaves the base title/description/url.
async fn scrape_results(
    merged: &[SearchHit],
    formats: FormatSet,
    opts: &ScrapeOptions,
    state: &Arc<AppState>,
) -> Vec<Value> {
    let base = scrape_config(opts, formats, &state.defaults);
    let sem = Arc::new(Semaphore::new(SCRAPE_FANOUT));
    let mut tasks = tokio::task::JoinSet::new();
    for (idx, hit) in merged.iter().enumerate() {
        let mut item = base_item(hit);
        let url = hit.url.clone();
        let config = base.clone();
        let state = state.clone();
        let sem = sem.clone();
        tasks.spawn(async move {
            // Bound search-scrape concurrency locally, and each scrape still
            // counts against the daemon's global gate (acquired only after the
            // local permit, so 4 ≤ gate size can never deadlock).
            let _local = sem.acquire_owned().await.ok();
            let _gate = state.gate.acquire().await.ok();
            let result = extract_with_pool(&url, &config, &state.tier2_pool).await;
            if result.status == Status::Success {
                merge_scrape_fields(&mut item, &result);
            }
            (idx, Value::Object(item))
        });
    }
    let mut slots: Vec<Option<Value>> = vec![None; merged.len()];
    while let Some(joined) = tasks.join_next().await {
        if let Ok((idx, value)) = joined {
            slots[idx] = Some(value);
        }
    }
    // Any panicked slot falls back to its base item so the result count is stable.
    slots
        .into_iter()
        .enumerate()
        .map(|(idx, slot)| slot.unwrap_or_else(|| Value::Object(base_item(&merged[idx]))))
        .collect()
}

/// The flat `{ title, description, url }` result item every surface emits.
pub(crate) fn base_item(hit: &SearchHit) -> serde_json::Map<String, Value> {
    let mut item = serde_json::Map::new();
    item.insert("title".into(), Value::String(hit.title.clone()));
    item.insert("description".into(), Value::String(hit.description.clone()));
    item.insert("url".into(), Value::String(hit.url.clone()));
    item
}

/// Merge a successful scrape's `Document` fields onto a result item, keyed
/// exactly as `to_firecrawl` keys them (markdown/html/rawHtml/links/json/metadata).
pub(crate) fn merge_scrape_fields(
    item: &mut serde_json::Map<String, Value>,
    result: &ExtractionResult,
) {
    if let Some(md) = &result.markdown {
        item.insert("markdown".into(), Value::String(md.clone()));
    }
    if let Some(h) = &result.html {
        item.insert("html".into(), Value::String(h.clone()));
    }
    if let Some(rh) = &result.raw_html {
        item.insert("rawHtml".into(), Value::String(rh.clone()));
    }
    if let Some(links) = &result.links {
        item.insert(
            "links".into(),
            serde_json::to_value(links).unwrap_or(Value::Null),
        );
    }
    if let Some(d) = &result.data {
        item.insert("json".into(), d.clone());
    }
    if let Some(metadata) = &result.metadata {
        item.insert("metadata".into(), metadata.clone());
    }
}

/// Per-engine diagnostics for the `draco` extension block.
pub(crate) fn outcomes_json(outcomes: &[EngineOutcome]) -> Value {
    let rows: Vec<Value> = outcomes
        .iter()
        .map(|o| match &o.status {
            EngineStatus::Ok(n) => json!({ "engine": o.name, "status": "ok", "results": n }),
            EngineStatus::Empty => json!({ "engine": o.name, "status": "empty" }),
            EngineStatus::Timeout => json!({ "engine": o.name, "status": "timeout" }),
            EngineStatus::Http(code) => json!({ "engine": o.name, "status": "http", "code": code }),
            EngineStatus::Error(msg) => {
                json!({ "engine": o.name, "status": "error", "message": msg })
            }
        })
        .collect();
    Value::Array(rows)
}

// `ExtractionResult` is referenced by the helpers above.
use draco_types::ExtractionResult;

#[cfg(test)]
mod tests {
    use super::*;

    const BRAVE_FIXTURE: &str = include_str!("../../tests/fixtures/search/brave.html");
    const BING_FIXTURE: &str = include_str!("../../tests/fixtures/search/bing.html");
    const DDG_FIXTURE: &str = include_str!("../../tests/fixtures/search/ddg.html");
    const MOJEEK_FIXTURE: &str = include_str!("../../tests/fixtures/search/mojeek.html");
    const BAIDU_FIXTURE: &str = include_str!("../../tests/fixtures/search/baidu.html");
    const ZAPMETA_FIXTURE: &str = include_str!("../../tests/fixtures/search/zapmeta.html");

    #[test]
    fn brave_fixture_yields_well_formed_web_hits() {
        let hits = Brave.parse(BRAVE_FIXTURE, "https://search.brave.com/search");
        assert!(hits.len() >= 5, "expected rich fixture, got {}", hits.len());
        assert!(hits.iter().all(|hit| {
            !hit.title.trim().is_empty()
                && Url::parse(&hit.url).is_ok()
                && hit.rank > 0
                && hit.engine == "brave"
        }));
        assert_eq!(
            hits.first().map(|hit| hit.url.as_str()),
            Some("https://www.scrapingbee.com/blog/web-scraping-rust/")
        );
    }

    #[test]
    fn default_engine_set_uses_six_live_adapters_and_excludes_mojeek() {
        let engines = default_engines();
        let names = engines
            .iter()
            .map(|engine| engine.name())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec!["duckduckgo", "bing", "brave", "baidu", "zapmeta", "yandex",]
        );
    }

    #[test]
    fn baidu_fixture_yields_organic_hits_with_proxied_urls() {
        let hits = Baidu.parse(BAIDU_FIXTURE, "https://www.baidu.com/s?wd=test");
        assert_eq!(
            hits.len(),
            9,
            "fixture has nine div.result.c-container organic nodes"
        );
        assert!(hits.iter().all(|hit| {
            !hit.title.trim().is_empty()
                && !hit.description.trim().is_empty()
                && hit.url.starts_with("http://www.baidu.com/link?url=")
                && hit.engine == "baidu"
                && hit.rank > 0
        }));
    }

    #[test]
    fn zapmeta_fixture_yields_all_nine_articles() {
        let hits = ZapMeta.parse(ZAPMETA_FIXTURE, "https://www.zapmeta.com/search?q=test");
        assert_eq!(hits.len(), 9);
        assert!(hits.iter().all(|hit| {
            !hit.title.trim().is_empty()
                && !hit.description.trim().is_empty()
                && Url::parse(&hit.url).is_ok()
                && hit.engine == "zapmeta"
                && hit.rank > 0
        }));
        assert_eq!(
            hits.first().map(|hit| hit.url.as_str()),
            Some("https://music.saconnects.org/star-search-2024-test-pieces/")
        );
    }

    #[test]
    fn yandex_challenge_body_is_empty_without_panicking() {
        let challenge = r#"
            <html><body><form action="/checkcaptcha">
              <h1>Captcha</h1>
            </form></body></html>
        "#;
        assert!(Yandex
            .parse(challenge, "https://yandex.com/showcaptcha")
            .is_empty());
    }

    #[test]
    fn yandex_documented_markup_parses_synthetically() {
        let html = r#"
            <ol>
              <li class="serp-item">
                <h2>
                  <a class="OrganicTitle-Link Link" href="https://example.com/one">
                    Multiline <span>organic title</span>
                  </a>
                </h2>
                <div class="TextContainer">First <b>Yandex</b> snippet.</div>
                <a class="Path"><span class="Path-Item">example.com</span></a>
              </li>
              <li class="serp-item">
                <a class="OrganicTitle-Link" href="/two">Fallback title</a>
                <div class="OrganicText OrganicText-Vanilla">Second snippet.</div>
              </li>
            </ol>
        "#;
        let hits = Yandex.parse(html, "https://yandex.com/search/?text=test");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Multiline organic title");
        assert_eq!(hits[0].description, "First Yandex snippet.");
        assert_eq!(hits[1].url, "https://yandex.com/two");
        assert_eq!(hits[1].description, "Second snippet.");
    }

    #[test]
    #[ignore = "no successful Yandex fixture: datacenter request redirected to captcha"]
    fn yandex_positive_fixture_when_captured() {
        let captured_html = std::env::var("DRACO_YANDEX_SERP_FIXTURE").unwrap_or_default();
        let hits = Yandex.parse(&captured_html, "https://yandex.com/search/?text=test");
        assert!(!hits.is_empty());
    }

    #[test]
    fn bing_fixture_documents_turnstile_variant_as_empty() {
        assert!(!BING_FIXTURE.contains("b_algo"));
        assert!(BING_FIXTURE.contains("turnstile-widget"));
        assert!(Bing
            .parse(BING_FIXTURE, "https://www.bing.com/search")
            .is_empty());
    }

    #[test]
    fn bing_documented_markup_parses_without_dom_dependency() {
        let html = r#"
            <ol id="b_results">
              <li class="b_algo"><h2><a href="https://example.com/a">Alpha</a></h2>
                <div class="b_caption"><p>First <strong>description</strong>.</p></div></li>
              <li class="b_algo"><h2><a href="/relative">Beta</a></h2>
                <div class="b_algoSlug">Second description.</div></li>
            </ol>
        "#;
        let hits = Bing.parse(html, "https://www.bing.com/search?q=rust");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Alpha");
        assert_eq!(hits[0].description, "First description.");
        assert_eq!(hits[1].url, "https://www.bing.com/relative");
        assert_eq!(hits[1].rank, 2);
    }

    #[test]
    fn mojeek_403_fixture_is_empty_without_panicking() {
        assert!(MOJEEK_FIXTURE.contains("403 - Forbidden"));
        assert!(MOJEEK_FIXTURE.contains("automated queries"));
        assert!(Mojeek
            .parse(MOJEEK_FIXTURE, "https://www.mojeek.com/search")
            .is_empty());
    }

    #[test]
    fn mojeek_documented_markup_parses_defensively() {
        let html = r#"
            <ul class="results-standard">
              <li><a class="title" href="https://example.com/a">Alpha</a><p>First result.</p></li>
              <li><h2><a href="/b">Beta</a></h2><p>Second result.</p></li>
            </ul>
        "#;
        let hits = Mojeek.parse(html, "https://www.mojeek.com/search?q=rust");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].description, "First result.");
        assert_eq!(hits[1].url, "https://www.mojeek.com/b");
    }

    #[test]
    #[ignore = "2026-07-10 fixture is a DDG anomaly challenge, not a POST result page"]
    fn duckduckgo_fixture_positive_parse_when_replaced_with_post_capture() {
        let hits = DuckDuckGo.parse(DDG_FIXTURE, "https://html.duckduckgo.com/html/");
        assert!(!hits.is_empty());
    }

    #[test]
    fn duckduckgo_documented_markup_and_redirect_parse() {
        let html = r#"
            <div class="result results_links">
              <h2><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Frust%3Futm_source%3Dddg">Rust &amp; scraping</a></h2>
              <a class="result__snippet">A <b>defensive</b> parser.</a>
            </div>
        "#;
        let hits = DuckDuckGo.parse(html, "https://html.duckduckgo.com/html/");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Rust & scraping");
        assert_eq!(hits[0].description, "A defensive parser.");
        assert!(hits[0].url.starts_with("https://example.com/rust"));
    }

    #[test]
    fn engine_request_shapes_encode_query() {
        let params = SearchParams {
            query: "rust web scraper".to_string(),
            ..SearchParams::default()
        };
        assert_eq!(DuckDuckGo.method(), HttpMethod::Post);
        assert_eq!(
            DuckDuckGo.body(&params).as_deref(),
            Some("q=rust+web+scraper&b=")
        );
        assert!(Bing.build_url(&params).contains("q=rust+web+scraper"));
        assert!(Brave.build_url(&params).contains("q=rust+web+scraper"));
        assert!(Baidu
            .build_url(&params)
            .contains("wd=rust+web+scraper&ie=utf-8"));
        assert!(ZapMeta.build_url(&params).contains("q=rust+web+scraper"));
        assert!(Yandex.build_url(&params).contains("text=rust+web+scraper"));
        assert!(Mojeek.build_url(&params).contains("q=rust+web+scraper"));
    }

    #[test]
    fn canonical_key_collapses_scheme_root_slash_and_tracking() {
        let expected = "example.com";
        assert_eq!(canonical_key("http://Example.com/"), expected);
        assert_eq!(canonical_key("https://example.com"), expected);
        assert_eq!(canonical_key("example.com/?utm_source=x"), expected);
        assert_eq!(
            canonical_key("https://EXAMPLE.com:443/path/?q=rust&utm_medium=cpc&ref=x#part"),
            "example.com/path?q=rust"
        );
        assert_eq!(
            canonical_key("http://example.com:8080/path?fbclid=x&keep=yes"),
            "example.com:8080/path?keep=yes"
        );
    }

    #[test]
    fn consensus_uses_cross_engine_reciprocal_rank_before_limit() {
        let hits = vec![
            hit("solo", 1, "Solo", "", "https://solo.example/"),
            hit("bing", 2, "Shared", "short", "https://shared.example/a"),
            hit(
                "brave",
                3,
                "Shared richer title",
                "A much richer description",
                "http://SHARED.example/a/?utm_source=brave",
            ),
            hit(
                "mojeek",
                4,
                "Shared",
                "third source",
                "https://shared.example/a#fragment",
            ),
            hit("ddg", 4, "Other", "", "https://other.example/"),
        ];

        let all = consensus(hits.clone(), 10);
        assert_eq!(all.len(), 3);
        assert_eq!(canonical_key(&all[0].url), "shared.example/a");
        assert_eq!(all[0].contributors.len(), 3);
        assert_eq!(all[0].rank, 2);
        assert_eq!(all[1].engine, "solo");

        let limited = consensus(hits, 1);
        assert_eq!(limited.len(), 1);
        assert_eq!(canonical_key(&limited[0].url), "shared.example/a");
    }

    #[test]
    fn consensus_prefers_richer_representative_when_best_ranks_tie() {
        let merged = consensus(
            vec![
                hit("bing", 2, "Short", "", "https://example.com/a"),
                hit(
                    "brave",
                    2,
                    "A richer result title",
                    "with useful descriptive context",
                    "http://EXAMPLE.com/a/?utm_source=brave",
                ),
            ],
            10,
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].engine, "brave");
        assert_eq!(merged[0].title, "A richer result title");
    }

    #[test]
    fn consensus_counts_one_best_rank_per_engine() {
        let merged = consensus(
            vec![
                hit("brave", 5, "A", "", "https://example.com/a"),
                hit("brave", 2, "A", "", "https://example.com/a/"),
                hit("bing", 4, "A", "", "https://example.com/a"),
            ],
            10,
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].contributors, vec![("bing", 4), ("brave", 2)]);
        assert_eq!(merged[0].rank, 2);
    }

    #[derive(Debug)]
    struct FakeEngine {
        name: &'static str,
        url: &'static str,
    }

    impl SearchEngine for FakeEngine {
        fn name(&self) -> &'static str {
            self.name
        }

        fn build_url(&self, _params: &SearchParams) -> String {
            self.url.to_string()
        }

        fn parse(&self, html: &str, _base_url: &str) -> Vec<SearchHit> {
            if html == "hit" {
                vec![hit(
                    self.name,
                    1,
                    "Good result",
                    "",
                    "https://good.example/result",
                )]
            } else {
                Vec::new()
            }
        }
    }

    #[derive(Debug)]
    struct FakeFetcher;

    impl EngineFetcher for FakeFetcher {
        fn fetch(self: Arc<Self>, request: EngineRequest, _session: SessionOpts) -> FetchFuture {
            Box::pin(async move {
                if request.url.ends_with("/timeout") {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    return Ok(fake_response(200, "late"));
                }
                if request.url.ends_with("/error") {
                    return Err("synthetic transport error".to_string());
                }
                if request.url.ends_with("/http") {
                    return Ok(fake_response(503, "unavailable"));
                }
                if request.url.ends_with("/good") {
                    return Ok(fake_response(200, "hit"));
                }
                Ok(fake_response(200, "no results"))
            })
        }
    }

    #[tokio::test]
    async fn fanout_keeps_good_results_when_peers_timeout_error_http_or_empty() {
        let engines: Vec<Box<dyn SearchEngine + Send + Sync>> = vec![
            Box::new(FakeEngine {
                name: "good",
                url: "https://fake.test/good",
            }),
            Box::new(FakeEngine {
                name: "timeout",
                url: "https://fake.test/timeout",
            }),
            Box::new(FakeEngine {
                name: "error",
                url: "https://fake.test/error",
            }),
            Box::new(FakeEngine {
                name: "empty",
                url: "https://fake.test/empty",
            }),
            Box::new(FakeEngine {
                name: "http",
                url: "https://fake.test/http",
            }),
        ];
        let params = SearchParams {
            query: "test".to_string(),
            limit: 5,
            ..SearchParams::default()
        };
        let (hits, outcomes) = search_all_with_fetcher(
            &params,
            &engines,
            Duration::from_millis(5),
            &SessionOpts::default(),
            Arc::new(FakeFetcher),
        )
        .await;

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].engine, "good");
        assert_eq!(outcomes.len(), 5);
        assert_eq!(outcome(&outcomes, "good"), Some(&EngineStatus::Ok(1)));
        assert_eq!(outcome(&outcomes, "timeout"), Some(&EngineStatus::Timeout));
        assert!(matches!(
            outcome(&outcomes, "error"),
            Some(EngineStatus::Error(message)) if message.contains("synthetic")
        ));
        assert_eq!(outcome(&outcomes, "empty"), Some(&EngineStatus::Empty));
        assert_eq!(outcome(&outcomes, "http"), Some(&EngineStatus::Http(503)));
    }

    fn hit(
        engine: &'static str,
        rank: usize,
        title: &str,
        description: &str,
        url: &str,
    ) -> SearchHit {
        engine_hit(
            engine,
            rank,
            title.to_string(),
            description.to_string(),
            url.to_string(),
        )
    }

    fn fake_response(status: u16, body: &str) -> EngineResponse {
        EngineResponse {
            status,
            final_url: "https://fake.test/final".to_string(),
            body: body.to_string(),
        }
    }

    fn outcome<'a>(outcomes: &'a [EngineOutcome], name: &str) -> Option<&'a EngineStatus> {
        outcomes
            .iter()
            .find(|outcome| outcome.name == name)
            .map(|outcome| &outcome.status)
    }
}

// Response-shaping helpers added by the REST/CLI/MCP wiring. Pure and
// self-contained (no network, no AppState) — the handler-level 400/422
// validation paths are covered by the router smoke tests under the daemon's
// gate, where the compiler validates the AppState construction.
#[cfg(test)]
mod wiring_tests {
    use super::*;

    fn hit(title: &str, url: &str) -> SearchHit {
        SearchHit {
            title: title.to_string(),
            description: "desc".to_string(),
            url: url.to_string(),
            engine: "brave",
            rank: 1,
            contributors: vec![("brave", 1)],
        }
    }

    #[test]
    fn base_item_is_title_description_url_only() {
        let item = base_item(&hit("T", "https://example.com/"));
        assert_eq!(
            item.len(),
            3,
            "base item carries exactly title/description/url"
        );
        assert_eq!(item.get("title").and_then(|v| v.as_str()), Some("T"));
        assert_eq!(
            item.get("description").and_then(|v| v.as_str()),
            Some("desc")
        );
        assert_eq!(
            item.get("url").and_then(|v| v.as_str()),
            Some("https://example.com/")
        );
        // No scrape fields leak in when scrapeOptions wasn't requested.
        assert!(item.get("markdown").is_none());
        assert!(item.get("metadata").is_none());
    }

    #[test]
    fn outcomes_json_maps_every_engine_status_variant() {
        let outcomes = vec![
            EngineOutcome {
                name: "brave",
                status: EngineStatus::Ok(5),
            },
            EngineOutcome {
                name: "mojeek",
                status: EngineStatus::Http(403),
            },
            EngineOutcome {
                name: "bing",
                status: EngineStatus::Timeout,
            },
            EngineOutcome {
                name: "duckduckgo",
                status: EngineStatus::Error("boom".to_string()),
            },
            EngineOutcome {
                name: "baidu",
                status: EngineStatus::Empty,
            },
        ];
        let rows = outcomes_json(&outcomes);
        let rows = rows.as_array().expect("engines is a JSON array");
        assert_eq!(rows.len(), 5);

        assert_eq!(rows[0]["engine"].as_str(), Some("brave"));
        assert_eq!(rows[0]["status"].as_str(), Some("ok"));
        assert_eq!(rows[0]["results"].as_u64(), Some(5));

        assert_eq!(rows[1]["status"].as_str(), Some("http"));
        assert_eq!(rows[1]["code"].as_u64(), Some(403));

        assert_eq!(rows[2]["status"].as_str(), Some("timeout"));

        assert_eq!(rows[3]["status"].as_str(), Some("error"));
        assert_eq!(rows[3]["message"].as_str(), Some("boom"));

        assert_eq!(rows[4]["status"].as_str(), Some("empty"));
    }
}
