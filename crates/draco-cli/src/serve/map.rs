//! `POST /v1/map` — Firecrawl-compatible site URL discovery.
//!
//! Fast, shallow discovery of a site's URLs from cheap sources, merged in
//! order (unless overridden by a flag below):
//!
//! 1. **`robots.txt` `Sitemap:` directives** at the target's origin — every
//!    `Sitemap:` line (case-insensitive key, one absolute URL per line,
//!    multiple directives allowed) is fetched as its own sitemap source. A
//!    missing/unfetchable `robots.txt`, or one that declares no sitemaps, is
//!    silently treated as "none found" — it never fails the request.
//! 2. **`/sitemap.xml`** at the target's origin — used only when `robots.txt`
//!    named no sitemaps at all (Firecrawl's fallback order). Skipped
//!    entirely (along with source 1) when `ignoreSitemap` is set.
//!
//!    Every sitemap source, however discovered, follows the same rule: a
//!    sitemap *index* is followed one level deep (first
//!    [`MAX_CHILD_SITEMAPS`] children) so large sites still yield real page
//!    URLs. Sitemap failures are non-fatal: many sites have none, and the
//!    on-page pass below still produces links.
//! 3. **On-page links** — the target page itself is fetched and its `href`
//!    attributes harvested and resolved against the page URL. Skipped
//!    entirely when `sitemapOnly` is set (the page is not even fetched).
//!
//! Results are same-host filtered (subdomains included by default; opt out
//! is not offered — set `includeSubdomains: false` to restrict to the exact
//! host), fragment-stripped, order-preserving deduped, optionally filtered by
//! a case-insensitive `search` substring, and truncated to `limit` (capped at
//! [`MAX_LIMIT`]).
//!
//! `ignoreSitemap` and `sitemapOnly` are mutually exclusive (one says "no
//! sitemaps", the other says "only sitemaps") — sending both is a `400`.
//!
//! All fetches go through `draco-net` (the stealth client), inherit the
//! daemon's default session options, and count against the daemon-wide
//! concurrency gate — a map request is potentially several upstream fetches,
//! so it takes a permit like any extraction.
//!
//! The core logic lives in [`map_site`], which takes a plain [`MapOptions`]
//! (no axum types) and returns a [`MapOutcome`] or [`MapError`] — reusable
//! from a future `draco map` CLI command, not just the HTTP handler.
//!
//! Unknown request fields (`useIndex`, …) are accepted and ignored, matching
//! the scrape endpoint's tolerance of stock Firecrawl client payloads.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use draco_core::session_opts;
use draco_net::{fetch_target, SessionOpts};
use serde::Deserialize;
use serde_json::{json, Value};
use url::Url;

use super::{error_body, AppState};

/// How many child sitemaps of a sitemap index to follow (one level deep).
/// Bounded so a pathological index can't turn one map request into hundreds of
/// upstream fetches. Applies per sitemap source (robots-discovered or
/// default), not globally.
const MAX_CHILD_SITEMAPS: usize = 5;

/// Firecrawl's documented default for `limit`.
const DEFAULT_LIMIT: usize = 5_000;

/// Hard ceiling on `limit` — one HTTP request shouldn't be able to demand an
/// unbounded response. Requests above this are clamped, not rejected.
const MAX_LIMIT: usize = 100_000;

/// Firecrawl-shaped map request (camelCase; unknown fields ignored).
/// `pub(crate)` because it appears in `map_handler`'s signature, which the
/// router in `mod.rs` names.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MapRequest {
    url: String,
    #[serde(default)]
    search: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    ignore_sitemap: Option<bool>,
    /// Skip the on-page fetch entirely; return only sitemap-derived links.
    /// Mutually exclusive with `ignoreSitemap`.
    #[serde(default)]
    sitemap_only: Option<bool>,
    #[serde(default)]
    include_subdomains: Option<bool>,
    /// Total per-fetch timeout in ms (Firecrawl field).
    #[serde(default)]
    timeout: Option<u64>,
}

pub(crate) async fn map_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<MapRequest>,
) -> (StatusCode, Json<Value>) {
    // Validate the target up front: it anchors host filtering and relative-link
    // resolution, so a bad URL can't do anything useful downstream.
    let target = match parse_http_url(&req.url) {
        Ok(u) => u,
        Err(msg) => return (StatusCode::BAD_REQUEST, Json(error_body(&msg))),
    };

    let mut session = session_opts(&state.defaults);
    if let Some(t) = req.timeout {
        session.timeout_ms = t;
    }

    let ignore_sitemap = req.ignore_sitemap.unwrap_or(false);
    let sitemap_only = req.sitemap_only.unwrap_or(false);
    if ignore_sitemap && sitemap_only {
        return (
            StatusCode::BAD_REQUEST,
            Json(error_body(
                "\"ignoreSitemap\" and \"sitemapOnly\" are mutually exclusive",
            )),
        );
    }

    let opts = MapOptions {
        target,
        session,
        search: req.search,
        limit: req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
        include_subdomains: req.include_subdomains.unwrap_or(true),
        ignore_sitemap,
        sitemap_only,
    };

    // A map request may perform multiple upstream fetches — take one permit for
    // the whole operation so it weighs like an extraction against
    // `--max-concurrency`.
    let Ok(_permit) = state.gate.acquire().await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(error_body("server is shutting down")),
        );
    };

    match map_site(&opts).await {
        Ok(outcome) => (
            StatusCode::OK,
            Json(json!({ "success": true, "links": outcome.links })),
        ),
        Err(MapError::BadRequest(msg)) => (StatusCode::BAD_REQUEST, Json(error_body(&msg))),
        Err(MapError::Upstream(msg)) => (StatusCode::BAD_GATEWAY, Json(error_body(&msg))),
    }
}

// ===================================================================
// Core mapping logic (axum-free — reusable from a CLI command)
// ===================================================================

/// Everything [`map_site`] needs, resolved from either the HTTP request or a
/// future CLI invocation. Deliberately axum-free: this is the boundary a
/// `draco map` command can call directly without pulling in the web layer.
#[derive(Debug, Clone)]
pub(crate) struct MapOptions {
    /// The validated target URL (see [`parse_http_url`]).
    pub(crate) target: Url,
    /// Session options (proxy, robots posture, timeout, …) for every fetch
    /// this operation makes.
    pub(crate) session: SessionOpts,
    /// Case-insensitive substring filter on the final URL list.
    pub(crate) search: Option<String>,
    /// Max links to return. Callers are expected to have already clamped this
    /// to `[1, MAX_LIMIT]`; `map_site` does not re-validate it.
    pub(crate) limit: usize,
    /// Include subdomains of the target host as "same site". Firecrawl's
    /// default is `true`.
    pub(crate) include_subdomains: bool,
    /// Skip all sitemap sources (robots-discovered and default) — on-page
    /// hrefs only.
    pub(crate) ignore_sitemap: bool,
    /// Return only sitemap-derived links; never fetch the page itself.
    /// Mutually exclusive with `ignore_sitemap` — callers must reject that
    /// combination before calling `map_site` (the HTTP handler does this at
    /// `400`; `map_site` itself trusts the precondition).
    pub(crate) sitemap_only: bool,
}

/// Successful result of [`map_site`].
#[derive(Debug, Default)]
pub(crate) struct MapOutcome {
    pub(crate) links: Vec<String>,
}

/// Failure modes of [`map_site`], kept axum-free so the function is callable
/// from non-HTTP contexts. The HTTP handler maps these to status codes
/// (`BadRequest` → 400, `Upstream` → 502); a CLI caller would just print the
/// message.
#[derive(Debug)]
pub(crate) enum MapError {
    /// The request itself was invalid (currently unused by `map_site` since
    /// the mutual-exclusivity check happens in the handler before options are
    /// built, but kept so a CLI caller performing its own validation has a
    /// natural variant to return).
    #[allow(dead_code)]
    BadRequest(String),
    /// Neither sitemap nor page could produce anything — the target is
    /// unreachable or returned an error status with no sitemap to fall back
    /// on.
    Upstream(String),
}

/// Run the full discovery pipeline (robots.txt sitemaps → default sitemap →
/// on-page links → filter/dedupe/limit) and return the merged link list.
///
/// This is the callable core the axum handler wraps: it does no HTTP request
/// parsing or response encoding, only fetches and pure post-processing.
pub(crate) async fn map_site(opts: &MapOptions) -> Result<MapOutcome, MapError> {
    // ---- Source 1 & 2: sitemaps (non-fatal) --------------------------------
    let mut links: Vec<String> = Vec::new();
    let mut sitemap_fetched = false;
    if !opts.ignore_sitemap {
        let sitemap_urls = discover_sitemap_urls(&opts.target, &opts.session).await;
        for sitemap_url in &sitemap_urls {
            if let Some(sitemap_links) = fetch_sitemap_links(sitemap_url, &opts.session).await {
                sitemap_fetched = true;
                links.extend(sitemap_links);
            }
        }
    }

    // ---- Source 3: the page's own links -------------------------------------
    // Skipped entirely under `sitemapOnly` — not even fetched.
    if !opts.sitemap_only {
        // An HTTP error page (4xx/5xx) counts as "not fetched": harvesting
        // links from an error page would map the error template, not the site.
        let page_result = fetch_target(opts.target.as_str(), &opts.session).await;
        match &page_result {
            Ok(resp) if resp.meta.status < 400 => {
                let html = String::from_utf8_lossy(&resp.body);
                links.extend(extract_hrefs(&html, &opts.target));
            }
            _ if sitemap_fetched => {
                // A sitemap already gave us an inventory; a dead page is fine.
            }
            Ok(resp) => {
                return Err(MapError::Upstream(format!(
                    "target returned HTTP {} (and no sitemap was found): {}",
                    resp.meta.status, opts.target
                )));
            }
            Err(e) => {
                // Neither source produced anything — the target is unreachable.
                return Err(MapError::Upstream(format!(
                    "could not fetch {} (and no sitemap was found): {e:?}",
                    opts.target
                )));
            }
        }
    } else if !sitemap_fetched {
        // sitemapOnly and no sitemap was found anywhere: nothing to return,
        // and (by design) we never fetched the page to compensate.
        return Err(MapError::Upstream(format!(
            "no sitemap was found for {} (sitemapOnly requested, page not fetched)",
            opts.target
        )));
    }

    // ---- Filter / dedupe / search / limit ----------------------------------
    let filtered = finalize_links(
        links,
        &opts.target,
        opts.include_subdomains,
        opts.search.as_deref(),
        opts.limit,
    );

    Ok(MapOutcome { links: filtered })
}

/// Parse and validate an http(s) URL from a request body. Shared with the
/// crawl endpoint (`pub(crate)`), which validates its seed the same way.
pub(crate) fn parse_http_url(raw: &str) -> Result<Url, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("\"url\" must be a non-empty string".into());
    }
    let url = Url::parse(raw).map_err(|e| format!("invalid \"url\": {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "invalid \"url\": unsupported scheme {:?} (http/https only)",
            url.scheme()
        ));
    }
    Ok(url)
}

/// Determine which sitemap URL(s) to fetch: every `Sitemap:` directive named
/// in `{origin}/robots.txt`, or — when `robots.txt` is missing/unfetchable or
/// names none — `{origin}/sitemap.xml` as a single-element fallback.
///
/// `robots.txt` failures (network error, HTTP error status, unparseable body)
/// are swallowed here and treated the same as "named no sitemaps": this
/// function never fails, matching the non-fatal spirit of sitemap discovery
/// in general.
async fn discover_sitemap_urls(target: &Url, opts: &SessionOpts) -> Vec<Url> {
    if let Ok(robots_url) = target.join("/robots.txt") {
        if let Ok(resp) = fetch_target(robots_url.as_str(), opts).await {
            if resp.meta.status < 400 {
                let body = String::from_utf8_lossy(&resp.body);
                let discovered = parse_robots_sitemaps(&body);
                if !discovered.is_empty() {
                    return discovered
                        .into_iter()
                        .filter_map(|s| Url::parse(&s).ok())
                        .collect();
                }
            }
        }
    }
    // Fallback: the conventional default location, exactly as before
    // robots.txt discovery existed.
    match target.join("/sitemap.xml") {
        Ok(u) => vec![u],
        Err(_) => vec![],
    }
}

/// Parse `Sitemap:` directives out of a `robots.txt` body. The directive key
/// is matched case-insensitively (`Sitemap:`, `sitemap:`, `SITEMAP:`, …); the
/// value is the rest of the line, trimmed. Lines that aren't a `Sitemap:`
/// directive (comments, `User-agent:`, `Disallow:`, blank lines, …) are
/// ignored. Multiple directives are all returned, in file order; values are
/// returned as-is (not yet parsed as URLs) so the caller can filter/parse per
/// its own needs.
fn parse_robots_sitemaps(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        // A `#` starts a comment that runs to the end of the line, per the
        // robots.txt spec; strip it before matching the directive.
        let line = match line.find('#') {
            Some(idx) => &line[..idx],
            None => line,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(colon) = line.find(':') else {
            continue;
        };
        let (key, value) = line.split_at(colon);
        if key.trim().eq_ignore_ascii_case("sitemap") {
            let value = value[1..].trim(); // skip the ':'
            if !value.is_empty() {
                out.push(value.to_string());
            }
        }
    }
    out
}

/// Fetch a sitemap URL and return every page URL it declares, following a
/// sitemap index one level deep. `None` when the sitemap can't be fetched or
/// isn't XML-ish — the caller treats that as "no sitemap", not an error.
async fn fetch_sitemap_links(sitemap_url: &Url, opts: &SessionOpts) -> Option<Vec<String>> {
    let resp = fetch_target(sitemap_url.as_str(), opts).await.ok()?;
    if resp.meta.status >= 400 {
        return None;
    }
    let body = String::from_utf8_lossy(&resp.body).into_owned();
    let locs = extract_locs(&body);
    if locs.is_empty() {
        return None;
    }

    // A sitemap index declares child sitemaps, not pages: recurse one level
    // (bounded) and collect the children's page URLs instead.
    if body.contains("<sitemapindex") {
        let mut pages = Vec::new();
        for child in locs.iter().take(MAX_CHILD_SITEMAPS) {
            if let Ok(child_resp) = fetch_target(child, opts).await {
                if child_resp.meta.status < 400 {
                    let child_body = String::from_utf8_lossy(&child_resp.body);
                    pages.extend(extract_locs(&child_body));
                }
            }
        }
        return Some(pages);
    }
    Some(locs)
}

/// Extract `<loc>…</loc>` values from sitemap XML by string scanning. Sitemaps
/// are machine-generated and regular enough that a scanner beats pulling in an
/// XML dependency; entity-unescape the one entity that legitimately appears in
/// URLs (`&amp;`).
fn extract_locs(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<loc>") {
        rest = &rest[start + "<loc>".len()..];
        let Some(end) = rest.find("</loc>") else {
            break;
        };
        let loc = rest[..end].trim().replace("&amp;", "&");
        if !loc.is_empty() {
            out.push(loc);
        }
        rest = &rest[end + "</loc>".len()..];
    }
    out
}

/// Harvest `href` attribute values from HTML (double- or single-quoted,
/// whitespace-tolerant around `=`) and resolve them against `base`. Only
/// http(s) results are kept; `javascript:`, `mailto:`, unparseable, and empty
/// hrefs drop out naturally via `Url::join`'s scheme handling.
fn extract_hrefs(html: &str, base: &Url) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = html.as_bytes();
    let lower = html.to_lowercase();
    let mut at = 0;
    while let Some(pos) = lower[at..].find("href") {
        let mut i = at + pos + "href".len();
        // Skip whitespace, expect '=', skip whitespace.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            at += pos + "href".len();
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let Some(&quote) = bytes.get(i) else { break };
        if quote != b'"' && quote != b'\'' {
            at = i;
            continue;
        }
        i += 1;
        let Some(end) = html[i..].find(quote as char) else {
            break;
        };
        let raw = &html[i..i + end];
        if let Ok(resolved) = base.join(raw) {
            if matches!(resolved.scheme(), "http" | "https") {
                out.push(resolved.to_string());
            }
        }
        at = i + end + 1;
    }
    out
}

/// Same-host filter (+ optional subdomains), fragment strip, order-preserving
/// dedupe, case-insensitive `search` substring, and `limit` truncation.
fn finalize_links(
    links: Vec<String>,
    target: &Url,
    include_subdomains: bool,
    search: Option<&str>,
    limit: usize,
) -> Vec<String> {
    let target_host = target.host_str().unwrap_or_default();
    let search_lower = search.map(str::to_lowercase);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for link in links {
        let Ok(mut url) = Url::parse(&link) else {
            continue;
        };
        let host_ok = match url.host_str() {
            Some(h) if h == target_host => true,
            Some(h) if include_subdomains => h.ends_with(&format!(".{target_host}")),
            _ => false,
        };
        if !host_ok {
            continue;
        }
        url.set_fragment(None);
        let s = url.to_string();
        if let Some(needle) = &search_lower {
            if !s.to_lowercase().contains(needle.as_str()) {
                continue;
            }
        }
        if seen.insert(s.clone()) {
            out.push(s);
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

// ===================================================================
// Tests
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use axum::routing::{get, post};
    use axum::Router;
    use draco_core::Config;
    use tokio::sync::Semaphore;
    use tower::ServiceExt;

    // ---- pure helpers -------------------------------------------------------

    #[test]
    fn locs_parse_and_unescape() {
        let xml = r#"<?xml version="1.0"?>
            <urlset><url><loc> https://a.example/p1 </loc></url>
            <url><loc>https://a.example/p2?x=1&amp;y=2</loc></url></urlset>"#;
        assert_eq!(
            extract_locs(xml),
            vec![
                "https://a.example/p1".to_string(),
                "https://a.example/p2?x=1&y=2".to_string()
            ]
        );
    }

    #[test]
    fn hrefs_resolve_and_filter_schemes() {
        let base = Url::parse("https://a.example/dir/page").unwrap();
        let html = r#"<a href="/abs">x</a> <a href='rel.html'>y</a>
            <a href = "https://other.example/z">z</a>
            <a href="mailto:x@y.z">m</a> <a href="javascript:void(0)">j</a>"#;
        let got = extract_hrefs(html, &base);
        assert_eq!(
            got,
            vec![
                "https://a.example/abs".to_string(),
                "https://a.example/dir/rel.html".to_string(),
                "https://other.example/z".to_string(),
            ]
        );
    }

    #[test]
    fn finalize_same_host_subdomains_search_dedupe_limit() {
        let target = Url::parse("https://a.example/").unwrap();
        let links = vec![
            "https://a.example/one#frag".to_string(),
            "https://a.example/one".to_string(), // dupe after fragment strip
            "https://sub.a.example/two".to_string(),
            "https://other.example/three".to_string(),
            "https://a.example/blog/four".to_string(),
        ];
        // Same-host only: subdomain + foreign host drop out.
        let strict = finalize_links(links.clone(), &target, false, None, 100);
        assert_eq!(
            strict,
            vec!["https://a.example/one", "https://a.example/blog/four"]
        );
        // Subdomains opt-in.
        let subs = finalize_links(links.clone(), &target, true, None, 100);
        assert!(subs.contains(&"https://sub.a.example/two".to_string()));
        assert!(!subs.contains(&"https://other.example/three".to_string()));
        // Case-insensitive search filter.
        let searched = finalize_links(links.clone(), &target, false, Some("BLOG"), 100);
        assert_eq!(searched, vec!["https://a.example/blog/four"]);
        // Limit truncates.
        let limited = finalize_links(links, &target, false, None, 1);
        assert_eq!(limited.len(), 1);
    }

    #[test]
    fn bad_urls_rejected() {
        assert!(parse_http_url("").is_err());
        assert!(parse_http_url("not a url").is_err());
        assert!(parse_http_url("ftp://a.example/x").is_err());
        assert!(parse_http_url("https://a.example/x").is_ok());
    }

    #[test]
    fn robots_sitemap_directives_parsed_mixed_case_with_comments() {
        let body = "\
            # this is a comment\n\
            User-agent: *\n\
            Disallow: /private\n\
            sitemap: https://a.example/sitemap-1.xml\n\
            Sitemap: https://a.example/sitemap-2.xml # inline comment\n\
            SITEMAP:   https://a.example/sitemap-3.xml   \n\
            Not-A-Directive: nope\n\
            \n\
            Sitemap:https://a.example/sitemap-4.xml\n\
        ";
        assert_eq!(
            parse_robots_sitemaps(body),
            vec![
                "https://a.example/sitemap-1.xml".to_string(),
                "https://a.example/sitemap-2.xml".to_string(),
                "https://a.example/sitemap-3.xml".to_string(),
                "https://a.example/sitemap-4.xml".to_string(),
            ]
        );
    }

    #[test]
    fn robots_sitemap_directives_absent_returns_empty() {
        let body = "User-agent: *\nDisallow: /\n# no sitemap here\n";
        assert!(parse_robots_sitemaps(body).is_empty());
    }

    // ---- end-to-end ---------------------------------------------------------

    fn test_state() -> Arc<AppState> {
        Arc::new(AppState {
            defaults: Config {
                tier_max: 0,
                respect_robots: false,
                ..Config::default()
            },
            gate: Semaphore::new(2),
            tier2_pool: draco_core::Tier2Pool::new(1, 100, true, false),
            crawl: Default::default(),
        })
    }

    async fn post_map(state: Arc<AppState>, body: Value) -> (StatusCode, Value) {
        let app = Router::new()
            .route("/v1/map", post(map_handler))
            .with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/map")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// Fixture site with a sitemap and on-page links; /v1/map merges both
    /// (sitemap first), same-host filters, and dedupes. No `robots.txt` route
    /// is registered, so this also exercises the "robots.txt missing → fall
    /// back to default /sitemap.xml" path.
    #[tokio::test]
    async fn map_end_to_end_sitemap_plus_page_links() {
        // Bind first so the sitemap fixture can embed real absolute URLs.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let sitemap = format!(
            "<urlset>\
             <url><loc>http://127.0.0.1:{port}/from-sitemap-1</loc></url>\
             <url><loc>http://127.0.0.1:{port}/from-sitemap-2</loc></url>\
             </urlset>"
        );
        let fixture = Router::new()
            .route(
                "/",
                get(|| async {
                    axum::response::Html(
                        r#"<html><body>
                        <a href="/from-page">page link</a>
                        <a href="/from-sitemap-1">dupe of sitemap</a>
                        <a href="https://elsewhere.example/x">foreign</a>
                        </body></html>"#,
                    )
                }),
            )
            .route(
                "/sitemap.xml",
                get(move || async move { ([("content-type", "application/xml")], sitemap) }),
            );
        tokio::spawn(async move {
            axum::serve(listener, fixture).await.unwrap();
        });

        let state = test_state();
        let (status, body) =
            post_map(state, json!({ "url": format!("http://127.0.0.1:{port}/") })).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);
        let links: Vec<String> = body["links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        // Sitemap entries first (discovery order), then page links.
        assert_eq!(
            links,
            vec![
                format!("http://127.0.0.1:{port}/from-sitemap-1"),
                format!("http://127.0.0.1:{port}/from-sitemap-2"),
                format!("http://127.0.0.1:{port}/from-page"),
            ],
            "sitemap-first merge, dedupe, and same-host filter"
        );
        // Foreign host filtered.
        assert!(!links.iter().any(|l| l.contains("elsewhere.example")));
    }

    /// `robots.txt` names two `Sitemap:` directives (distinct from the default
    /// `/sitemap.xml`, which is deliberately left 404 to prove it's not used);
    /// both are fetched and merged, ahead of on-page links.
    #[tokio::test]
    async fn map_discovers_multiple_sitemaps_from_robots_txt() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let robots = format!(
            "User-agent: *\n\
             Sitemap: http://127.0.0.1:{port}/sitemap-a.xml\n\
             Sitemap: http://127.0.0.1:{port}/sitemap-b.xml\n"
        );
        let sitemap_a =
            format!("<urlset><url><loc>http://127.0.0.1:{port}/from-a</loc></url></urlset>");
        let sitemap_b =
            format!("<urlset><url><loc>http://127.0.0.1:{port}/from-b</loc></url></urlset>");
        let fixture = Router::new()
            .route(
                "/",
                get(|| async {
                    axum::response::Html(r#"<html><body><a href="/from-page">x</a></body></html>"#)
                }),
            )
            .route(
                "/robots.txt",
                get(move || async move { ([("content-type", "text/plain")], robots) }),
            )
            .route(
                "/sitemap-a.xml",
                get(move || async move { ([("content-type", "application/xml")], sitemap_a) }),
            )
            .route(
                "/sitemap-b.xml",
                get(move || async move { ([("content-type", "application/xml")], sitemap_b) }),
            );
        // Deliberately no /sitemap.xml route: if the handler fell back to it
        // despite robots.txt naming sitemaps, that fetch 404s and contributes
        // nothing, which would silently pass this test for the wrong reason —
        // so we additionally assert both robots-named sitemaps' pages appear.
        tokio::spawn(async move {
            axum::serve(listener, fixture).await.unwrap();
        });

        let state = test_state();
        let (status, body) =
            post_map(state, json!({ "url": format!("http://127.0.0.1:{port}/") })).await;
        assert_eq!(status, StatusCode::OK);
        let links: Vec<String> = body["links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(
            links.contains(&format!("http://127.0.0.1:{port}/from-a")),
            "{links:?}"
        );
        assert!(
            links.contains(&format!("http://127.0.0.1:{port}/from-b")),
            "{links:?}"
        );
        assert!(
            links.contains(&format!("http://127.0.0.1:{port}/from-page")),
            "{links:?}"
        );
    }

    /// `ignoreSitemap: true` skips both robots.txt discovery and the default
    /// sitemap — only on-page links come back, even though both sitemap
    /// sources are live and would otherwise contribute.
    #[tokio::test]
    async fn map_ignore_sitemap_returns_page_links_only() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let robots = format!("Sitemap: http://127.0.0.1:{port}/sitemap.xml\n");
        let sitemap =
            format!("<urlset><url><loc>http://127.0.0.1:{port}/from-sitemap</loc></url></urlset>");
        let fixture = Router::new()
            .route(
                "/",
                get(|| async {
                    axum::response::Html(r#"<html><body><a href="/from-page">x</a></body></html>"#)
                }),
            )
            .route(
                "/robots.txt",
                get(move || async move { ([("content-type", "text/plain")], robots) }),
            )
            .route(
                "/sitemap.xml",
                get(move || async move { ([("content-type", "application/xml")], sitemap) }),
            );
        tokio::spawn(async move {
            axum::serve(listener, fixture).await.unwrap();
        });

        let state = test_state();
        let (status, body) = post_map(
            state,
            json!({ "url": format!("http://127.0.0.1:{port}/"), "ignoreSitemap": true }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let links: Vec<String> = body["links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(links, vec![format!("http://127.0.0.1:{port}/from-page")]);
    }

    /// `sitemapOnly: true` returns only sitemap-derived links and never
    /// fetches the page — proven by pointing the page route at a handler that
    /// would panic the fixture server if invoked (a distinct on-page-only
    /// link would appear in the output if the page were fetched, so its
    /// absence plus an explicit hit counter both confirm the skip).
    #[tokio::test]
    async fn map_sitemap_only_skips_page_fetch() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let sitemap =
            format!("<urlset><url><loc>http://127.0.0.1:{port}/from-sitemap</loc></url></urlset>");
        let page_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let page_hits_handler = page_hits.clone();
        let fixture = Router::new()
            .route(
                "/",
                get(move || {
                    let hits = page_hits_handler.clone();
                    async move {
                        hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        axum::response::Html(
                            r#"<html><body><a href="/from-page">x</a></body></html>"#,
                        )
                    }
                }),
            )
            .route(
                "/sitemap.xml",
                get(move || async move { ([("content-type", "application/xml")], sitemap) }),
            );
        tokio::spawn(async move {
            axum::serve(listener, fixture).await.unwrap();
        });

        let state = test_state();
        let (status, body) = post_map(
            state,
            json!({ "url": format!("http://127.0.0.1:{port}/"), "sitemapOnly": true }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let links: Vec<String> = body["links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(links, vec![format!("http://127.0.0.1:{port}/from-sitemap")]);
        assert_eq!(
            page_hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "sitemapOnly must not fetch the page at all"
        );
    }

    /// `sitemapOnly: true` against a site with no discoverable sitemap (no
    /// `robots.txt` route, and `/sitemap.xml` 404s) has no source to draw
    /// from — and, by design, must not fall back to fetching the page. This
    /// is a `502` (matching the module's existing "both sources failed"
    /// convention), not a silent empty success.
    #[tokio::test]
    async fn map_sitemap_only_with_no_sitemap_is_bad_gateway() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let page_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let page_hits_handler = page_hits.clone();
        // No /sitemap.xml route at all (falls through to axum's default 404),
        // no /robots.txt route either.
        let fixture = Router::new().route(
            "/",
            get(move || {
                let hits = page_hits_handler.clone();
                async move {
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    axum::response::Html("<html></html>")
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, fixture).await.unwrap();
        });

        let state = test_state();
        let (status, body) = post_map(
            state,
            json!({ "url": format!("http://127.0.0.1:{port}/"), "sitemapOnly": true }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["success"], false);
        assert_eq!(
            page_hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "sitemapOnly must not fall back to the page even when no sitemap exists"
        );
    }

    /// `ignoreSitemap` and `sitemapOnly` together are a client error, not an
    /// empty/undefined result — surfaced before any fetch happens.
    #[tokio::test]
    async fn map_ignore_sitemap_and_sitemap_only_together_is_bad_request() {
        let state = test_state();
        let (status, body) = post_map(
            state,
            json!({
                "url": "https://a.example/",
                "ignoreSitemap": true,
                "sitemapOnly": true
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["success"], false);
        let msg = body["error"].as_str().unwrap();
        assert!(msg.contains("mutually exclusive"), "{msg}");
    }

    /// A `limit` above [`MAX_LIMIT`] is clamped, not rejected — the request
    /// still succeeds, just capped.
    #[tokio::test]
    async fn map_limit_above_max_is_clamped() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Three distinct pages from the sitemap so we have something to clamp
        // against with a tiny requested limit.
        let sitemap = format!(
            "<urlset>\
             <url><loc>http://127.0.0.1:{port}/p1</loc></url>\
             <url><loc>http://127.0.0.1:{port}/p2</loc></url>\
             <url><loc>http://127.0.0.1:{port}/p3</loc></url>\
             </urlset>"
        );
        let fixture = Router::new()
            .route("/", get(|| async { axum::response::Html("<html></html>") }))
            .route(
                "/sitemap.xml",
                get(move || async move { ([("content-type", "application/xml")], sitemap) }),
            );
        tokio::spawn(async move {
            axum::serve(listener, fixture).await.unwrap();
        });

        let state = test_state();
        // Absurdly high limit must not be rejected — it's clamped to MAX_LIMIT
        // internally, which still comfortably fits these 3 links, proving the
        // request succeeds rather than 400ing on an out-of-range value.
        let (status, body) = post_map(
            state,
            json!({ "url": format!("http://127.0.0.1:{port}/"), "limit": 1_000_000 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let links = body["links"].as_array().unwrap();
        assert_eq!(links.len(), 3);
    }

    /// Direct unit check that the clamp itself lands at `MAX_LIMIT`, not that
    /// it merely doesn't error — calls `finalize_links` with more candidates
    /// than `MAX_LIMIT` would be impractical to fixture, so this checks the
    /// clamp arithmetic the handler performs before building `MapOptions`.
    #[test]
    fn limit_clamped_to_max() {
        assert_eq!(1_000_000usize.clamp(1, MAX_LIMIT), MAX_LIMIT);
        assert_eq!(0usize.clamp(1, MAX_LIMIT), 1);
        assert_eq!(DEFAULT_LIMIT.clamp(1, MAX_LIMIT), DEFAULT_LIMIT);
    }

    /// End-to-end confirmation of the default flip via the actual handler: no
    /// `includeSubdomains` field in the request, yet a subdomain-hosted link
    /// declared by the sitemap survives filtering. The target is addressed as
    /// `localhost` (not the numeric `127.0.0.1`) because the `url` crate
    /// rejects `sub.127.0.0.1` as a malformed IPv4 literal rather than
    /// accepting it as an opaque domain label; `sub.localhost` has no such
    /// problem and, like `sub.127.0.0.1` would if it parsed, is never
    /// actually dialed — `finalize_links` only string-compares it — so this
    /// stays fully deterministic with no real subdomain DNS involved.
    #[tokio::test]
    async fn map_end_to_end_include_subdomains_default_true() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let sitemap = format!(
            "<urlset>\
             <url><loc>http://sub.localhost:{port}/from-sub</loc></url>\
             <url><loc>http://localhost:{port}/from-root</loc></url>\
             </urlset>"
        );
        let fixture = Router::new()
            .route("/", get(|| async { axum::response::Html("<html></html>") }))
            .route(
                "/sitemap.xml",
                get(move || async move { ([("content-type", "application/xml")], sitemap) }),
            );
        tokio::spawn(async move {
            axum::serve(listener, fixture).await.unwrap();
        });

        let state = test_state();
        let (status, body) =
            post_map(state, json!({ "url": format!("http://localhost:{port}/") })).await;
        assert_eq!(status, StatusCode::OK);
        let links: Vec<String> = body["links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(
            links.contains(&format!("http://localhost:{port}/from-root")),
            "{links:?}"
        );
        // The real proof of the default flip: a subdomain link is *included*
        // by default now, where the old default (false) would have dropped it.
        assert!(
            links.contains(&format!("http://sub.localhost:{port}/from-sub")),
            "{links:?}"
        );
    }

    /// Explicit `includeSubdomains: false` still restricts to the exact host
    /// (the opt-out path continues to work after the default flip).
    #[tokio::test]
    async fn map_include_subdomains_false_still_restricts_to_exact_host() {
        let target = Url::parse("https://a.example/").unwrap();
        let links = vec![
            "https://sub.a.example/two".to_string(),
            "https://a.example/one".to_string(),
        ];
        let out = finalize_links(links, &target, false, None, 100);
        assert_eq!(out, vec!["https://a.example/one".to_string()]);
    }
}
