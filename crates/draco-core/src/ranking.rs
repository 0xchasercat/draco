//! Intercept **ranking policy** (spec §11).
//!
//! Tier 2 boots an isolate and lets the page's own SPA code fire off `fetch`
//! / `XHR` requests; the jailed child reports *every* one of them
//! rank-agnostically (see `draco-types` module docs — ranking is deliberately
//! kept out of the wire so it can evolve without touching the sandbox). It is
//! `draco-core`'s job to pick the single request most likely to be *the* data
//! endpoint, replay it through [`crate::fetcher::PageFetcher`], and treat its
//! JSON body as the extraction.
//!
//! The policy is a pure, deterministic scoring function over an intercepted
//! request's shape — method, URL, headers, and the [`InterceptVia`] transport.
//! Keeping it pure makes it exhaustively unit-testable on synthetic requests
//! with no network and no live isolate, which is exactly what WS-C needs
//! (Tier 2 itself lands in Slice 4).
//!
//! ## Scoring model
//!
//! A request accumulates points for signals that correlate with "this is a
//! JSON data API the page consumed to render", and loses points for signals
//! that mark it as chrome — a static asset, a page navigation, telemetry. The
//! score is a plain `i32`; only the *relative order* matters, but the weights
//! are chosen so that an obvious API call (`GET /api/... Accept: json`) lands
//! around the low-20s (cf. the `"/api/products (score 23)"` trace example in
//! the wire contract), while assets and beacons go negative and are dropped.
//!
//! Nothing here is Next-specific: Tier 1 already handles the `_next/data`
//! build-id path deterministically, so by the time ranking runs we are on a
//! generic SPA and lean on transport/shape heuristics instead.

use draco_net::HtmlResponse;
use draco_types::{HttpRequestSpec, InterceptVia};

// ---------------------------------------------------------------------------
// Public scoring weights (spec §11). Exposed so Slice 4 (and tests) can reason
// about the policy, and so a future config could tune it without code surgery.
// ---------------------------------------------------------------------------

/// Base score for a request whose transport is `fetch()` — the modern data
/// path, mildly preferred over legacy `XMLHttpRequest`.
pub const SCORE_VIA_FETCH: i32 = 4;
/// Base score for a request that came in over `XMLHttpRequest`.
pub const SCORE_VIA_XHR: i32 = 2;

/// The response advertises JSON (`Accept: application/json` on the request, or
/// a JSON `Content-Type` on the response). The single strongest positive tell.
pub const SCORE_JSON_ACCEPT: i32 = 8;
/// Response body actually parses as JSON. Only knowable post-replay, so it is a
/// *confirmation* bonus used when re-scoring a fetched candidate, not during
/// the initial pick.
pub const SCORE_JSON_BODY: i32 = 6;

/// URL path contains a conventional API segment (`/api/`, `/graphql`, …).
pub const SCORE_API_PATH: i32 = 7;
/// Path looks versioned (`/v1/`, `/v2/`, …) — common for REST data APIs.
pub const SCORE_VERSIONED_PATH: i32 = 3;
/// Host is an `api.`/`graphql.` subdomain, or otherwise off the page origin
/// toward an API host.
pub const SCORE_API_HOST: i32 = 4;
/// Request carries a query string — data reads are usually parameterized.
pub const SCORE_HAS_QUERY: i32 = 2;
/// `X-Requested-With`, `X-CSRF-Token`, GraphQL/JSON-RPC content-type, and
/// similar "programmatic client" header tells.
pub const SCORE_XHR_HEADERS: i32 = 3;

/// The path ends in a static-asset extension (`.js`, `.css`, `.png`, …). Heavy
/// penalty: these are never the data endpoint.
pub const PENALTY_STATIC_ASSET: i32 = -20;
/// The URL matches a known analytics / telemetry / ads beacon. Heavy penalty.
pub const PENALTY_ANALYTICS: i32 = -15;
/// The response is HTML (a sub-document / navigation), not data.
pub const PENALTY_HTML: i32 = -10;
/// Non-idempotent verbs (POST/PUT/PATCH/DELETE) are demoted: they usually
/// *mutate*, and a replay could have side effects. GraphQL POSTs claw most of
/// this back via the API-path / JSON signals.
pub const PENALTY_WRITE_METHOD: i32 = -4;

/// Minimum score for a candidate to be considered a viable data endpoint. A
/// request scoring at or below this (e.g. a penalized asset) is discarded
/// rather than replayed.
pub const MIN_VIABLE_SCORE: i32 = 1;

// ---------------------------------------------------------------------------
// Candidate view
// ---------------------------------------------------------------------------

/// A single intercepted request, in the shape the ranking policy scores.
///
/// This is a thin, owned view rather than the wire [`JailToSupervisor::Intercept`]
/// frame so the policy stays decoupled from IPC framing; Slice 4 builds one of
/// these per intercept before ranking.
///
/// [`JailToSupervisor::Intercept`]: draco_types::JailToSupervisor
#[derive(Debug, Clone)]
pub struct Candidate {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub via: InterceptVia,
}

impl Candidate {
    /// Convenience constructor for the common `GET` case in tests / call sites.
    pub fn get(url: impl Into<String>, via: InterceptVia) -> Self {
        Self {
            method: "GET".to_string(),
            url: url.into(),
            headers: Vec::new(),
            via,
        }
    }

    /// Turn a scored candidate into the [`HttpRequestSpec`] that
    /// [`PageFetcher::replay`](crate::fetcher::PageFetcher::replay) consumes.
    pub fn to_request_spec(&self) -> HttpRequestSpec {
        HttpRequestSpec {
            method: self.method.clone(),
            url: self.url.clone(),
            headers: self.headers.clone(),
            body_b64: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

/// Static-asset extensions that immediately disqualify a candidate.
const ASSET_EXTS: &[&str] = &[
    ".js", ".mjs", ".css", ".map", ".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".ico",
    ".woff", ".woff2", ".ttf", ".otf", ".eot", ".mp4", ".webm", ".avif", ".wasm", ".txt", ".xml",
    ".pdf",
];

/// Substrings that mark a URL as analytics / telemetry / ad tech.
const ANALYTICS_MARKERS: &[&str] = &[
    "google-analytics.com",
    "googletagmanager.com",
    "analytics.google",
    "/collect",
    "/gtm.js",
    "/gtag/",
    "doubleclick.net",
    "facebook.com/tr",
    "connect.facebook.net",
    "segment.io",
    "segment.com/v1",
    "sentry.io",
    "/sentry",
    "bugsnag.com",
    "datadoghq.com",
    "/rum",
    "/beacon",
    "/telemetry",
    "/metrics",
    "/pixel",
    "hotjar.com",
    "mixpanel.com",
    "amplitude.com",
    "newrelic.com",
    "nr-data.net",
];

/// API path segments that signal a data endpoint.
const API_MARKERS: &[&str] = &[
    "/api/", "/api.", "/graphql", "/gql", "/rest/", "/rpc", "/query",
];

/// Lower-cased header lookup, returning the first matching value.
fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    let name = name.to_ascii_lowercase();
    headers
        .iter()
        .find(|(k, _)| k.to_ascii_lowercase() == name)
        .map(|(_, v)| v.as_str())
}

/// Does the URL's *path* (query stripped) end in a known asset extension?
fn path_has_asset_ext(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    ASSET_EXTS.iter().any(|ext| path.ends_with(ext))
}

/// Split a URL string into `(host, path, has_query)`, tolerating both absolute
/// (`https://h/p?q`) and origin-relative (`/p?q`) forms without a real base.
fn dissect(url: &str) -> (Option<String>, String, bool) {
    // Try a real parse first for absolute URLs.
    if let Ok(parsed) = url::Url::parse(url) {
        let host = parsed.host_str().map(|h| h.to_ascii_lowercase());
        let path = parsed.path().to_string();
        return (host, path, parsed.query().is_some());
    }
    // Relative URL: no host; split off the query manually.
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (url, None),
    };
    (None, path.to_string(), query.is_some())
}

/// Score a single intercepted request. Higher is more likely to be *the* data
/// endpoint. See the module docs for the model; the individual weights are the
/// `SCORE_*` / `PENALTY_*` constants.
pub fn score_request(c: &Candidate) -> i32 {
    let mut score = match c.via {
        InterceptVia::Fetch => SCORE_VIA_FETCH,
        InterceptVia::Xhr => SCORE_VIA_XHR,
    };

    let (host, path, has_query) = dissect(&c.url);
    let url_lc = c.url.to_ascii_lowercase();
    let path_lc = path.to_ascii_lowercase();

    // --- Disqualifying signals (still return a (negative) score so callers can
    //     see *why* it lost, rather than filtering silently). ---------------
    if path_has_asset_ext(&path_lc) {
        score += PENALTY_STATIC_ASSET;
    }
    if ANALYTICS_MARKERS.iter().any(|m| url_lc.contains(m)) {
        score += PENALTY_ANALYTICS;
    }

    // --- Method ------------------------------------------------------------
    let method = c.method.to_ascii_uppercase();
    if matches!(method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE") {
        score += PENALTY_WRITE_METHOD;
    }

    // --- Path / host shape -------------------------------------------------
    if API_MARKERS.iter().any(|m| path_lc.contains(m)) {
        score += SCORE_API_PATH;
    }
    if is_versioned_path(&path_lc) {
        score += SCORE_VERSIONED_PATH;
    }
    if let Some(h) = host.as_deref() {
        if h.starts_with("api.") || h.starts_with("graphql.") || h.contains(".api.") {
            score += SCORE_API_HOST;
        }
    }
    if has_query {
        score += SCORE_HAS_QUERY;
    }

    // --- Request header tells ---------------------------------------------
    if let Some(accept) = header(&c.headers, "accept") {
        if accept.to_ascii_lowercase().contains("application/json") {
            score += SCORE_JSON_ACCEPT;
        } else if accept.to_ascii_lowercase().contains("text/html") {
            // The page asked for a document, not data.
            score += PENALTY_HTML;
        }
    }
    if let Some(ct) = header(&c.headers, "content-type") {
        let ct = ct.to_ascii_lowercase();
        if ct.contains("application/json") || ct.contains("application/graphql") {
            score += SCORE_XHR_HEADERS;
        }
    }
    if header(&c.headers, "x-requested-with").is_some()
        || header(&c.headers, "x-csrf-token").is_some()
        || header(&c.headers, "x-xsrf-token").is_some()
    {
        score += SCORE_XHR_HEADERS;
    }

    score
}

/// `/v1/`, `/v12/`, `/api/v2/` … a `v` followed by digits as a full segment.
fn is_versioned_path(path: &str) -> bool {
    path.split('/').any(|seg| {
        seg.len() >= 2 && seg.starts_with('v') && seg[1..].bytes().all(|b| b.is_ascii_digit())
    })
}

/// Re-score a candidate *after* replaying it, folding in what the response
/// revealed (JSON vs HTML body). Used by Slice 4 to confirm a winner before
/// accepting its body as the extraction. Kept here so all scoring weights live
/// in one policy module.
pub fn confirm_score(base: i32, resp: &HtmlResponse) -> i32 {
    let mut score = base;
    let ct = resp
        .meta
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.to_ascii_lowercase())
        .unwrap_or_default();
    if ct.contains("application/json") || ct.contains("+json") {
        score += SCORE_JSON_BODY;
    } else if ct.contains("text/html") {
        score += PENALTY_HTML;
    }
    score
}

/// Rank a batch of intercepts and return the highest-scoring *viable*
/// candidate (score `>` [`MIN_VIABLE_SCORE`]), if any. Ties break toward the
/// earliest intercept (stable), matching capture order.
///
/// This is the entry point Slice 4 calls with the collected Tier 2 intercepts.
pub fn best_candidate(candidates: &[Candidate]) -> Option<(usize, i32)> {
    candidates
        .iter()
        .enumerate()
        .map(|(i, c)| (i, score_request(c)))
        .filter(|(_, s)| *s >= MIN_VIABLE_SCORE)
        // max_by_key keeps the *last* max on ties; reverse the index so the
        // earliest capture wins instead.
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn json_api_get_beats_bare_navigation() {
        let api = Candidate {
            method: "GET".into(),
            url: "https://shop.example.com/api/products?page=1".into(),
            headers: hdr(&[("accept", "application/json")]),
            via: InterceptVia::Fetch,
        };
        let nav = Candidate {
            method: "GET".into(),
            url: "https://shop.example.com/products".into(),
            headers: hdr(&[("accept", "text/html")]),
            via: InterceptVia::Fetch,
        };
        let api_score = score_request(&api);
        let nav_score = score_request(&nav);
        assert!(
            api_score > nav_score,
            "api {api_score} should beat nav {nav_score}"
        );
        // Sanity: a textbook JSON API GET lands in the low-20s (cf. wire example).
        assert!(
            (18..=30).contains(&api_score),
            "api score {api_score} out of expected band"
        );
    }

    #[test]
    fn static_assets_and_analytics_go_negative() {
        let js = Candidate::get(
            "https://cdn.example.com/static/app.abc123.js",
            InterceptVia::Fetch,
        );
        let ga = Candidate::get(
            "https://www.google-analytics.com/collect?v=1",
            InterceptVia::Xhr,
        );
        assert!(score_request(&js) < 0, "asset should be negative");
        assert!(score_request(&ga) < 0, "analytics should be negative");
        // And neither is viable.
        assert!(score_request(&js) < MIN_VIABLE_SCORE);
        assert!(score_request(&ga) < MIN_VIABLE_SCORE);
    }

    #[test]
    fn graphql_post_survives_write_penalty() {
        let gql = Candidate {
            method: "POST".into(),
            url: "https://api.example.com/graphql".into(),
            headers: hdr(&[
                ("content-type", "application/json"),
                ("accept", "application/json"),
            ]),
            via: InterceptVia::Fetch,
        };
        let s = score_request(&gql);
        // API host + api path + json accept + json content-type + fetch, minus a
        // small write penalty — comfortably viable.
        assert!(
            s >= MIN_VIABLE_SCORE,
            "graphql post should stay viable, got {s}"
        );
    }

    #[test]
    fn relative_urls_are_scored_without_a_base() {
        let rel = Candidate {
            method: "GET".into(),
            url: "/api/v2/cart?id=7".into(),
            headers: hdr(&[("accept", "application/json")]),
            via: InterceptVia::Xhr,
        };
        let s = score_request(&rel);
        // /api/ + versioned + query + json accept + xhr, no host bonus.
        assert!(s >= SCORE_API_PATH + SCORE_JSON_ACCEPT, "got {s}");
    }

    #[test]
    fn fetch_edges_out_equivalent_xhr() {
        let f = Candidate::get("https://example.com/api/x", InterceptVia::Fetch);
        let x = Candidate::get("https://example.com/api/x", InterceptVia::Xhr);
        assert!(score_request(&f) > score_request(&x));
        assert_eq!(
            score_request(&f) - score_request(&x),
            SCORE_VIA_FETCH - SCORE_VIA_XHR
        );
    }

    #[test]
    fn best_candidate_picks_top_score_and_drops_junk() {
        let cands = vec![
            Candidate::get("https://cdn.example.com/app.js", InterceptVia::Fetch), // asset
            Candidate {
                method: "GET".into(),
                url: "https://api.example.com/v1/items?q=1".into(),
                headers: hdr(&[("accept", "application/json")]),
                via: InterceptVia::Fetch,
            }, // strong
            Candidate::get("https://example.com/logo.png", InterceptVia::Xhr),     // asset
        ];
        let (idx, score) = best_candidate(&cands).expect("a viable candidate");
        assert_eq!(idx, 1);
        assert!(score >= MIN_VIABLE_SCORE);
    }

    #[test]
    fn best_candidate_none_when_all_junk() {
        let cands = vec![
            Candidate::get("https://cdn.example.com/app.js", InterceptVia::Fetch),
            Candidate::get(
                "https://www.google-analytics.com/collect",
                InterceptVia::Xhr,
            ),
        ];
        assert!(best_candidate(&cands).is_none());
    }

    #[test]
    fn best_candidate_breaks_ties_toward_earliest() {
        // Two identical strong candidates: the earlier index must win.
        let one = Candidate {
            method: "GET".into(),
            url: "https://api.example.com/v1/a?x=1".into(),
            headers: hdr(&[("accept", "application/json")]),
            via: InterceptVia::Fetch,
        };
        let two = one.clone();
        let cands = vec![one, two];
        let (idx, _) = best_candidate(&cands).unwrap();
        assert_eq!(idx, 0, "ties should break toward the earliest capture");
    }

    #[test]
    fn versioned_path_detection() {
        assert!(is_versioned_path("/api/v2/items"));
        assert!(is_versioned_path("/v1/"));
        assert!(is_versioned_path("/v12/x"));
        assert!(!is_versioned_path("/video/vlog"));
        assert!(!is_versioned_path("/vary/"));
        assert!(!is_versioned_path("/products"));
    }
}
