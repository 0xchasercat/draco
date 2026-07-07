//! Intercept **ranking policy** (canonical spec §11).
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
//! request's shape — method, URL, headers, transport — plus the page origin (so
//! a request to the page's own backend can be preferred over a third party).
//! Keeping it pure makes it exhaustively unit-testable on synthetic requests
//! with no network and no live isolate.
//!
//! ## Scoring model — the §11 table (authoritative)
//!
//! A request accumulates points for the signals that mark it as "the page's own
//! JSON data API", and loses points for the signals that mark it as chrome — a
//! static asset or a telemetry beacon:
//!
//! | Signal | Δ |
//! |--------|---|
//! | Same-origin as the page (relative URL, or matching host) | **+10** |
//! | Path looks like a data API (`/api/`, `/graphql`, `/v1/`, `/query`, …) | **+8** |
//! | JSON intent (`Accept: application/json`, or a JSON request content-type) | **+5** |
//! | Known analytics / telemetry / ad beacon | **−100** |
//! | Static-asset path extension (`.js`, `.css`, `.png`, …) | **−50** |
//!
//! The score is a plain `i32`; only the *relative order* matters, but the
//! weights are the canonical ones, so the textbook case — a same-origin JSON API
//! GET — scores exactly **23** (`10 + 8 + 5`), matching the `"/api/products
//! (score 23)"` example in the wire contract. Assets and beacons go sharply
//! negative and are dropped by [`MIN_VIABLE_SCORE`].
//!
//! Nothing here is Next-specific: Tier 1 already handles the `_next/data`
//! build-id path deterministically, so by the time ranking runs we are on a
//! generic SPA and lean on origin/path/shape heuristics instead.
//!
//! ## Deliberately *not* scored (yet)
//!
//! Transport (`fetch` vs `XHR`) and HTTP method are **not** scored. In
//! particular there is no write-method penalty: GraphQL and JSON-RPC issue their
//! *reads* over `POST`, so penalizing `POST` would wrongly demote the single most
//! common POST-based data API. The flip side — never blind-replaying a genuine
//! state-changing mutation — is handled where it belongs, at **replay** time:
//! see [`best_replayable`] and [`is_safe_method`] / [`is_read_style_post`]. It
//! screens the ranked candidates so an unsafe non-read `POST` is skipped in
//! favor of the next eligible one, without perturbing the scoring table.

use draco_types::{HttpRequestSpec, InterceptVia};

// ---------------------------------------------------------------------------
// Public scoring weights — the canonical §11 table. Exposed so callers/tests can
// reason about the policy and a future config could tune it without code surgery.
// ---------------------------------------------------------------------------

/// (+10) The request targets the page's own origin — a relative URL, or an
/// absolute URL whose host matches the page. The strongest "this is the site's
/// own data API, not a third party" signal.
pub const SCORE_SAME_ORIGIN: i32 = 10;

/// (+8) The URL path contains a conventional data-API segment (`/api/`,
/// `/graphql`, `/query`, a versioned `/v1/` segment, …).
pub const SCORE_API_PATH: i32 = 8;

/// (+5) JSON intent: the request sends `Accept: application/json`, or carries a
/// JSON/GraphQL request `Content-Type` (e.g. a GraphQL POST body).
pub const SCORE_JSON: i32 = 5;

/// (−100) The URL matches a known analytics / telemetry / ads beacon. A
/// disqualifying penalty: these are never the data endpoint.
pub const PENALTY_ANALYTICS: i32 = -100;

/// (−50) The path ends in a static-asset extension (`.js`, `.css`, `.png`, …).
/// A disqualifying penalty.
pub const PENALTY_STATIC_ASSET: i32 = -50;

/// Minimum score for a candidate to be considered a viable data endpoint. Set so
/// that a bare same-origin navigation (+10 alone) is rejected, while a
/// same-origin API path (+18), a same-origin JSON read (+15), or even a
/// cross-origin API+JSON call (+13, e.g. an `api.` subdomain) all clear the bar.
pub const MIN_VIABLE_SCORE: i32 = 13;

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
    /// Transport the page used. Recorded for the trace / replay fidelity; not a
    /// scoring signal (see the module docs).
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

/// Is the candidate same-origin with the page? A relative candidate URL is
/// same-origin by definition; an absolute one must share the page's host.
fn is_same_origin(candidate_host: Option<&str>, target_url: Option<&str>) -> bool {
    match candidate_host {
        None => true, // relative URL → same origin as whatever loaded it
        Some(h) => matches!(
            target_url.map(dissect),
            Some((Some(t), _, _)) if t.eq_ignore_ascii_case(h)
        ),
    }
}

/// JSON intent: `Accept: application/json`, or a JSON/GraphQL request body type.
fn has_json_intent(c: &Candidate) -> bool {
    if let Some(accept) = header(&c.headers, "accept") {
        if accept.to_ascii_lowercase().contains("application/json") {
            return true;
        }
    }
    if let Some(ct) = header(&c.headers, "content-type") {
        let ct = ct.to_ascii_lowercase();
        if ct.contains("application/json")
            || ct.contains("application/graphql")
            || ct.contains("+json")
        {
            return true;
        }
    }
    false
}

/// Score a single intercepted request against the §11 table. `target_url` is the
/// page URL (for the same-origin signal); `None` means same-origin cannot be
/// credited to absolute URLs. Higher is more likely to be *the* data endpoint.
pub fn score_request(c: &Candidate, target_url: Option<&str>) -> i32 {
    let (host, path, _has_query) = dissect(&c.url);
    let path_lc = path.to_ascii_lowercase();
    let url_lc = c.url.to_ascii_lowercase();

    let mut score = 0;

    // (+10) Same-origin as the page.
    if is_same_origin(host.as_deref(), target_url) {
        score += SCORE_SAME_ORIGIN;
    }
    // (+8) Conventional data-API path (including versioned REST segments).
    if API_MARKERS.iter().any(|m| path_lc.contains(m)) || is_versioned_path(&path_lc) {
        score += SCORE_API_PATH;
    }
    // (+5) JSON intent.
    if has_json_intent(c) {
        score += SCORE_JSON;
    }
    // (−100) Analytics / telemetry / ad beacon.
    if ANALYTICS_MARKERS.iter().any(|m| url_lc.contains(m)) {
        score += PENALTY_ANALYTICS;
    }
    // (−50) Static asset.
    if path_has_asset_ext(&path_lc) {
        score += PENALTY_STATIC_ASSET;
    }

    score
}

/// `/v1/`, `/v12/`, `/api/v2/` … a `v` followed by digits as a full segment.
fn is_versioned_path(path: &str) -> bool {
    path.split('/').any(|seg| {
        seg.len() >= 2 && seg.starts_with('v') && seg[1..].bytes().all(|b| b.is_ascii_digit())
    })
}

/// Rank a batch of intercepts and return the highest-scoring *viable* candidate
/// (score `>=` [`MIN_VIABLE_SCORE`]), if any. `target_url` is the page URL, used
/// for the same-origin signal. Ties break toward the earliest intercept
/// (stable), matching capture order.
///
/// This is the pure ranking primitive. For **replay**, prefer
/// [`best_replayable`], which additionally screens out unsafe state-changing
/// methods before handing a winner to the network — ranking deliberately does
/// not penalize `POST` (see the module docs), so mutation-safety must be applied
/// at replay time.
pub fn best_candidate(candidates: &[Candidate], target_url: Option<&str>) -> Option<(usize, i32)> {
    candidates
        .iter()
        .enumerate()
        .map(|(i, c)| (i, score_request(c, target_url)))
        .filter(|(_, s)| *s >= MIN_VIABLE_SCORE)
        // max_by keeps the *last* max on ties; reverse the index comparison so
        // the earliest capture wins instead.
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
}

// ---------------------------------------------------------------------------
// Mutation-safety (replay policy)
// ---------------------------------------------------------------------------
//
// Ranking scores a request by how *data-endpoint-like* it looks and never by
// its HTTP method: GraphQL and JSON-RPC issue their reads over POST, so a
// method penalty would demote the single most common POST-based data API. That
// leaves a real hazard — blind-replaying a genuine state-changing mutation
// (e.g. `POST /api/cart/add`) — which we address here, at *replay* selection
// time rather than in the flat scoring table. A candidate is replay-eligible
// only if it is an idempotent/safe method, OR it is a recognizable read issued
// over POST (GraphQL/JSON-RPC), OR the operator has explicitly opted in to
// replaying anything the ranker picks.

/// HTTP methods considered inherently side-effect-free (safe to replay blindly):
/// `GET`, `HEAD`, `OPTIONS`. Case-insensitive.
pub fn is_safe_method(method: &str) -> bool {
    let m = method.trim();
    m.eq_ignore_ascii_case("GET")
        || m.eq_ignore_ascii_case("HEAD")
        || m.eq_ignore_ascii_case("OPTIONS")
}

/// URL-path markers that identify a POST/PUT body as a *read* transport
/// (GraphQL / JSON-RPC / query endpoints) rather than a mutation.
const READ_POST_PATH_MARKERS: &[&str] = &["/graphql", "/gql", "/rpc", "/query"];

/// Is this candidate a **read issued over POST/PUT** — a GraphQL / JSON-RPC
/// style query rather than a state-changing mutation? True when the method is
/// `POST`/`PUT` *and* either the path names a query transport
/// (`/graphql`, `/gql`, `/rpc`, `/query`) or the request carries a JSON/GraphQL
/// content-type (the shape those reads use). Such requests are safe to replay
/// even though they are not GET, because they fetch rather than mutate.
///
/// Note this is intentionally conservative: an unrecognized `POST /api/cart/add`
/// with a JSON body is *not* a read (its path is not a query marker and — see
/// below — a bare JSON content-type alone is not enough to make a mutation look
/// like a read). We only treat a POST as read-style when its *path* is a query
/// transport, or when JSON intent is present on a path that is not obviously a
/// mutation endpoint. The bar is: query-marker path OR JSON/GraphQL body type.
pub fn is_read_style_post(c: &Candidate) -> bool {
    let m = c.method.trim();
    if !(m.eq_ignore_ascii_case("POST") || m.eq_ignore_ascii_case("PUT")) {
        return false;
    }
    let (_host, path, _q) = dissect(&c.url);
    let path_lc = path.to_ascii_lowercase();
    if READ_POST_PATH_MARKERS.iter().any(|m| path_lc.contains(m)) {
        return true;
    }
    // A JSON/GraphQL request content-type is the other hallmark of a
    // GraphQL/JSON-RPC read (they POST a JSON `{query|method|params}` envelope).
    has_json_content_type(c)
}

/// True if the request declares a JSON or GraphQL *request body* content-type —
/// the body shape GraphQL/JSON-RPC reads use. (Distinct from [`has_json_intent`],
/// which also accepts an `Accept:` header — here we look only at what is being
/// *sent*, since a mutation could also `Accept: application/json` back.)
fn has_json_content_type(c: &Candidate) -> bool {
    if let Some(ct) = header(&c.headers, "content-type") {
        let ct = ct.to_ascii_lowercase();
        return ct.contains("application/json")
            || ct.contains("application/graphql")
            || ct.contains("+json");
    }
    false
}

/// Rank a batch of intercepts like [`best_candidate`], but return the
/// highest-scoring *viable* candidate that is also **replay-eligible** under the
/// mutation-safety policy. Eligibility is
/// `is_safe_method(method) || is_read_style_post(c) || allow_unsafe`.
///
/// Candidates are considered in descending score (ties → earliest, matching
/// [`best_candidate`]); the first that is replay-eligible wins. So a top-scoring
/// but unsafe non-read `POST` (e.g. `POST /api/cart/add`) is **skipped** in favor
/// of the next eligible candidate — a lower-scored safe `GET`, say — and if none
/// of the viable candidates are eligible, this returns `None`. With
/// `allow_unsafe = true` the safety screen is disabled and this behaves exactly
/// like [`best_candidate`].
pub fn best_replayable(
    candidates: &[Candidate],
    target_url: Option<&str>,
    allow_unsafe: bool,
) -> Option<(usize, i32)> {
    // Collect viable candidates, then sort by the same order best_candidate uses
    // (score desc, earliest index on ties) and pick the first replay-eligible one.
    let mut viable: Vec<(usize, i32)> = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| (i, score_request(c, target_url)))
        .filter(|(_, s)| *s >= MIN_VIABLE_SCORE)
        .collect();
    // Descending score; on equal score, ascending index (earliest wins).
    viable.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    viable
        .into_iter()
        .find(|(i, _)| is_replay_eligible(&candidates[*i], allow_unsafe))
}

/// Is this candidate allowed to be replayed under the mutation-safety policy?
///
/// A **streaming** endpoint is never replay-eligible regardless of method: an
/// SSE stream or WebSocket has no finite body, so replaying it to capture a
/// sample hangs until the session timeout (stake.com's
/// `/_api/feature-flag/v1/flags/stream` produced a 30 s replay timeout — a 43 s
/// total run — once the `EventSource` stub started surfacing it as an endpoint).
/// It is still reported as a discovered endpoint; it just isn't replayed.
fn is_replay_eligible(c: &Candidate, allow_unsafe: bool) -> bool {
    !is_streaming_endpoint(c)
        && (allow_unsafe || is_safe_method(&c.method) || is_read_style_post(c))
}

/// A long-lived streaming endpoint (Server-Sent Events or WebSocket) that cannot
/// be meaningfully HTTP-replayed for a sample body. Detected from the request
/// itself: an `Accept: text/event-stream` header (SSE — set by real EventSource
/// clients and by the runtime's `EventSource` stub) or a `ws(s)://` URL
/// (WebSocket handshake).
pub fn is_streaming_endpoint(c: &Candidate) -> bool {
    let url = c.url.trim_start();
    if url.len() >= 5 {
        let head = url[..url.len().min(6)].to_ascii_lowercase();
        if head.starts_with("ws://") || head.starts_with("wss://") {
            return true;
        }
    }
    c.headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("accept") && v.to_ascii_lowercase().contains("text/event-stream")
    })
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
    fn streaming_endpoints_are_discovered_but_never_replay_eligible() {
        let page = Some("https://stake.com/");
        // SSE: same-origin(10) + api-path(8) = 18, well above MIN_VIABLE, and a
        // safe GET — yet must NOT be replay-eligible (replaying it hangs).
        let sse = Candidate {
            method: "GET".into(),
            url: "https://stake.com/_api/feature-flag/v1/flags/stream".into(),
            headers: hdr(&[("accept", "text/event-stream")]),
            via: InterceptVia::Fetch,
        };
        assert!(is_streaming_endpoint(&sse));
        assert!(score_request(&sse, page) >= MIN_VIABLE_SCORE);
        assert!(
            best_replayable(&[sse], page, false).is_none(),
            "SSE endpoint must not be selected for replay"
        );

        // WebSocket handshake URL — likewise never replay-eligible.
        let ws = Candidate {
            method: "GET".into(),
            url: "wss://stake.com/socket".into(),
            headers: hdr(&[]),
            via: InterceptVia::Fetch,
        };
        assert!(is_streaming_endpoint(&ws));
        assert!(best_replayable(&[ws], page, false).is_none());

        // A normal same-origin JSON GET is still replay-eligible (guard against
        // over-broad matching).
        let json = Candidate {
            method: "GET".into(),
            url: "https://stake.com/api/data".into(),
            headers: hdr(&[("accept", "application/json")]),
            via: InterceptVia::Fetch,
        };
        assert!(!is_streaming_endpoint(&json));
        assert!(best_replayable(&[json], page, false).is_some());
    }

    #[test]
    fn same_origin_json_api_get_scores_exactly_23() {
        // The canonical wire example: same-origin(10) + api-path(8) + json(5).
        let api = Candidate {
            method: "GET".into(),
            url: "https://shop.example.com/api/products?page=1".into(),
            headers: hdr(&[("accept", "application/json")]),
            via: InterceptVia::Fetch,
        };
        assert_eq!(score_request(&api, Some("https://shop.example.com/")), 23);
    }

    #[test]
    fn json_api_get_beats_bare_navigation() {
        let target = Some("https://shop.example.com/");
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
        assert!(score_request(&api, target) > score_request(&nav, target));
        // Bare same-origin navigation is only worth the origin signal, and is not
        // viable on its own.
        assert_eq!(score_request(&nav, target), SCORE_SAME_ORIGIN);
        assert!(score_request(&nav, target) < MIN_VIABLE_SCORE);
    }

    #[test]
    fn static_assets_and_analytics_go_negative() {
        let target = Some("https://shop.example.com/");
        let js = Candidate::get(
            "https://cdn.example.com/static/app.abc123.js",
            InterceptVia::Fetch,
        );
        let ga = Candidate::get(
            "https://www.google-analytics.com/collect?v=1",
            InterceptVia::Xhr,
        );
        assert!(score_request(&js, target) < 0, "asset should be negative");
        assert!(
            score_request(&ga, target) < 0,
            "analytics should be negative"
        );
        assert!(score_request(&js, target) < MIN_VIABLE_SCORE);
        assert!(score_request(&ga, target) < MIN_VIABLE_SCORE);
    }

    #[test]
    fn graphql_post_is_viable() {
        // GraphQL reads over POST: no write penalty, so a same-origin /graphql
        // with JSON intent is a strong candidate (10 + 8 + 5 = 23).
        let gql = Candidate {
            method: "POST".into(),
            url: "https://shop.example.com/graphql".into(),
            headers: hdr(&[
                ("content-type", "application/json"),
                ("accept", "application/json"),
            ]),
            via: InterceptVia::Fetch,
        };
        let s = score_request(&gql, Some("https://shop.example.com/"));
        assert_eq!(s, 23);
        assert!(s >= MIN_VIABLE_SCORE);
    }

    #[test]
    fn relative_urls_are_same_origin_without_a_base() {
        // A relative URL is same-origin by definition, even with no target.
        let rel = Candidate {
            method: "GET".into(),
            url: "/api/v2/cart?id=7".into(),
            headers: hdr(&[("accept", "application/json")]),
            via: InterceptVia::Xhr,
        };
        // same-origin(10) + api-path(8, /api/ and /v2/) + json(5) = 23.
        assert_eq!(score_request(&rel, None), 23);
    }

    #[test]
    fn cross_origin_api_json_is_viable_but_lower_than_same_origin() {
        // An `api.` subdomain: no same-origin, but api-path(8) + json(5) = 13
        // clears the viability bar.
        let cross = Candidate {
            method: "GET".into(),
            url: "https://api.othersite.com/v1/items".into(),
            headers: hdr(&[("accept", "application/json")]),
            via: InterceptVia::Fetch,
        };
        let s = score_request(&cross, Some("https://shop.example.com/"));
        assert_eq!(s, SCORE_API_PATH + SCORE_JSON);
        assert!(s >= MIN_VIABLE_SCORE);
    }

    #[test]
    fn best_candidate_picks_top_score_and_drops_junk() {
        let target = Some("https://shop.example.com/");
        let cands = vec![
            Candidate::get("https://cdn.example.com/app.js", InterceptVia::Fetch), // asset
            Candidate {
                method: "GET".into(),
                url: "https://shop.example.com/api/v1/items?q=1".into(),
                headers: hdr(&[("accept", "application/json")]),
                via: InterceptVia::Fetch,
            }, // strong (same-origin)
            Candidate::get("https://shop.example.com/logo.png", InterceptVia::Xhr), // asset
        ];
        let (idx, score) = best_candidate(&cands, target).expect("a viable candidate");
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
        assert!(best_candidate(&cands, Some("https://shop.example.com/")).is_none());
    }

    #[test]
    fn best_candidate_breaks_ties_toward_earliest() {
        let target = Some("https://api.example.com/");
        let one = Candidate {
            method: "GET".into(),
            url: "https://api.example.com/api/v1/a?x=1".into(),
            headers: hdr(&[("accept", "application/json")]),
            via: InterceptVia::Fetch,
        };
        let two = one.clone();
        let cands = vec![one, two];
        let (idx, _) = best_candidate(&cands, target).unwrap();
        assert_eq!(idx, 0, "ties should break toward the earliest capture");
    }

    // ---- mutation-safety replay policy ----

    #[test]
    fn is_safe_method_covers_read_verbs_case_insensitively() {
        for m in ["GET", "get", "Head", "HEAD", "options", "OPTIONS"] {
            assert!(is_safe_method(m), "{m} should be safe");
        }
        for m in ["POST", "put", "PATCH", "DELETE", "connect", ""] {
            assert!(!is_safe_method(m), "{m} should NOT be safe");
        }
    }

    #[test]
    fn is_read_style_post_recognizes_graphql_and_jsonrpc() {
        // GraphQL POST by path.
        let gql_path = Candidate {
            method: "POST".into(),
            url: "https://shop.example.com/graphql".into(),
            headers: Vec::new(),
            via: InterceptVia::Fetch,
        };
        assert!(is_read_style_post(&gql_path));

        // JSON-RPC style path.
        let rpc = Candidate {
            method: "POST".into(),
            url: "https://shop.example.com/rpc".into(),
            headers: Vec::new(),
            via: InterceptVia::Fetch,
        };
        assert!(is_read_style_post(&rpc));

        // POST with a JSON body content-type (GraphQL/JSON-RPC envelope) — read.
        let json_body = Candidate {
            method: "POST".into(),
            url: "https://shop.example.com/data".into(),
            headers: hdr(&[("content-type", "application/json")]),
            via: InterceptVia::Fetch,
        };
        assert!(is_read_style_post(&json_body));

        // PUT counts too (also a non-mutating query transport in some APIs).
        let put_gql = Candidate {
            method: "PUT".into(),
            url: "https://shop.example.com/gql".into(),
            headers: Vec::new(),
            via: InterceptVia::Fetch,
        };
        assert!(is_read_style_post(&put_gql));
    }

    #[test]
    fn is_read_style_post_rejects_plain_mutations_and_gets() {
        // A bare state-changing POST with no JSON body / query path is NOT a read.
        let cart_add = Candidate {
            method: "POST".into(),
            url: "https://shop.example.com/api/cart/add".into(),
            headers: hdr(&[("content-type", "application/x-www-form-urlencoded")]),
            via: InterceptVia::Fetch,
        };
        assert!(!is_read_style_post(&cart_add));

        // A GET is never "read-style POST" (it's just safe) — the predicate is
        // specifically about POST/PUT reads.
        let get = Candidate::get("https://shop.example.com/api/products", InterceptVia::Fetch);
        assert!(!is_read_style_post(&get));
    }

    #[test]
    fn best_replayable_allows_same_origin_graphql_post_read() {
        // A same-origin GraphQL POST read scores 23 and is replay-eligible.
        let target = Some("https://shop.example.com/");
        let cands = vec![Candidate {
            method: "POST".into(),
            url: "https://shop.example.com/graphql".into(),
            headers: hdr(&[
                ("content-type", "application/json"),
                ("accept", "application/json"),
            ]),
            via: InterceptVia::Fetch,
        }];
        let (idx, score) = best_replayable(&cands, target, false).expect("eligible GraphQL read");
        assert_eq!(idx, 0);
        assert!(score >= MIN_VIABLE_SCORE);
    }

    #[test]
    fn best_replayable_skips_unsafe_post_and_falls_through_to_safe_get() {
        // The top-scoring intercept is an unsafe mutation POST (same-origin, api
        // path, json body → 23). A lower-scored safe GET (same-origin api path →
        // 18) is present. By default the unsafe POST is skipped and the GET wins.
        let target = Some("https://shop.example.com/");
        // Unsafe mutation POST: same-origin(10) + api-path(8) + json-intent(5 via
        // the *Accept* header, NOT a JSON request body) = 23. It out-scores the
        // GET, yet is not read-style (its path is not a query transport and it
        // sends no JSON/GraphQL request content-type), so it must be skipped.
        let unsafe_post = Candidate {
            method: "POST".into(),
            url: "https://shop.example.com/api/cart/add".into(),
            headers: hdr(&[("accept", "application/json")]),
            via: InterceptVia::Fetch,
        };
        // Safe GET: same-origin(10) + api-path(8, /api/ + /v1/) = 18.
        let safe_get = Candidate::get(
            "https://shop.example.com/api/v1/items?id=1",
            InterceptVia::Fetch,
        );
        // Sanity: the POST really does out-score the GET (so we know we're
        // testing the safety skip, not just ranking order), and is genuinely
        // not read-style.
        assert!(
            score_request(&unsafe_post, target) > score_request(&safe_get, target),
            "test premise: unsafe POST must out-score the safe GET"
        );
        assert!(
            !is_read_style_post(&unsafe_post),
            "test premise: the POST must be a genuine (non-read) mutation"
        );
        let cands = vec![unsafe_post, safe_get];

        // Default (safe) mode: skip the POST, fall through to the GET at idx 1.
        let (idx, _s) = best_replayable(&cands, target, false).expect("falls through to safe GET");
        assert_eq!(idx, 1, "unsafe POST must be skipped for the safe GET");

        // allow_unsafe: the top-scoring POST is now eligible and wins (idx 0),
        // matching best_candidate.
        let (uidx, _us) = best_replayable(&cands, target, true).expect("unsafe allowed");
        assert_eq!(uidx, 0);
        assert_eq!(best_candidate(&cands, target).map(|(i, _)| i), Some(0));
    }

    #[test]
    fn best_replayable_none_when_only_unsafe_post_and_not_allowed() {
        // The single viable candidate is an unsafe mutation POST. Default mode
        // yields None (nothing eligible); allow_unsafe replays it.
        let target = Some("https://shop.example.com/");
        let cands = vec![Candidate {
            method: "POST".into(),
            url: "https://shop.example.com/api/cart/add".into(),
            headers: hdr(&[("content-type", "application/x-www-form-urlencoded")]),
            via: InterceptVia::Fetch,
        }];
        assert!(
            best_replayable(&cands, target, false).is_none(),
            "unsafe-only capture must yield no replayable candidate by default"
        );
        let (idx, _s) = best_replayable(&cands, target, true).expect("allowed under override");
        assert_eq!(idx, 0);
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
