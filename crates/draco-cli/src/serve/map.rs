//! `POST /v1/map` — Firecrawl-compatible site URL discovery.
//!
//! Fast, shallow discovery of a site's URLs from two cheap sources, merged in
//! order:
//!
//! 1. **`/sitemap.xml`** at the target's origin (unless `ignoreSitemap`) — the
//!    site's own declared URL inventory. A sitemap *index* is followed one
//!    level deep (first [`MAX_CHILD_SITEMAPS`] children) so large sites still
//!    yield real page URLs. Sitemap failures are non-fatal: many sites have
//!    none, and the on-page pass below still produces links.
//! 2. **On-page links** — the target page itself is fetched and its `href`
//!    attributes harvested and resolved against the page URL.
//!
//! Results are same-host filtered (subdomains opt-in via `includeSubdomains`),
//! fragment-stripped, order-preserving deduped, optionally filtered by a
//! case-insensitive `search` substring, and truncated to `limit`.
//!
//! Both fetches go through `draco-net` (the stealth client), inherit the
//! daemon's default session options, and count against the daemon-wide
//! concurrency gate — a map request is 2+ upstream fetches, so it takes a
//! permit like any extraction.
//!
//! Unknown request fields (`sitemapOnly`, `useIndex`, …) are accepted and
//! ignored, matching the scrape endpoint's tolerance of stock Firecrawl client
//! payloads.

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
/// upstream fetches.
const MAX_CHILD_SITEMAPS: usize = 5;

/// Firecrawl's documented default for `limit`.
const DEFAULT_LIMIT: usize = 5_000;

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

    let mut opts = session_opts(&state.defaults);
    if let Some(t) = req.timeout {
        opts.timeout_ms = t;
    }

    // A map request performs multiple upstream fetches — take one permit for
    // the whole operation so it weighs like an extraction against
    // `--max-concurrency`.
    let Ok(_permit) = state.gate.acquire().await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(error_body("server is shutting down")),
        );
    };

    // ---- Source 1: sitemap (non-fatal) ------------------------------------
    let mut links: Vec<String> = Vec::new();
    let mut sitemap_fetched = false;
    if !req.ignore_sitemap.unwrap_or(false) {
        if let Some(sitemap_links) = fetch_sitemap_links(&target, &opts).await {
            sitemap_fetched = true;
            links.extend(sitemap_links);
        }
    }

    // ---- Source 2: the page's own links (non-fatal if the sitemap worked) --
    // An HTTP error page (4xx/5xx) counts as "not fetched": harvesting links
    // from an error page would map the error template, not the site.
    let page_result = fetch_target(target.as_str(), &opts).await;
    match &page_result {
        Ok(resp) if resp.meta.status < 400 => {
            let html = String::from_utf8_lossy(&resp.body);
            links.extend(extract_hrefs(&html, &target));
        }
        _ if sitemap_fetched => {
            // The sitemap already gave us an inventory; a dead page is fine.
        }
        Ok(resp) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(error_body(&format!(
                    "target returned HTTP {} (and no sitemap was found): {target}",
                    resp.meta.status
                ))),
            );
        }
        Err(e) => {
            // Neither source produced anything — the target is unreachable.
            return (
                StatusCode::BAD_GATEWAY,
                Json(error_body(&format!(
                    "could not fetch {target} (and no sitemap was found): {e:?}"
                ))),
            );
        }
    }

    // ---- Filter / dedupe / search / limit ----------------------------------
    let include_subdomains = req.include_subdomains.unwrap_or(false);
    let filtered = finalize_links(
        links,
        &target,
        include_subdomains,
        req.search.as_deref(),
        req.limit.unwrap_or(DEFAULT_LIMIT),
    );

    (
        StatusCode::OK,
        Json(json!({ "success": true, "links": filtered })),
    )
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

/// Fetch `{origin}/sitemap.xml` and return every page URL it declares, following
/// a sitemap index one level deep. `None` when the sitemap can't be fetched or
/// isn't XML-ish — the caller treats that as "no sitemap", not an error.
async fn fetch_sitemap_links(target: &Url, opts: &SessionOpts) -> Option<Vec<String>> {
    let sitemap_url = target.join("/sitemap.xml").ok()?;
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

    // ---- end-to-end ---------------------------------------------------------

    /// Fixture site with a sitemap and on-page links; /v1/map merges both
    /// (sitemap first), same-host filters, and dedupes.
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

        let state = Arc::new(AppState {
            defaults: Config {
                tier_max: 0,
                respect_robots: false,
                ..Config::default()
            },
            gate: Semaphore::new(2),
            crawl: Default::default(),
        });
        let app = Router::new()
            .route("/v1/map", post(map_handler))
            .with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/map")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "url": format!("http://127.0.0.1:{port}/") }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
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
}
