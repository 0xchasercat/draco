//! # draco-net
//!
//! Stealth TLS/JA4 HTTP client. Implements the canonical spec §9: a
//! [`wreq`]-backed fetch with a faithful recent-Chrome JA4 / HTTP-2 fingerprint
//! (via [`wreq_util`]'s emulation database), a per-session cookie jar,
//! `--proxy` (http / https / socks5), `--delay` (per-host minimum spacing with
//! jitter), `robots.txt` fetch + per-host cache + honoring, 429/503 backoff that
//! respects `Retry-After`, a capped redirect chain, and connect + total
//! timeouts.
//!
//! ## Connection pool vs. cookie isolation
//!
//! The frozen public surface is two free functions, so a *session* is the scope
//! of a single [`fetch_target`] / [`replay`] call. Two concerns pull in
//! opposite directions here, and are deliberately separated:
//!
//! - **Connection reuse wants sharing.** The [`wreq::Client`] owns the
//!   keep-alive / HTTP-2 connection pool and the (expensive) BoringSSL
//!   connector + emulation profile. Rebuilding it per call — as this module
//!   originally did — throws the pool away every request, so every fetch pays a
//!   fresh TCP + TLS handshake and the profile is recompiled each time. That is
//!   pure waste in a long-lived process (the `draco serve` daemon) and even
//!   across the several fetches of one extraction (page + script subresources +
//!   replay, typically the same host). So the client is now built **once per
//!   proxy** and cached process-wide ([`shared_client`]); `Client` is
//!   `Arc`-backed, so handing out clones shares one pool.
//! - **Cookies want isolation.** A shared client must not let one call's
//!   cookies bleed into an unrelated one. wreq lets a cookie store be attached
//!   *per request* ([`wreq::RequestBuilder::cookie_provider`]), which overrides
//!   the client for that request, so each call gets its own fresh
//!   [`Jar`](wreq::cookie::Jar): cookies set by a redirect or the robots probe
//!   ride along within the call but never leak between calls. The total request
//!   timeout is likewise applied per request, so the shared client need not be
//!   fragmented by timeout.
//!
//! Net effect: one connection pool for the whole process (connection reuse
//! across requests and within an extraction) with strict per-call cookie
//! isolation. Two further pieces of state are *process-wide* because the spec
//! scopes them "per-host" across a run, not per call: the last-request
//! timestamp used for delay spacing, and the parsed robots.txt cache. Both live
//! behind small mutexes and are keyed by host.
//!
//! ## Error mapping
//!
//! Every failure is funnelled to [`DracoError::Network`] with the most specific
//! [`NetKind`] we can determine from the [`wreq::Error`] predicates
//! (`is_timeout`, `is_connect`, `is_proxy_connect`, `is_redirect`, `is_tls`,
//! `is_body`/`is_decode`, …) or from the resolver failing DNS.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use draco_types::{DracoError, HttpRequestSpec, HttpResponseMeta, NetKind};

use base64::Engine as _;
use wreq::cookie::Jar;
use wreq::header::{HeaderMap, HeaderName, HeaderValue, OrigHeaderMap};
use wreq::redirect::Policy;
use wreq::{Client, Proxy, RequestBuilder};
// wreq-util 3.0.0-rc.13 emulation surface: `Profile` = the browser/client
// preset, `Platform` = the OS, and `Emulation` = the config struct (built via a
// typed builder) that implements `wreq::IntoEmulation`.
use wreq_util::{Emulation, Platform, Profile};

// ===================================================================
// Frozen public API (bodies filled in; signatures unchanged).
// ===================================================================

/// In-process HTTP response. Not serialized; the raw body is carried as bytes.
#[derive(Debug, Clone)]
pub struct HtmlResponse {
    pub meta: HttpResponseMeta,
    pub body: Bytes,
}

/// Per-session network options.
#[derive(Debug, Clone)]
pub struct SessionOpts {
    pub proxy: Option<String>,
    pub delay_ms: u64,
    pub respect_robots: bool,
    pub timeout_ms: u64,
    /// Extra request headers applied to every outbound request (Firecrawl's
    /// `headers` — custom UA, cookies, auth, etc.). Ordered; empty by default.
    pub headers: Vec<(String, String)>,
}

impl Default for SessionOpts {
    fn default() -> Self {
        Self {
            proxy: None,
            delay_ms: 0,
            respect_robots: true,
            timeout_ms: 30_000,
            headers: Vec::new(),
        }
    }
}

/// Tier 0 entry: fetch a page with a browser-faithful fingerprint.
pub async fn fetch_target(url: &str, opts: &SessionOpts) -> Result<HtmlResponse, DracoError> {
    let client = shared_client(opts.proxy.as_deref())?;
    // Per-call cookie jar: isolates this call's cookies from every other call
    // sharing the pooled client (see module docs).
    let jar = Arc::new(Jar::default());
    // Robots gate + per-host spacing happen before the real request.
    guard_request(&client, &jar, url, opts).await?;
    // No explicit headers: let the emulation preset supply Chrome's header set
    // (names, values, and order) so the request-header fingerprint is faithful.
    send_with_retry(|| dress(client.get(url), &jar, opts)).await
}

/// Replay a constructed (Tier 1) or intercepted (Tier 2) request with the same
/// pooled client.
pub async fn replay(
    spec: &HttpRequestSpec,
    opts: &SessionOpts,
) -> Result<HtmlResponse, DracoError> {
    let client = shared_client(opts.proxy.as_deref())?;
    let jar = Arc::new(Jar::default());

    let method = parse_method(&spec.method)?;
    let body = decode_body(spec.body_b64.as_deref())?;
    // Preserve the caller's exact header order + original casing: it is
    // fingerprint-relevant for an intercepted request.
    let ordered = build_ordered_headers(&spec.headers)?;

    guard_request(&client, &jar, &spec.url, opts).await?;

    send_with_retry(|| {
        let mut rb = dress(
            client
                .request(method.clone(), &spec.url)
                .headers(ordered.map.clone())
                .orig_headers(ordered.orig.clone()),
            &jar,
            opts,
        );
        if let Some(bytes) = body.clone() {
            rb = rb.body(bytes);
        }
        rb
    })
    .await
}

/// Attach the per-call cookie jar and total request timeout to a request
/// builder. Applied to *every* outbound request (the real fetch, each retry
/// attempt, and the robots probe) so cookie isolation and the timeout hold
/// uniformly now that neither lives on the shared client.
fn dress(rb: RequestBuilder, jar: &Arc<Jar>, opts: &SessionOpts) -> RequestBuilder {
    let mut rb = rb
        .cookie_provider(jar.clone())
        .timeout(Duration::from_millis(opts.timeout_ms.max(1)));
    // Caller-supplied request headers, applied uniformly to the real fetch,
    // each retry, and the robots probe (dress runs on all three).
    for (name, value) in &opts.headers {
        rb = rb.header(name, value);
    }
    rb
}

// ===================================================================
// Tunables (canonical §9). Kept as constants so the policy is auditable.
// ===================================================================

/// Hard cap on the redirect chain. Exceeding it maps to
/// [`NetKind::TooManyRedirects`].
const MAX_REDIRECTS: usize = 10;
/// Connect-phase timeout, derived as a fraction of the total budget but never
/// larger than this ceiling.
const CONNECT_TIMEOUT_CEIL_MS: u64 = 10_000;
/// Bounded retry attempts for retryable statuses (429/503).
const MAX_RETRIES: u32 = 3;
/// Cap on any single honored `Retry-After` delay, so a hostile server cannot
/// park us for minutes.
const RETRY_AFTER_CAP_MS: u64 = 20_000;
/// Base for exponential backoff when no `Retry-After` is present.
const BACKOFF_BASE_MS: u64 = 500;
/// User-agent used for the robots.txt probe token match (see [`Robots`]).
const ROBOTS_UA_TOKEN: &str = "draco";

// ===================================================================
// Client construction
// ===================================================================

/// Process-wide cache of pooled clients, keyed by proxy (empty string = no
/// proxy). One client per proxy is all we need: the emulation profile,
/// redirect policy, and connector are otherwise identical, and the total
/// timeout + cookie jar are applied per request. Sharing the client shares its
/// keep-alive / HTTP-2 connection pool across every call in the process.
fn client_pool() -> &'static Mutex<HashMap<String, Client>> {
    static POOL: OnceLock<Mutex<HashMap<String, Client>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Get the pooled [`Client`] for this proxy, building (and caching) it on first
/// use. The returned clone shares the cached client's connection pool.
fn shared_client(proxy: Option<&str>) -> Result<Client, DracoError> {
    let proxy = proxy.filter(|p| !p.is_empty());
    let key = proxy.unwrap_or("").to_string();

    if let Some(client) = client_pool().lock().unwrap().get(&key) {
        return Ok(client.clone());
    }

    // Cache miss: build outside the lock (BoringSSL/connector setup isn't free),
    // then insert-if-absent so a concurrent builder for the same proxy can't
    // leave two live pools — the first insert wins, later ones reuse it.
    let built = build_pooled_client(proxy)?;
    let mut pool = client_pool().lock().unwrap();
    Ok(pool.entry(key).or_insert(built).clone())
}

/// Build the fingerprinted, pooled [`Client`] for a proxy. Carries everything
/// that is constant across calls (emulation, redirect policy, connect timeout,
/// cookie layer); the per-call jar and total timeout are attached per request
/// by [`dress`], so they are intentionally *absent* here.
fn build_pooled_client(proxy: Option<&str>) -> Result<Client, DracoError> {
    // A faithful recent-Chrome preset: TLS (JA3/JA4), HTTP/2 SETTINGS &
    // pseudo-header order, and the default request-header set/order all come
    // from wreq-util's emulation database. `http2` and `headers` default to
    // `true` on the builder, so the profile drives the full fingerprint.
    let emulation = Emulation::builder()
        .profile(Profile::Chrome137)
        .platform(Platform::Windows)
        .build();

    let mut builder = Client::builder()
        .emulation(emulation)
        // Enable the cookie layer; the *store* is supplied per request (see
        // `dress`) so cookies never leak between calls on this shared client.
        .cookie_store(true)
        .redirect(Policy::limited(MAX_REDIRECTS))
        // Connect-phase fail-fast bound. The real per-request total timeout
        // (applied by `dress`) is what ultimately bounds the whole operation,
        // so a single ceiling here is fine for the shared client.
        .connect_timeout(Duration::from_millis(CONNECT_TIMEOUT_CEIL_MS));

    if let Some(proxy) = proxy {
        // `Proxy::all` covers http+https targets; a `socks5://` URI is accepted
        // here too (the `socks` feature). Parse failures are a config-level
        // proxy error.
        let p = Proxy::all(proxy)
            .map_err(|e| net_err(NetKind::Proxy, format!("invalid proxy: {e}")))?;
        builder = builder.proxy(p);
    }

    builder
        .build()
        .map_err(|e| classify_wreq_error(&e, "client build"))
}

// ===================================================================
// Request execution: robots gate, per-host spacing, retry/backoff.
// ===================================================================

/// Run the pre-flight guards shared by every outbound request: robots.txt
/// (when enabled) then per-host delay spacing. `jar` is the call's cookie jar,
/// used for the robots probe so it shares the call's cookie scope.
async fn guard_request(
    client: &Client,
    jar: &Arc<Jar>,
    url: &str,
    opts: &SessionOpts,
) -> Result<(), DracoError> {
    if opts.respect_robots {
        enforce_robots(client, jar, url, opts).await?;
    }
    apply_host_delay(url, opts.delay_ms).await;
    Ok(())
}

/// Send `make` (a fresh [`RequestBuilder`](wreq::RequestBuilder) each attempt)
/// with bounded retry/backoff on 429/503, honoring `Retry-After`.
async fn send_with_retry<F>(make: F) -> Result<HtmlResponse, DracoError>
where
    F: Fn() -> wreq::RequestBuilder,
{
    let mut attempt: u32 = 0;
    loop {
        let started = Instant::now();
        let resp = make().send().await;

        match resp {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if is_retryable_status(status) && attempt < MAX_RETRIES {
                    let retry_after = resp
                        .headers()
                        .get(wreq::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_owned());
                    let wait = backoff_delay(attempt, retry_after.as_deref());
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(wait)).await;
                    continue;
                }
                return finalize_response(resp, started).await;
            }
            Err(e) => return Err(classify_wreq_error(&e, "request")),
        }
    }
}

/// Consume a [`wreq::Response`] into an [`HtmlResponse`], preserving response
/// header order and the post-redirect final URL.
async fn finalize_response(
    resp: wreq::Response,
    started: Instant,
) -> Result<HtmlResponse, DracoError> {
    let status = resp.status().as_u16();
    let final_url = resp.uri().to_string();
    let headers = collect_headers(resp.headers());
    let elapsed_ms = started.elapsed().as_millis() as u64;

    let body = resp
        .bytes()
        .await
        .map_err(|e| classify_wreq_error(&e, "read body"))?;

    Ok(HtmlResponse {
        meta: HttpResponseMeta {
            status,
            headers,
            final_url,
            elapsed_ms,
        },
        body,
    })
}

// ===================================================================
// Per-host delay spacing (process-wide, keyed by host).
// ===================================================================

fn host_clock() -> &'static Mutex<HashMap<String, Instant>> {
    static CLOCK: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    CLOCK.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Enforce a minimum `delay_ms` between requests to the same host, plus a small
/// (10%) random jitter so spacing is not perfectly periodic. No-op when
/// `delay_ms == 0` or the host cannot be parsed.
async fn apply_host_delay(url: &str, delay_ms: u64) {
    if delay_ms == 0 {
        return;
    }
    let Some(host) = host_of(url) else {
        return;
    };

    let wait = {
        let mut clock = host_clock().lock().unwrap();
        let now = Instant::now();
        let base = match clock.get(&host) {
            Some(last) => {
                let since = now.duration_since(*last).as_millis() as u64;
                delay_ms.saturating_sub(since)
            }
            None => 0,
        };
        // Reserve our slot now (optimistically), so concurrent callers to the
        // same host serialize behind us rather than all reading the same stale
        // timestamp.
        clock.insert(host.clone(), now + Duration::from_millis(base));
        base
    };

    let total = wait + jitter_ms(delay_ms);
    if total > 0 {
        tokio::time::sleep(Duration::from_millis(total)).await;
    }
}

/// 0..=10% of `delay_ms`, as an additive jitter.
fn jitter_ms(delay_ms: u64) -> u64 {
    if delay_ms == 0 {
        return 0;
    }
    let span = (delay_ms / 10).max(1);
    rand::random::<u64>() % (span + 1)
}

// ===================================================================
// robots.txt: fetch + per-host cache + allow/deny.
// ===================================================================

fn robots_cache() -> &'static Mutex<HashMap<String, Arc<Robots>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Robots>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Fetch (once per host, then cached) and consult robots.txt; deny → a
/// `NetKind::Status` network error carrying the disallowed path. A robots file
/// that cannot be fetched is treated as "allow all" (standard crawler
/// behavior), and is cached so we do not re-probe on every request.
async fn enforce_robots(
    client: &Client,
    jar: &Arc<Jar>,
    url: &str,
    opts: &SessionOpts,
) -> Result<(), DracoError> {
    let Some((scheme, host, path)) = split_url(url) else {
        return Ok(()); // unparseable → nothing to gate on
    };

    // Never gate the robots.txt fetch on itself.
    if path == "/robots.txt" {
        return Ok(());
    }

    let origin = format!("{scheme}://{host}");
    if let Some(robots) = robots_cache().lock().unwrap().get(&host).cloned() {
        return robots.decision(&path);
    }

    // Cache miss: fetch it. We still honor per-host spacing for the probe, and
    // dress it with the call's jar + timeout (neither lives on the shared
    // client anymore).
    apply_host_delay(url, opts.delay_ms).await;
    let robots_url = format!("{origin}/robots.txt");
    let parsed = match dress(client.get(&robots_url), jar, opts).send().await {
        Ok(resp) if resp.status().is_success() => match resp.text().await {
            Ok(body) => Robots::parse(&body, ROBOTS_UA_TOKEN),
            Err(_) => Robots::allow_all(),
        },
        // 4xx/5xx or transport error → treat as unrestricted.
        _ => Robots::allow_all(),
    };

    let robots = Arc::new(parsed);
    robots_cache()
        .lock()
        .unwrap()
        .insert(host.clone(), robots.clone());
    robots.decision(&path)
}

/// A minimal robots.txt model: the set of `Disallow` / `Allow` path prefixes
/// that apply to our user-agent (longest-match wins, per the de-facto spec).
#[derive(Debug, Default, Clone)]
struct Robots {
    /// The applicable rules, in file order.
    rules: Vec<Rule>,
}

/// A single robots.txt path rule: a path `prefix` and whether it `allow`s.
#[derive(Debug, Clone)]
struct Rule {
    prefix: String,
    allow: bool,
}

/// A `User-agent:` group: the agent tokens it names and its rules.
#[derive(Debug, Default)]
struct RuleGroup {
    agents: Vec<String>,
    rules: Vec<Rule>,
}

impl Robots {
    fn allow_all() -> Self {
        Robots { rules: Vec::new() }
    }

    /// Parse robots.txt, keeping only rule groups whose `User-agent` matches
    /// `ua_token` (case-insensitive substring) or `*`. If both a specific group
    /// and `*` exist, the specific group wins and `*` is dropped.
    fn parse(body: &str, ua_token: &str) -> Self {
        let mut groups: Vec<RuleGroup> = Vec::new();
        let mut cur = RuleGroup::default();
        // Once we start seeing rules, the next `User-agent` opens a new group.
        let mut in_rules = false;

        for raw in body.lines() {
            // Strip comments and surrounding whitespace.
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((field, value)) = line.split_once(':') else {
                continue;
            };
            let field = field.trim().to_ascii_lowercase();
            let value = value.trim().to_string();

            match field.as_str() {
                "user-agent" => {
                    if in_rules {
                        if !cur.agents.is_empty() {
                            groups.push(std::mem::take(&mut cur));
                        } else {
                            cur = RuleGroup::default();
                        }
                        in_rules = false;
                    }
                    cur.agents.push(value.to_ascii_lowercase());
                }
                "disallow" => {
                    in_rules = true;
                    cur.rules.push(Rule {
                        prefix: value,
                        allow: false,
                    });
                }
                "allow" => {
                    in_rules = true;
                    cur.rules.push(Rule {
                        prefix: value,
                        allow: true,
                    });
                }
                // Sitemap, Crawl-delay, etc. are ignored for allow/deny.
                _ => {}
            }
        }
        if !cur.agents.is_empty() {
            groups.push(cur);
        }

        // Select the best-matching group: prefer a specific token match over `*`.
        let ua = ua_token.to_ascii_lowercase();
        let mut specific: Option<Vec<Rule>> = None;
        let mut wildcard: Option<Vec<Rule>> = None;
        for group in groups {
            for agent in &group.agents {
                if agent == "*" {
                    wildcard.get_or_insert_with(|| group.rules.clone());
                } else if ua.contains(agent) || agent.contains(&ua) {
                    specific.get_or_insert_with(|| group.rules.clone());
                }
            }
        }

        Robots {
            rules: specific.or(wildcard).unwrap_or_default(),
        }
    }

    /// Longest-match decision for `path`. An empty `Disallow:` means "allow
    /// all"; the longest matching rule wins, ties break toward `Allow`.
    fn decision(&self, path: &str) -> Result<(), DracoError> {
        let mut best: Option<(usize, bool)> = None;
        for rule in &self.rules {
            if rule.prefix.is_empty() {
                // `Disallow:` (empty) explicitly allows everything; treat as a
                // zero-length allow that anything else outranks.
                continue;
            }
            if path_matches(&rule.prefix, path) {
                let len = rule.prefix.len();
                match best {
                    Some((best_len, _)) if best_len > len => {}
                    Some((best_len, _)) if best_len == len => {
                        // tie → allow wins
                        if rule.allow {
                            best = Some((len, true));
                        }
                    }
                    _ => best = Some((len, rule.allow)),
                }
            }
        }
        match best {
            Some((_, false)) => Err(net_err(
                NetKind::Status,
                format!("blocked by robots.txt: {path}"),
            )),
            _ => Ok(()),
        }
    }
}

/// robots.txt path matching: prefix match with `*` wildcard and `$` end-anchor
/// support (the two extensions every major crawler honors).
fn path_matches(pattern: &str, path: &str) -> bool {
    // Fast path: no wildcards → plain prefix (with optional `$` anchor).
    if !pattern.contains('*') {
        if let Some(stripped) = pattern.strip_suffix('$') {
            return path == stripped;
        }
        return path.starts_with(pattern);
    }

    // Wildcard match: split on `*`, each segment must appear in order. A
    // trailing `$` anchors the final segment to the end of `path`.
    let (pat, anchored) = match pattern.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pattern, false),
    };
    let segments: Vec<&str> = pat.split('*').collect();
    let mut idx = 0usize;
    for (i, seg) in segments.iter().enumerate() {
        if seg.is_empty() {
            continue;
        }
        if i == 0 {
            // First segment must match at the start.
            if !path[idx..].starts_with(seg) {
                return false;
            }
            idx += seg.len();
        } else if let Some(found) = path[idx..].find(seg) {
            idx += found + seg.len();
        } else {
            return false;
        }
    }
    if anchored {
        // The last non-empty segment must land exactly at the end.
        return path.len() == idx || pat.ends_with('*');
    }
    true
}

// ===================================================================
// Backoff / Retry-After
// ===================================================================

/// 429 (Too Many Requests) and 503 (Service Unavailable) are retryable.
fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 503)
}

/// Compute the delay (ms) before retry `attempt` (0-based). Honors a
/// `Retry-After` header when present (delta-seconds only; HTTP-date is ignored
/// and falls back to exponential backoff), capped at [`RETRY_AFTER_CAP_MS`].
/// Without a hint, uses exponential backoff `BASE * 2^attempt` plus jitter.
fn backoff_delay(attempt: u32, retry_after: Option<&str>) -> u64 {
    if let Some(secs) = retry_after.and_then(parse_retry_after_secs) {
        return (secs.saturating_mul(1_000)).min(RETRY_AFTER_CAP_MS);
    }
    let factor = 1u64 << attempt.min(6); // cap the shift
    let base = BACKOFF_BASE_MS.saturating_mul(factor);
    let jitter = base / 4;
    (base + (rand::random::<u64>() % (jitter + 1))).min(RETRY_AFTER_CAP_MS)
}

/// Parse the delta-seconds form of `Retry-After`. Returns `None` for the
/// HTTP-date form (callers fall back to exponential backoff).
fn parse_retry_after_secs(value: &str) -> Option<u64> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    // Pure integer seconds only; anything else (e.g. a date) → None.
    v.parse::<u64>().ok()
}

// ===================================================================
// Header helpers
// ===================================================================

/// Response headers → ordered `Vec<(String, String)>`, lossy-decoding any
/// non-UTF-8 value so a weird header can never fail the whole fetch.
fn collect_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect()
}

/// Ordered request headers built from a spec: a [`HeaderMap`] carrying the
/// values plus an [`OrigHeaderMap`] carrying the original name casing and
/// insertion order (what wreq emits on the wire).
#[derive(Debug)]
struct OrderedHeaders {
    map: HeaderMap,
    orig: OrigHeaderMap,
}

fn build_ordered_headers(pairs: &[(String, String)]) -> Result<OrderedHeaders, DracoError> {
    let mut map = HeaderMap::with_capacity(pairs.len());
    let mut orig = OrigHeaderMap::new();
    for (name, value) in pairs {
        let hname = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
            net_err(
                NetKind::Status,
                format!("invalid header name {name:?}: {e}"),
            )
        })?;
        let hval = HeaderValue::from_str(value).map_err(|e| {
            net_err(
                NetKind::Status,
                format!("invalid header value for {name:?}: {e}"),
            )
        })?;
        // `append` keeps multi-valued headers (e.g. duplicate Cookie/Set-Cookie).
        map.append(hname, hval);
        // Record the original spelling (verbatim) to drive on-wire ordering +
        // casing. `IntoOrigHeaderName` is implemented for owned `String`, so we
        // hand it a clone of the caller's exact spelling.
        orig.insert(name.clone());
    }
    Ok(OrderedHeaders { map, orig })
}

// ===================================================================
// Small parsing utilities
// ===================================================================

/// Parse an HTTP method string into a [`wreq::Method`].
fn parse_method(method: &str) -> Result<wreq::Method, DracoError> {
    wreq::Method::from_bytes(method.as_bytes())
        .map_err(|e| net_err(NetKind::Status, format!("invalid method {method:?}: {e}")))
}

/// Decode an optional base64 request body.
fn decode_body(body_b64: Option<&str>) -> Result<Option<Bytes>, DracoError> {
    match body_b64 {
        None => Ok(None),
        Some(b64) => {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(b64.as_bytes())
                .map_err(|e| net_err(NetKind::Body, format!("bad base64 body: {e}")))?;
            Ok(Some(Bytes::from(raw)))
        }
    }
}

/// Host (authority without userinfo/port) of a URL, lowercased.
fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
}

/// Split a URL into `(scheme, host, path-with-query)`.
fn split_url(url: &str) -> Option<(String, String, String)> {
    let parsed = url::Url::parse(url).ok()?;
    let scheme = parsed.scheme().to_string();
    let host = parsed.host_str()?.to_ascii_lowercase();
    let mut path = parsed.path().to_string();
    if let Some(q) = parsed.query() {
        path.push('?');
        path.push_str(q);
    }
    if path.is_empty() {
        path.push('/');
    }
    Some((scheme, host, path))
}

// ===================================================================
// Error construction / classification
// ===================================================================

/// Build a [`DracoError::Network`].
fn net_err(reason: NetKind, detail: impl Into<String>) -> DracoError {
    DracoError::Network {
        reason,
        detail: detail.into(),
    }
}

/// Map a [`wreq::Error`] to the most specific [`NetKind`]. Order matters:
/// proxy-connect and timeout are checked before the generic connect bucket, and
/// redirect/status before body/decode.
fn classify_wreq_error(e: &wreq::Error, ctx: &str) -> DracoError {
    let reason = if e.is_timeout() {
        NetKind::Timeout
    } else if e.is_proxy_connect() {
        NetKind::Proxy
    } else if e.is_redirect() {
        NetKind::TooManyRedirects
    } else if e.is_status() {
        NetKind::Status
    } else if e.is_connect() {
        // A connect failure with no resolvable host is our best DNS signal:
        // wreq folds resolver errors into the connect phase.
        if looks_like_dns(e) {
            NetKind::Dns
        } else {
            NetKind::Tls
        }
    } else if is_tls_error(e) {
        NetKind::Tls
    } else if e.is_body() || e.is_decode() {
        NetKind::Body
    } else {
        // Builder/request/other: surface as a connect-ish transport failure.
        NetKind::Tls
    };
    net_err(reason, format!("{ctx}: {e}"))
}

/// `is_tls` may be gated/renamed across wreq point releases; probe the error
/// chain textually as a fallback so TLS failures never masquerade as something
/// else.
fn is_tls_error(e: &wreq::Error) -> bool {
    if e.is_tls() {
        return true;
    }
    let mut src: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(cur) = src {
        let s = cur.to_string().to_ascii_lowercase();
        if s.contains("tls")
            || s.contains("ssl")
            || s.contains("certificate")
            || s.contains("handshake")
        {
            return true;
        }
        src = cur.source();
    }
    false
}

/// Heuristic: does a connect-phase error look like a name-resolution failure?
fn looks_like_dns(e: &wreq::Error) -> bool {
    let mut src: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(cur) = src {
        let s = cur.to_string().to_ascii_lowercase();
        if s.contains("dns")
            || s.contains("resolve")
            || s.contains("name or service not known")
            || s.contains("failed to lookup")
            || s.contains("nodename nor servname")
        {
            return true;
        }
        src = cur.source();
    }
    false
}

// ===================================================================
// Tests (offline only; any live-network test is #[ignore]d).
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- SessionOpts defaults ----------------------------------------

    #[test]
    fn session_opts_defaults() {
        let o = SessionOpts::default();
        assert_eq!(o.proxy, None);
        assert_eq!(o.delay_ms, 0);
        assert!(o.respect_robots);
        assert_eq!(o.timeout_ms, 30_000);
    }

    // ---- Header order preservation -----------------------------------

    #[test]
    fn ordered_headers_preserve_insertion_order_and_casing() {
        let pairs = vec![
            ("User-Agent".to_string(), "draco/0.1".to_string()),
            ("Accept".to_string(), "text/html".to_string()),
            ("X-Custom-Order".to_string(), "1".to_string()),
            ("accept-language".to_string(), "en-US".to_string()),
        ];
        let ordered = build_ordered_headers(&pairs).expect("build");

        // The orig map drives on-wire order + casing; verify it round-trips the
        // exact spellings in the exact order we inserted them. `OrigHeaderName`
        // exposes its bytes via `AsRef<[u8]>` (it is not `Display`).
        let seen: Vec<String> = ordered
            .orig
            .iter()
            .map(|(_, orig)| String::from_utf8_lossy(orig.as_ref()).into_owned())
            .collect();
        assert_eq!(
            seen,
            vec!["User-Agent", "Accept", "X-Custom-Order", "accept-language"],
            "header order/casing must be preserved verbatim"
        );

        // And the value map carries the right values.
        assert_eq!(
            ordered.map.get("accept").map(|v| v.to_str().unwrap()),
            Some("text/html")
        );
    }

    #[test]
    fn ordered_headers_keep_duplicate_values() {
        let pairs = vec![
            ("Cookie".to_string(), "a=1".to_string()),
            ("Cookie".to_string(), "b=2".to_string()),
        ];
        let ordered = build_ordered_headers(&pairs).expect("build");
        let cookies: Vec<&str> = ordered
            .map
            .get_all("cookie")
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(cookies, vec!["a=1", "b=2"]);
    }

    #[test]
    fn invalid_header_name_is_a_network_error() {
        let pairs = vec![("Bad Header".to_string(), "x".to_string())];
        let err = build_ordered_headers(&pairs).unwrap_err();
        match err {
            DracoError::Network { reason, .. } => assert_eq!(reason, NetKind::Status),
            other => panic!("expected Network error, got {other:?}"),
        }
    }

    // ---- Body decoding -----------------------------------------------

    #[test]
    fn decode_body_roundtrip_and_errors() {
        assert!(decode_body(None).unwrap().is_none());
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"hello");
        assert_eq!(decode_body(Some(&b64)).unwrap().unwrap().as_ref(), b"hello");
        let err = decode_body(Some("!!!not base64!!!")).unwrap_err();
        assert!(matches!(
            err,
            DracoError::Network {
                reason: NetKind::Body,
                ..
            }
        ));
    }

    #[test]
    fn parse_method_maps_bad_input() {
        assert_eq!(parse_method("POST").unwrap(), wreq::Method::POST);
        assert!(parse_method("BAD METHOD").is_err());
    }

    // ---- URL helpers -------------------------------------------------

    #[test]
    fn split_url_extracts_scheme_host_path() {
        let (s, h, p) = split_url("https://Example.COM/a/b?q=1").unwrap();
        assert_eq!(s, "https");
        assert_eq!(h, "example.com");
        assert_eq!(p, "/a/b?q=1");

        let (_, _, root) = split_url("https://example.com").unwrap();
        assert_eq!(root, "/");
        assert!(split_url("not a url").is_none());
    }

    // ---- robots.txt parsing + decisions ------------------------------

    #[test]
    fn robots_disallow_blocks_matching_prefix() {
        let body = "User-agent: *\nDisallow: /private\nAllow: /private/ok\n";
        let r = Robots::parse(body, ROBOTS_UA_TOKEN);
        assert!(r.decision("/public").is_ok());
        assert!(r.decision("/private").is_err());
        assert!(r.decision("/private/secret").is_err());
        // Longer Allow overrides shorter Disallow.
        assert!(r.decision("/private/ok").is_ok());
    }

    #[test]
    fn robots_empty_disallow_allows_everything() {
        let body = "User-agent: *\nDisallow:\n";
        let r = Robots::parse(body, ROBOTS_UA_TOKEN);
        assert!(r.decision("/anything").is_ok());
        assert!(r.decision("/a/b/c").is_ok());
    }

    #[test]
    fn robots_specific_agent_group_wins_over_wildcard() {
        // Wildcard blocks all; our token's group allows all.
        let body = "\
User-agent: *
Disallow: /

User-agent: draco-bot
Disallow:
";
        let r = Robots::parse(body, ROBOTS_UA_TOKEN);
        // ROBOTS_UA_TOKEN is "draco", which is a substring of "draco-bot".
        assert!(
            r.decision("/anything").is_ok(),
            "specific group (draco-bot) should override the wildcard block"
        );

        // A client whose token does not match falls back to the wildcard block.
        let r_other = Robots::parse(body, "googlebot");
        assert!(r_other.decision("/anything").is_err());
    }

    #[test]
    fn robots_wildcard_and_anchor_patterns() {
        assert!(path_matches("/*.json", "/data/x.json"));
        assert!(!path_matches("/*.json", "/data/x.html"));
        assert!(path_matches("/end$", "/end"));
        assert!(!path_matches("/end$", "/end/more"));
        assert!(path_matches("/plain", "/plain/deeper"));

        let body = "User-agent: *\nDisallow: /*.pdf$\n";
        let r = Robots::parse(body, ROBOTS_UA_TOKEN);
        assert!(r.decision("/docs/manual.pdf").is_err());
        assert!(r.decision("/docs/manual.pdfx").is_ok());
    }

    #[test]
    fn robots_ignores_comments_and_unknown_fields() {
        let body = "\
# a comment
User-agent: *   # trailing comment
Crawl-delay: 5
Sitemap: https://x/sitemap.xml
Disallow: /nope
";
        let r = Robots::parse(body, ROBOTS_UA_TOKEN);
        assert!(r.decision("/nope").is_err());
        assert!(r.decision("/yes").is_ok());
    }

    #[test]
    fn robots_allow_all_when_no_rules() {
        let r = Robots::allow_all();
        assert!(r.decision("/whatever").is_ok());
    }

    // ---- backoff / Retry-After ---------------------------------------

    #[test]
    fn retryable_status_set() {
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(503));
        assert!(!is_retryable_status(200));
        assert!(!is_retryable_status(500));
        assert!(!is_retryable_status(404));
    }

    #[test]
    fn retry_after_seconds_is_honored_and_capped() {
        // Numeric seconds → exact milliseconds.
        assert_eq!(backoff_delay(0, Some("2")), 2_000);
        assert_eq!(backoff_delay(5, Some("0")), 0);
        // Capped so a hostile server can't park us.
        assert_eq!(backoff_delay(0, Some("9999")), RETRY_AFTER_CAP_MS);
    }

    #[test]
    fn retry_after_date_form_falls_back_to_backoff() {
        // HTTP-date form is not parsed → None → exponential backoff path.
        assert!(parse_retry_after_secs("Wed, 21 Oct 2099 07:28:00 GMT").is_none());
        assert!(parse_retry_after_secs("").is_none());
        assert_eq!(parse_retry_after_secs("3"), Some(3));
    }

    #[test]
    fn exponential_backoff_grows_and_stays_bounded() {
        // Base window (base * 2^attempt) with up to +25% jitter.
        for attempt in 0..5 {
            let d = backoff_delay(attempt, None);
            let factor = 1u64 << attempt;
            let base = BACKOFF_BASE_MS * factor;
            assert!(d >= base, "attempt {attempt}: {d} < base {base}");
            assert!(
                d <= (base + base / 4).min(RETRY_AFTER_CAP_MS),
                "attempt {attempt}: {d} exceeds base+jitter"
            );
        }
        // Even a large attempt stays under the cap.
        assert!(backoff_delay(30, None) <= RETRY_AFTER_CAP_MS);
    }

    // ---- jitter ------------------------------------------------------

    #[test]
    fn jitter_is_within_ten_percent() {
        assert_eq!(jitter_ms(0), 0);
        for _ in 0..1000 {
            assert!(jitter_ms(1000) <= 100);
            assert!(jitter_ms(50) <= 5);
            // delay < 10 still yields a valid (<=1) jitter, never panics.
            assert!(jitter_ms(3) <= 1);
        }
    }

    // ---- shared client pool -------------------------------------------

    #[test]
    fn shared_client_is_cached_per_proxy() {
        // First call for the default (no-proxy) key builds + caches it.
        let _a = shared_client(None).expect("build default client");
        assert!(
            client_pool().lock().unwrap().contains_key(""),
            "no-proxy client should be cached under the empty key"
        );

        // A distinct proxy is a distinct pool entry (parse only — never dialed).
        let proxy = "http://127.0.0.1:59321";
        let _p = shared_client(Some(proxy)).expect("build proxied client");
        assert!(client_pool().lock().unwrap().contains_key(proxy));

        // Re-requesting the same proxy reuses the cached client (returns a
        // clone sharing its pool) rather than rebuilding — no new key appears.
        let before = client_pool().lock().unwrap().len();
        let _p2 = shared_client(Some(proxy)).expect("reuse proxied client");
        let after = client_pool().lock().unwrap().len();
        assert_eq!(before, after, "same proxy must not add a pool entry");
    }

    #[test]
    fn empty_proxy_string_maps_to_the_no_proxy_client() {
        // An empty/whitespace proxy is treated as "no proxy", not a distinct key.
        let _ = shared_client(Some("")).expect("empty proxy → default client");
        assert!(client_pool().lock().unwrap().contains_key(""));
    }

    // ---- error classification ----------------------------------------

    #[test]
    fn net_err_shape() {
        let e = net_err(NetKind::Dns, "boom");
        match e {
            DracoError::Network { reason, detail } => {
                assert_eq!(reason, NetKind::Dns);
                assert_eq!(detail, "boom");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    // ---- live network smoke test (never run in CI/sandbox) -----------

    #[tokio::test]
    #[ignore = "requires live external network + BoringSSL runtime"]
    async fn live_fetch_smoke() {
        let opts = SessionOpts {
            respect_robots: false,
            ..Default::default()
        };
        let resp = fetch_target("https://tls.peet.ws/api/all", &opts)
            .await
            .expect("live fetch");
        assert_eq!(resp.meta.status, 200);
        assert!(!resp.body.is_empty());
    }
}
