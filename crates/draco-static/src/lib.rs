//! # draco-static (WS-B)
//!
//! Tier 0 static extraction + Tier 1 build-id URL construction. Implemented
//! against canonical spec §10. Pure and synchronous: bytes in, structured data
//! out — no I/O, no async, no global state.
//!
//! ## Tier 0 (`extract_static`)
//! Scans raw HTML, in priority order, for embedded application state:
//! 1. `<script id="__NEXT_DATA__">` → its JSON body ([`ExtractOrigin::NextData`]).
//! 2. every `<script type="application/ld+json">` → a JSON array of the parsed
//!    blocks ([`ExtractOrigin::JsonLd`]).
//! 3. `window.__NUXT__ = { … }` in **object-literal** form → the parsed object
//!    ([`ExtractOrigin::NuxtWindow`]). The `window.__NUXT__=(function(){…}())`
//!    **factory** form is deliberately a [`StaticOutcome::Miss`]: it cannot be
//!    evaluated without a JS runtime, so the caller must escalate.
//!
//! ## Tier 1 (`discover_build_id` / `next_data_url` / `is_app_router`)
//! Helpers for Next.js *pages-router* `_next/data` replay. App-router (RSC)
//! pages are detected by [`is_app_router`] and are **not** Tier-1 eligible in
//! v0.1.
//!
//! **Frozen public API** — the four public function signatures and
//! [`StaticOutcome`] are fixed by the spec; only the bodies live here.
//!
//! Parsing uses [`tl`] as the primary (fast, zero-copy) HTML parser, with
//! [`scraper`] as a fallback for pathological markup that `tl` mis-parses.

use draco_types::{ExtractOrigin, ExtractedData, SourceTier};
use serde_json::Value;

pub mod content;

/// Result of a Tier 0 static extraction attempt.
#[derive(Debug, Clone)]
pub enum StaticOutcome {
    /// A paradigm matched.
    Hit(ExtractedData),
    /// Nothing matched; caller should escalate.
    Miss,
}

/// Tier 0: scan raw HTML for `__NEXT_DATA__`, JSON-LD, and object-literal
/// `window.__NUXT__`.
///
/// Paradigms are tried in the fixed priority order documented on the crate:
/// `__NEXT_DATA__` → JSON-LD → object-literal `window.__NUXT__`. The first one
/// that yields well-formed JSON wins; if none do, the result is
/// [`StaticOutcome::Miss`] and the caller should escalate.
pub fn extract_static(html: &str) -> StaticOutcome {
    if let Some(data) = extract_next_data(html) {
        return StaticOutcome::Hit(ExtractedData {
            tier: SourceTier::Static,
            origin: ExtractOrigin::NextData,
            data,
        });
    }

    if let Some(data) = extract_json_ld(html) {
        return StaticOutcome::Hit(ExtractedData {
            tier: SourceTier::Static,
            origin: ExtractOrigin::JsonLd,
            data,
        });
    }

    if let Some(data) = extract_nuxt_window(html) {
        return StaticOutcome::Hit(ExtractedData {
            tier: SourceTier::Static,
            origin: ExtractOrigin::NuxtWindow,
            data,
        });
    }

    StaticOutcome::Miss
}

/// Tier 1: discover a Next.js build id from the HTML, if present.
///
/// Sources are consulted in order of trustworthiness:
/// 1. `__NEXT_DATA__.buildId` (the authoritative value the server embedded);
/// 2. a `__BUILD_ID = "…"` script assignment;
/// 3. a `/_next/static/<buildId>/` asset path (preferring the `_buildManifest.js`
///    / `_ssgManifest.js` markers, which live directly under the build-id dir).
pub fn discover_build_id(html: &str) -> Option<String> {
    // (1) __NEXT_DATA__.buildId — the canonical source.
    if let Some(value) = extract_next_data(html) {
        if let Some(id) = value.get("buildId").and_then(Value::as_str) {
            if is_plausible_build_id(id) {
                return Some(id.to_string());
            }
        }
    }

    // (2) An explicit `__BUILD_ID = "…"` assignment.
    if let Some(id) = scan_build_id_assignment(html) {
        return Some(id);
    }

    // (3) A `/_next/static/<buildId>/…` asset path.
    scan_next_static_build_id(html)
}

/// Tier 1: construct the `_next/data/<build_id><pathname>.json` URL for a route.
///
/// The `pathname` is expected to be an absolute path (leading `/`); a bare `/`
/// maps to `/index` per Next.js convention, and a trailing slash is dropped.
/// `query` pairs are percent-encoded and appended verbatim in the given order.
pub fn next_data_url(build_id: &str, pathname: &str, query: &[(String, String)]) -> String {
    let mut route = pathname.trim();
    // Normalize away a trailing slash (except the bare root).
    while route.len() > 1 && route.ends_with('/') {
        route = &route[..route.len() - 1];
    }

    // Next.js writes the index route as `/index.json`.
    let route_segment: String = if route.is_empty() || route == "/" {
        "/index".to_string()
    } else if route.starts_with('/') {
        route.to_string()
    } else {
        format!("/{route}")
    };

    let mut url = format!("/_next/data/{build_id}{route_segment}.json");

    if !query.is_empty() {
        url.push('?');
        for (i, (key, val)) in query.iter().enumerate() {
            if i > 0 {
                url.push('&');
            }
            url.push_str(&percent_encode_component(key));
            url.push('=');
            url.push_str(&percent_encode_component(val));
        }
    }

    url
}

/// Detect Next.js **app-router** (RSC) pages, which are NOT Tier-1 eligible in
/// v0.1.
///
/// App-router pages stream their payload through `self.__next_f.push(…)` flight
/// chunks and do not embed a `__NEXT_DATA__` blob the way the pages-router does.
/// The presence of the flight sink is the reliable discriminator.
pub fn is_app_router(html: &str) -> bool {
    // The flight-data sink is emitted by every app-router document.
    if html.contains("__next_f") {
        return true;
    }

    // Streaming RSC markup uses these bootstrap globals; treat them as
    // corroborating signals when the flight sink was minified/renamed.
    (html.contains("__RSC_") || html.contains("react-server-dom"))
        && !html.contains("__NEXT_DATA__")
}

// ===================================================================
// Tier 0 paradigm matchers
// ===================================================================

/// Extract and parse the `<script id="__NEXT_DATA__">` JSON blob.
fn extract_next_data(html: &str) -> Option<Value> {
    // Primary: tl.
    if let Ok(dom) = tl::parse(html, tl::ParserOptions::default()) {
        let parser = dom.parser();
        if let Some(handle) = dom.get_element_by_id("__NEXT_DATA__") {
            if let Some(tag) = handle.get(parser).and_then(|n| n.as_tag()) {
                let body = tag.inner_text(parser);
                if let Some(v) = parse_json_relaxed(body.trim()) {
                    return Some(v);
                }
            }
        }
    }

    // Fallback: scraper.
    let document = scraper::Html::parse_document(html);
    let selector = scraper::Selector::parse(r#"script#__NEXT_DATA__"#).ok()?;
    let element = document.select(&selector).next()?;
    let body = element.inner_html();
    parse_json_relaxed(body.trim())
}

/// Collect every `<script type="application/ld+json">` block into a JSON array.
///
/// Each block is parsed independently; a block that is itself an array is kept
/// as a single element (matching how JSON-LD `@graph`-style sites embed data).
/// Returns `None` when no well-formed block exists.
fn extract_json_ld(html: &str) -> Option<Value> {
    let mut blocks: Vec<Value> = Vec::new();

    // Primary: tl. Iterate every <script> and filter on the `type` attribute
    // ourselves — this avoids the query-selector's attribute-value parser
    // tripping over the `/` and `+` in the media type.
    if let Ok(dom) = tl::parse(html, tl::ParserOptions::default()) {
        let parser = dom.parser();
        for node in dom.nodes() {
            let Some(tag) = node.as_tag() else { continue };
            if !tag.name().as_bytes().eq_ignore_ascii_case(b"script") {
                continue;
            }
            let is_ld = tag
                .attributes()
                .get("type")
                .flatten()
                .map(|b| {
                    b.as_utf8_str()
                        .trim()
                        .eq_ignore_ascii_case("application/ld+json")
                })
                .unwrap_or(false);
            if !is_ld {
                continue;
            }
            let body = tag.inner_text(parser);
            if let Some(v) = parse_json_relaxed(body.trim()) {
                blocks.push(v);
            }
        }
    }

    // Fallback: scraper, if tl found nothing.
    if blocks.is_empty() {
        let document = scraper::Html::parse_document(html);
        if let Ok(selector) = scraper::Selector::parse(r#"script[type="application/ld+json"]"#) {
            for element in document.select(&selector) {
                let body = element.inner_html();
                if let Some(v) = parse_json_relaxed(body.trim()) {
                    blocks.push(v);
                }
            }
        }
    }

    if blocks.is_empty() {
        None
    } else {
        Some(Value::Array(blocks))
    }
}

/// Extract the object-literal `window.__NUXT__ = { … }` payload.
///
/// Only the object-literal form is supported. The IIFE/factory form
/// (`window.__NUXT__=(function(){…}())`) is intentionally rejected here —
/// evaluating it needs a JS runtime, so the caller escalates.
fn extract_nuxt_window(html: &str) -> Option<Value> {
    let assign = find_nuxt_assignment(html)?;
    // Skip whitespace after `=` to look at the first significant char.
    let rhs = assign.trim_start();

    // Factory form: `(function(){…}())` / `(function(){…})()`. Reject.
    if rhs.starts_with('(') {
        return None;
    }
    // Object-literal form must open with `{`.
    if !rhs.starts_with('{') {
        return None;
    }

    let object_src = balanced_span(rhs, '{', '}')?;
    parse_json_relaxed(object_src.trim())
}

/// Return the substring immediately following the first `window.__NUXT__=`
/// (allowing arbitrary whitespace around the `=`), up to the end of the input.
fn find_nuxt_assignment(html: &str) -> Option<&str> {
    let idx = html.find("__NUXT__")?;
    let after = &html[idx + "__NUXT__".len()..];
    let after = after.trim_start();
    let after = after.strip_prefix('=')?;
    Some(after)
}

// ===================================================================
// Tier 1 build-id discovery helpers
// ===================================================================

/// Scan for an explicit `__BUILD_ID = "…"` (or `'…'`) assignment.
fn scan_build_id_assignment(html: &str) -> Option<String> {
    let idx = html.find("__BUILD_ID")?;
    let after = html[idx + "__BUILD_ID".len()..].trim_start();
    let after = after.strip_prefix('=')?.trim_start();
    let quote = after.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &after[1..];
    let end = rest.find(quote)?;
    let id = &rest[..end];
    if is_plausible_build_id(id) {
        Some(id.to_string())
    } else {
        None
    }
}

/// Scan asset paths of the form `/_next/static/<buildId>/…`.
///
/// Segments that are well-known static sub-directories (`chunks`, `css`,
/// `media`, `image`) are skipped; the `_buildManifest.js` / `_ssgManifest.js`
/// markers (which sit directly under the build-id directory) are preferred.
fn scan_next_static_build_id(html: &str) -> Option<String> {
    const PREFIX: &str = "/_next/static/";
    const RESERVED: [&str; 4] = ["chunks", "css", "media", "image"];

    let mut fallback: Option<String> = None;
    let mut search = html;

    while let Some(pos) = search.find(PREFIX) {
        let after = &search[pos + PREFIX.len()..];
        // Advance the cursor past this prefix for the next iteration.
        search = after;

        let seg_end = after.find('/')?;
        let segment = &after[..seg_end];
        if segment.is_empty() || RESERVED.contains(&segment) || !is_plausible_build_id(segment) {
            continue;
        }

        let remainder = &after[seg_end..];
        if remainder.starts_with("/_buildManifest.js") || remainder.starts_with("/_ssgManifest.js")
        {
            // Highest-confidence marker — return immediately.
            return Some(segment.to_string());
        }
        fallback.get_or_insert_with(|| segment.to_string());
    }

    fallback
}

/// A build id is an opaque token (Next.js uses a ~21-char nanoid, but custom
/// ids are allowed). Guard against obvious non-ids: it must be non-empty, of
/// sane length, and contain only URL-path-safe id characters.
fn is_plausible_build_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

// ===================================================================
// Generic helpers
// ===================================================================

/// Parse a JSON document, tolerating leading/trailing junk that HTML minifiers
/// sometimes leave around an embedded blob (a trailing `;`, CDATA guards, HTML
/// comment wrappers). Returns `None` if no valid JSON value can be recovered.
fn parse_json_relaxed(input: &str) -> Option<Value> {
    if input.is_empty() {
        return None;
    }

    // Fast path: the whole string is JSON.
    if let Ok(v) = serde_json::from_str::<Value>(input) {
        return Some(v);
    }

    // Strip common wrappers and a single trailing `;`, then retry.
    let mut s = input.trim();
    s = s
        .strip_prefix("<!--")
        .map(str::trim)
        .unwrap_or(s)
        .strip_suffix("-->")
        .map(str::trim)
        .unwrap_or(s);
    if let Some(inner) = s.strip_prefix("/*<![CDATA[*/").map(str::trim) {
        s = inner
            .strip_suffix("/*]]>*/")
            .map(str::trim)
            .unwrap_or(inner);
    }
    let s = s.strip_suffix(';').map(str::trim).unwrap_or(s);

    // Last resort: carve out the first balanced `{…}` or `[…]` region and parse
    // just that. This rescues blobs prefixed by an assignment expression.
    let carved = balanced_span(s, '{', '}').or_else(|| balanced_span(s, '[', ']'));
    if let Some(region) = carved {
        if let Ok(v) = serde_json::from_str::<Value>(region.trim()) {
            return Some(v);
        }
    }

    serde_json::from_str::<Value>(s).ok()
}

/// Return the smallest prefix of `src` that starts at the first `open` char and
/// runs through its matching `close`, honoring JS/JSON string and comment
/// nesting so braces inside strings do not throw off the balance count.
///
/// Returns `None` if the delimiters are unbalanced.
fn balanced_span(src: &str, open: char, close: char) -> Option<&str> {
    let bytes = src.as_bytes();
    let start = src.find(open)?;

    let mut depth: i32 = 0;
    let mut in_string: Option<u8> = None; // the active quote byte, if inside a string
    let mut escaped = false;
    let mut i = start;

    while i < bytes.len() {
        let c = bytes[i];

        if let Some(quote) = in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == quote {
                in_string = None;
            }
            i += 1;
            continue;
        }

        match c {
            b'"' | b'\'' | b'`' => in_string = Some(c),
            _ if c as char == open => depth += 1,
            _ if c as char == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(&src[start..=i]);
                }
            }
            _ => {}
        }
        i += 1;
    }

    None
}

/// Percent-encode a single query-string component (key or value).
///
/// Unreserved characters (`A–Z a–z 0–9 - _ . ~`) pass through; a space becomes
/// `+`; everything else is `%XX` (UTF-8, upper-hex) — matching how Next.js
/// serializes route query params.
fn percent_encode_component(input: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(input.len());
    for &byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
    out
}
