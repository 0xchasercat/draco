//! `POST /v1/crawl`, `GET|DELETE /v1/crawl/{id}` — Firecrawl-compatible async
//! crawl jobs.
//!
//! A crawl is a bounded BFS over same-host links, where every visited page goes
//! through the full extraction ladder ([`draco_core::extract`]) — so crawled
//! SPAs hydrate exactly like single scrapes do. The Firecrawl job model is
//! preserved: `POST` returns a job id immediately, `GET /v1/crawl/{id}` polls
//! status + accumulated per-page data (same shape as `/v1/scrape`'s `data`),
//! `DELETE` cancels.
//!
//! **Link discovery** deliberately harvests from the scraped page's *Markdown*
//! rather than re-fetching HTML: Draco absolutizes links during conversion, so
//! the Markdown already carries full URLs; there is no second fetch per page;
//! and when the render escalation ran, JS-injected links come for free because
//! the Markdown was made from the hydrated DOM.
//!
//! **Frontier admission** semantics (documented contract):
//! - same host as the seed unless `allowExternalLinks`;
//! - fragments stripped; dedupe against every URL ever enqueued;
//! - `depth + 1 <= maxDepth` (seed is depth 0);
//! - stop admitting once `total` reaches `limit`;
//! - `includePaths` / `excludePaths` are regex patterns (Rust `regex` syntax,
//!   unanchored search — not full-match) tested against the URL *pathname*
//!   only (e.g. `"blog/.*"` matches `/blog/post-1`; no leading slash is
//!   needed since the match is a substring search, not an anchor). When
//!   `regexOnFullURL` is true, the same patterns are tested against the full
//!   URL string (scheme, host, path, query) instead. Exclude wins over
//!   include; an empty `includePaths` means "include everything"; the seed
//!   URL is always admitted. Patterns are compiled once when the crawl plan
//!   is built — an invalid pattern in the request is rejected at `POST` time
//!   with `400`, before any worker spawns.
//!
//! Failed pages count toward `completed` but contribute no `data` entry
//! (Firecrawl omits failed pages). A job whose every page failed reports
//! `failed`; a cancelled job stops before its next page and keeps the data it
//! already gathered.
//!
//! Each page acquires the daemon-wide concurrency permit (`state.gate`) for
//! the duration of its extraction, so a crawl shares the same budget as
//! interactive `/v1/scrape` traffic instead of starving it. Pages within one
//! job run sequentially — polite by construction.
//!
//! The registry is in-memory (`JobStore`): jobs do not survive a daemon
//! restart, matching self-hosted Firecrawl's default (no external queue) and
//! keeping the daemon dependency-free.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use draco_core::{extract_with_pool, Config};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use url::Url;

use super::{error_body, parse_formats, to_firecrawl, AppState, PageQuery};

/// Default page budget when the request gives none, and the hard cap a request
/// may ask for — one HTTP call shouldn't be able to schedule an unbounded
/// scrape of the internet.
const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 100;

/// Default BFS depth (seed = 0).
const DEFAULT_MAX_DEPTH: usize = 2;

// ===================================================================
// Requests / handlers
// ===================================================================

/// Firecrawl-shaped crawl request (camelCase; unknown fields ignored).
/// `pub(crate)` because it appears in `start_handler`'s signature.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CrawlRequest {
    url: String,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    max_depth: Option<usize>,
    #[serde(default)]
    include_paths: Option<Vec<String>>,
    #[serde(default)]
    exclude_paths: Option<Vec<String>>,
    /// When true, `includePaths`/`excludePaths` regexes match against the
    /// full URL (scheme, host, path, query) instead of the pathname only.
    #[serde(default, rename = "regexOnFullURL")]
    regex_on_full_url: bool,
    #[serde(default)]
    allow_external_links: Option<bool>,
    #[serde(default)]
    scrape_options: Option<ScrapeOptions>,
    /// Per-page total timeout in ms.
    #[serde(default)]
    timeout: Option<u64>,
    /// Optional webhook (bare URL string or object) for lifecycle events.
    #[serde(default)]
    webhook: Option<super::webhook::WebhookSpec>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ScrapeOptions {
    #[serde(default)]
    formats: Vec<String>,
    #[serde(default)]
    include_tags: Option<Vec<String>>,
    #[serde(default)]
    exclude_tags: Option<Vec<String>>,
    #[serde(default)]
    headers: Option<std::collections::HashMap<String, String>>,
}

/// Everything the BFS worker needs, resolved at admission time so the worker
/// is a pure function of these bounds.
///
/// `include_paths`/`exclude_paths` are compiled once here (not per candidate
/// URL) — see [`compile_path_patterns`].
struct CrawlPlan {
    seed: Url,
    limit: usize,
    max_depth: usize,
    include_paths: Vec<Regex>,
    exclude_paths: Vec<Regex>,
    regex_on_full_url: bool,
    allow_external: bool,
    config: Config,
}

/// Compile each `includePaths`/`excludePaths` entry as a regex. Firecrawl
/// treats these as regex patterns, not literal substrings, so an invalid
/// pattern is a client error — reported the same way other bad request
/// params are (`400` with a message naming the offender), not a panic or a
/// silently-empty filter.
fn compile_path_patterns(patterns: &[String], field: &str) -> Result<Vec<Regex>, String> {
    patterns
        .iter()
        .map(|p| Regex::new(p).map_err(|e| format!("invalid {field} pattern {p:?}: {e}")))
        .collect()
}

pub(crate) async fn start_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CrawlRequest>,
) -> (StatusCode, Json<Value>) {
    // Validate everything up front so a bad request never spawns a worker.
    let seed = match super::map::parse_http_url(&req.url) {
        Ok(u) => u,
        Err(msg) => return (StatusCode::BAD_REQUEST, Json(error_body(&msg))),
    };
    let formats = req
        .scrape_options
        .as_ref()
        .map(|o| o.formats.clone())
        .unwrap_or_default();
    let parsed_formats = match parse_formats(&formats) {
        Ok(f) => f,
        Err(rej) => {
            let code = if rej.unsupported {
                StatusCode::UNPROCESSABLE_ENTITY
            } else {
                StatusCode::BAD_REQUEST
            };
            return (code, Json(error_body(&rej.message)));
        }
    };
    let include_paths = match compile_path_patterns(
        req.include_paths.as_deref().unwrap_or_default(),
        "includePaths",
    ) {
        Ok(r) => r,
        Err(msg) => return (StatusCode::BAD_REQUEST, Json(error_body(&msg))),
    };
    let exclude_paths = match compile_path_patterns(
        req.exclude_paths.as_deref().unwrap_or_default(),
        "excludePaths",
    ) {
        Ok(r) => r,
        Err(msg) => return (StatusCode::BAD_REQUEST, Json(error_body(&msg))),
    };

    let mut config = state.defaults.clone();
    config.formats = parsed_formats;
    // Per-page content shaping from the nested scrapeOptions (Firecrawl parity).
    if let Some(opts) = req.scrape_options.as_ref() {
        if let Some(inc) = &opts.include_tags {
            config.include_tags = inc.clone();
        }
        if let Some(exc) = &opts.exclude_tags {
            config.exclude_tags = exc.clone();
        }
        if let Some(h) = &opts.headers {
            config.headers = h.clone().into_iter().collect();
        }
    }
    if let Some(t) = req.timeout {
        config.timeout_ms = t;
    }
    let plan = CrawlPlan {
        seed,
        limit: req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
        max_depth: req.max_depth.unwrap_or(DEFAULT_MAX_DEPTH),
        include_paths,
        exclude_paths,
        regex_on_full_url: req.regex_on_full_url,
        allow_external: req.allow_external_links.unwrap_or(false),
        config,
    };

    let id = state.crawl.create_seeded();
    let sink = super::webhook::WebhookSink::new(req.webhook, id.clone(), "crawl");
    tokio::spawn(run_crawl(state.clone(), id.clone(), plan, sink));

    // The status URL is relative: the daemon can't reliably know the external
    // host/scheme clients reach it by (reverse proxies, port maps), and a
    // relative path is unambiguous against whatever base the client used.
    let status_url = format!("/v1/crawl/{id}");
    (
        StatusCode::OK,
        Json(json!({ "success": true, "id": id, "url": status_url })),
    )
}

pub(crate) async fn status_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(page): Query<PageQuery>,
) -> (StatusCode, Json<Value>) {
    let next_base = format!("/v1/crawl/{id}");
    match state
        .crawl
        .snapshot(&id, page.skip.unwrap_or(0), page.limit, &next_base)
    {
        Some(body) => (StatusCode::OK, Json(body)),
        None => (
            StatusCode::NOT_FOUND,
            Json(error_body("crawl job not found")),
        ),
    }
}

/// `GET /v1/crawl/{id}/errors` — per-page failures + robots-blocked URLs.
pub(crate) async fn errors_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    match state.crawl.errors_snapshot(&id) {
        Some(body) => (StatusCode::OK, Json(body)),
        None => (
            StatusCode::NOT_FOUND,
            Json(error_body("crawl job not found")),
        ),
    }
}

pub(crate) async fn cancel_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    if state.crawl.cancel(&id) {
        (
            StatusCode::OK,
            Json(json!({ "success": true, "status": "cancelled" })),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(error_body("crawl job not found")),
        )
    }
}

// ===================================================================
// Worker
// ===================================================================

/// The result of scraping one crawl page: success (with the data payload +
/// Markdown for frontier harvesting), a `robots.txt` skip, or a failure.
enum PageOutcome {
    Ok(Value, Option<String>),
    RobotsBlocked,
    Failed(String),
}

/// The BFS worker: scrape pages breadth-first from the seed, harvesting new
/// frontier URLs from each page's Markdown, until the frontier drains, the
/// page budget is spent, or the job is cancelled.
async fn run_crawl(
    state: Arc<AppState>,
    id: String,
    plan: CrawlPlan,
    sink: super::webhook::WebhookSink,
) {
    use super::webhook::WebhookEvent;
    sink.emit(WebhookEvent::Started, json!([]));

    let mut frontier: VecDeque<(String, usize)> = VecDeque::new();
    let mut admitted: HashSet<String> = HashSet::new();
    let seed = normalized(&plan.seed);
    admitted.insert(seed.clone());
    frontier.push_back((seed, 0));
    let mut total = 1usize;

    while let Some((page_url, depth)) = frontier.pop_front() {
        if state.crawl.is_cancelled(&id) {
            return; // Sticky cancelled status; keep gathered data.
        }

        // One page = one daemon-wide permit, held only for the extraction so a
        // long crawl can't monopolize the gate between pages.
        let outcome = {
            let Ok(_permit) = state.gate.acquire().await else {
                break; // Gate closed: daemon shutting down.
            };
            let result = extract_with_pool(&page_url, &plan.config, &state.tier2_pool).await;
            if super::is_robots_blocked(&result) {
                PageOutcome::RobotsBlocked
            } else {
                let (code, mut body) = to_firecrawl(&result);
                if code == StatusCode::OK {
                    PageOutcome::Ok(body["data"].take(), result.markdown)
                } else {
                    PageOutcome::Failed(
                        body["error"]
                            .as_str()
                            .unwrap_or("extraction failed")
                            .to_string(),
                    )
                }
            }
        };

        match outcome {
            PageOutcome::Ok(data, markdown) => {
                sink.emit(WebhookEvent::Page, json!([data.clone()]));
                state.crawl.record_page(&id, Some(data));
                // Frontier growth from this page's Markdown links (children
                // sit at depth + 1, which must stay within max_depth).
                if depth < plan.max_depth {
                    let mut fresh = 0usize;
                    for link in markdown_links(markdown.as_deref().unwrap_or_default()) {
                        if total >= plan.limit {
                            break;
                        }
                        if let Some(url) = admit(&link, &plan) {
                            if admitted.insert(url.clone()) {
                                frontier.push_back((url, depth + 1));
                                total += 1;
                                fresh += 1;
                            }
                        }
                    }
                    if fresh > 0 {
                        state.crawl.add_admitted(&id, fresh);
                    }
                }
            }
            PageOutcome::RobotsBlocked => {
                state.crawl.record_robots_blocked(&id, &page_url);
                state.crawl.record_page(&id, None);
            }
            PageOutcome::Failed(msg) => {
                state.crawl.record_error(&id, &page_url, &msg);
                state.crawl.record_page(&id, None);
            }
        }
    }
    // Terminal event: completed / failed. Cancelled (or unknown) fires neither.
    match state.crawl.finish(&id) {
        Some(super::jobs::JobStatus::Completed) => sink.emit(WebhookEvent::Completed, json!([])),
        Some(super::jobs::JobStatus::Failed) => sink.emit(WebhookEvent::Failed, json!([])),
        _ => {}
    }
}

/// Fragment-stripped canonical string form used for dedupe and scraping.
fn normalized(url: &Url) -> String {
    let mut u = url.clone();
    u.set_fragment(None);
    u.to_string()
}

/// Apply the admission contract from the module docs to one candidate link.
/// Returns the normalized URL string when admitted.
fn admit(candidate: &str, plan: &CrawlPlan) -> Option<String> {
    let url = Url::parse(candidate).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    if !plan.allow_external && url.host_str() != plan.seed.host_str() {
        return None;
    }
    // Firecrawl matches includePaths/excludePaths as regexes against the
    // pathname by default, or the full URL when `regexOnFullURL` is set.
    // This is an unanchored search (not a full match), so a pattern like
    // "blog/.*" matches pathname "/blog/post-1" with no leading slash needed.
    let owned;
    let subject: &str = if plan.regex_on_full_url {
        owned = normalized(&url);
        owned.as_str()
    } else {
        url.path()
    };
    if plan.exclude_paths.iter().any(|re| re.is_match(subject)) {
        return None;
    }
    if !plan.include_paths.is_empty() && !plan.include_paths.iter().any(|re| re.is_match(subject)) {
        return None;
    }
    Some(normalized(&url))
}

/// Harvest absolute http(s) link targets from Markdown: inline links
/// `](https://…)` and autolinks `<https://…>`. Draco's converter absolutizes
/// links, so this sees the full URL inventory of the rendered page without a
/// second fetch.
fn markdown_links(md: &str) -> Vec<String> {
    let mut out = Vec::new();
    // Inline links / images: the target sits between "](" and the next ')'.
    let mut rest = md;
    while let Some(pos) = rest.find("](") {
        rest = &rest[pos + 2..];
        let Some(end) = rest.find(')') else { break };
        let target = rest[..end].trim();
        // Markdown permits `](url "title")`; keep only the URL part.
        let target = target.split_whitespace().next().unwrap_or_default();
        if target.starts_with("http://") || target.starts_with("https://") {
            out.push(target.to_string());
        }
        rest = &rest[end + 1..];
    }
    // Autolinks: <https://…>.
    let mut rest = md;
    while let Some(pos) = rest.find("<http") {
        rest = &rest[pos + 1..];
        let Some(end) = rest.find('>') else { break };
        let target = rest[..end].trim();
        if target.starts_with("http://") || target.starts_with("https://") {
            out.push(target.to_string());
        }
        rest = &rest[end + 1..];
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
    use tokio::sync::Semaphore;
    use tower::ServiceExt;

    // ---- pure helpers -------------------------------------------------------

    #[test]
    fn markdown_link_harvesting() {
        let md = "# T\n\nSee [a](https://s.example/a) and ![img](https://s.example/i.png)\n\
                  and [titled](https://s.example/t \"Title\") and <https://s.example/auto>\n\
                  but not [rel](/relative) nor [mail](mailto:x@y.z).";
        assert_eq!(
            markdown_links(md),
            vec![
                "https://s.example/a",
                "https://s.example/i.png",
                "https://s.example/t",
                "https://s.example/auto",
            ]
        );
    }

    fn plan(seed: &str) -> CrawlPlan {
        CrawlPlan {
            seed: Url::parse(seed).unwrap(),
            limit: 10,
            max_depth: 2,
            include_paths: vec![],
            exclude_paths: vec![],
            regex_on_full_url: false,
            allow_external: false,
            config: Config::default(),
        }
    }

    /// Build a `Vec<Regex>` from pattern literals for test plans — mirrors
    /// what `compile_path_patterns` does at request time.
    fn patterns(pats: &[&str]) -> Vec<Regex> {
        pats.iter().map(|p| Regex::new(p).unwrap()).collect()
    }

    #[test]
    fn admission_same_host_and_fragment_strip() {
        let p = plan("https://s.example/");
        assert_eq!(
            admit("https://s.example/x#frag", &p).as_deref(),
            Some("https://s.example/x")
        );
        assert_eq!(admit("https://other.example/x", &p), None);
        assert_eq!(admit("ftp://s.example/x", &p), None);
        let mut open = plan("https://s.example/");
        open.allow_external = true;
        assert!(admit("https://other.example/x", &open).is_some());
    }

    #[test]
    fn admission_include_exclude_paths_are_regex() {
        let mut p = plan("https://s.example/");
        p.include_paths = patterns(&["^/blog"]);
        p.exclude_paths = patterns(&["^/blog/drafts"]);
        assert!(admit("https://s.example/blog/post", &p).is_some());
        assert_eq!(admit("https://s.example/shop/item", &p), None);
        // Exclude wins over include.
        assert_eq!(admit("https://s.example/blog/drafts/wip", &p), None);
    }

    /// Firecrawl patterns are typically written with no leading slash and are
    /// matched unanchored against the pathname — "blog/.*" must match
    /// "/blog/x" (the pattern sits as a substring of the pathname, not a
    /// prefix requiring the leading "/").
    #[test]
    fn include_path_regex_matches_unanchored_without_leading_slash() {
        let mut p = plan("https://s.example/");
        p.include_paths = patterns(&["blog/.*"]);
        assert!(admit("https://s.example/blog/x", &p).is_some());
        // Also matches when the pattern sits deeper in the path — proves the
        // search is unanchored, not just "no leading slash required".
        assert!(admit("https://s.example/en/blog/x", &p).is_some());
        assert_eq!(admit("https://s.example/shop/item", &p), None);
    }

    /// With `regexOnFullURL`, patterns run against the full URL string
    /// (scheme + host + path + query), so a query-string-only pattern can
    /// still admit or reject a candidate — something pathname-only matching
    /// could never do.
    #[test]
    fn regex_on_full_url_matches_query_string() {
        let mut p = plan("https://s.example/");
        p.regex_on_full_url = true;
        p.include_paths = patterns(&[r"[?&]lang=en(&|$)"]);
        assert!(admit("https://s.example/blog/post?lang=en", &p).is_some());
        assert_eq!(admit("https://s.example/blog/post?lang=fr", &p), None);
        // Without regexOnFullURL the same pattern can never match (the
        // pathname carries no query string), so this is a genuine behavior
        // difference, not just a redundant assertion.
        p.regex_on_full_url = false;
        assert_eq!(admit("https://s.example/blog/post?lang=en", &p), None);
    }

    /// Exclude wins over include even under regex semantics and even when
    /// both target the full URL.
    #[test]
    fn exclude_wins_over_include_full_url() {
        let mut p = plan("https://s.example/");
        p.regex_on_full_url = true;
        p.include_paths = patterns(&["s\\.example/blog"]);
        p.exclude_paths = patterns(&["draft=true"]);
        assert!(admit("https://s.example/blog/post", &p).is_some());
        assert_eq!(admit("https://s.example/blog/post?draft=true", &p), None);
    }

    #[test]
    fn compile_path_patterns_rejects_invalid_regex_with_named_pattern() {
        let err = compile_path_patterns(&["(unclosed".to_string()], "includePaths")
            .expect_err("unbalanced group must not compile");
        assert!(err.contains("includePaths"), "err: {err}");
        assert!(err.contains("(unclosed"), "err: {err}");
    }

    // ---- handlers -----------------------------------------------------------

    fn test_state() -> Arc<AppState> {
        Arc::new(AppState {
            defaults: Config {
                force_render: false,
                tier_max: 0,
                respect_robots: false,
                ..Config::default()
            },
            gate: Semaphore::new(2),
            tier2_pool: draco_core::Tier2Pool::new(1, 100, true, false),
            crawl: Default::default(),
            batch: Default::default(),
        })
    }

    fn crawl_router(state: Arc<AppState>) -> Router {
        Router::new()
            .route("/v1/crawl", post(start_handler))
            .route("/v1/crawl/{id}", get(status_handler).delete(cancel_handler))
            .with_state(state)
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = to_bytes(resp.into_body(), 1 << 22).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn bad_format_rejected_before_spawning() {
        // "rawHtml" is a supported format now; use an unrecognized token to
        // exercise the pre-spawn 400 short-circuit.
        let app = crawl_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/crawl")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "url": "https://s.example/",
                            "scrapeOptions": { "formats": ["bogus"] }
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// An invalid `includePaths`/`excludePaths` regex is rejected at `POST`
    /// time with `400`, same as any other bad request param — never a panic,
    /// and never silently spawning a worker with a broken filter.
    #[tokio::test]
    async fn invalid_regex_path_rejected_before_spawning() {
        let app = crawl_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/crawl")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "url": "https://s.example/",
                            "includePaths": ["(unclosed"]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["success"], false);
        let msg = body["error"].as_str().unwrap_or_default();
        assert!(msg.contains("includePaths"), "error: {msg}");
        assert!(msg.contains("(unclosed"), "error: {msg}");
    }

    #[tokio::test]
    async fn unknown_job_is_404() {
        let app = crawl_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/crawl/999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// Full lifecycle against a 3-page fixture site (a → b, c): POST starts the
    /// job, polling GET reaches `completed` with all three pages' data.
    #[tokio::test]
    async fn crawl_end_to_end_three_pages() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let page = |title: &str, body_html: &str| {
            format!(
                "<!doctype html><html><head><title>{title}</title></head><body>\
                 <article><h1>{title}</h1><p>Body of {title} with enough text to \
                 not be considered a thin client-rendered shell by the content \
                 engine. It talks about crawling, frontiers, and budgets at some \
                 length so the extractor keeps it verbatim.</p>{body_html}</article>\
                 </body></html>"
            )
        };
        let a = page(
            "Page A",
            &format!(
                "<p><a href=\"http://127.0.0.1:{port}/b\">to b</a> \
                 <a href=\"http://127.0.0.1:{port}/c\">to c</a></p>"
            ),
        );
        let b = page("Page B", "");
        let c = page("Page C", "");
        let fixture = Router::new()
            .route("/a", get(move || async move { axum::response::Html(a) }))
            .route("/b", get(move || async move { axum::response::Html(b) }))
            .route("/c", get(move || async move { axum::response::Html(c) }));
        tokio::spawn(async move {
            axum::serve(listener, fixture).await.unwrap();
        });

        let state = test_state();
        let app = crawl_router(state);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/crawl")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "url": format!("http://127.0.0.1:{port}/a"),
                            "limit": 3,
                            "maxDepth": 2
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let started = body_json(resp).await;
        assert_eq!(started["success"], true);
        let id = started["id"].as_str().unwrap().to_string();
        assert_eq!(started["url"], format!("/v1/crawl/{id}"));

        // Poll until terminal (bounded).
        let mut last = Value::Null;
        for _ in 0..200 {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/v1/crawl/{id}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            last = body_json(resp).await;
            if last["status"] != "scraping" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(last["status"], "completed", "job: {last}");
        assert_eq!(last["total"], 3);
        assert_eq!(last["completed"], 3);
        let data = last["data"].as_array().unwrap();
        assert_eq!(data.len(), 3, "data: {data:?}");
        let all_md: String = data
            .iter()
            .map(|d| d["markdown"].as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n---\n");
        for expected in ["Page A", "Page B", "Page C"] {
            assert!(all_md.contains(expected), "missing {expected}: {all_md}");
        }
        // Every entry carries Firecrawl-keyed metadata.
        assert!(data.iter().all(|d| d["metadata"]["sourceURL"].is_string()));
    }

    #[tokio::test]
    async fn cancel_marks_job_cancelled() {
        let state = test_state();
        // Create a job directly (no worker) — cancellation is a store-level
        // contract; the worker only ever observes the flag.
        let id = state.crawl.create_seeded();
        let app = crawl_router(state.clone());
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/v1/crawl/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(state.crawl.is_cancelled(&id));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/crawl/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_json(resp).await;
        assert_eq!(body["status"], "cancelled");
    }
}
