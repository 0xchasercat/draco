//! # Content engine — URL → clean Markdown + metadata (Firecrawl-style scrape).
//!
//! Pure and synchronous: HTML bytes in, [`ScrapeResult`] (Markdown + metadata
//! JSON) out. No I/O, no async, no global state — the same discipline as the
//! rest of `draco-static`.
//!
//! The pipeline is three independent passes over the source HTML:
//!
//! 1. **Main-content extraction (readability).** [`dom_smoothie`] (a Mozilla
//!    Readability port) strips site chrome — `<nav>`/`<header>`/`<footer>`,
//!    sidebars, ad boilerplate — and hands back just the article's content
//!    HTML. It also absolutizes relative `href`/`src` against the document URL.
//!    When `only_main_content` is `false`, we skip readability and convert the
//!    whole `<body>` instead. When readability *fails* (a thin,
//!    client-rendered SPA shell with almost no content), we fall back to the
//!    body so the caller still gets whatever text is present.
//!
//! 2. **HTML → Markdown.** [`htmd`] converts the extracted content HTML,
//!    preserving headings, links, images, lists, blockquotes, code/`<pre>`, and
//!    GFM tables. Because `htmd` does not resolve relative URLs, we absolutize
//!    every `href`/`src` in the content HTML against `url` *before* conversion
//!    (belt-and-suspenders on the readability path, load-bearing on the
//!    whole-`<body>` path).
//!
//! 3. **Metadata.** We parse `<title>`, every `<meta name=…>` / `<meta
//!    property=…>` (flattened as `name → content`, so `og:*` / `twitter:*` /
//!    `description` / `viewport` / `theme-color` all appear), `<html lang>` →
//!    `language`, `<link rel=canonical>` → `canonical`, and `<link rel~=icon>`
//!    → `favicon` (absolutized). We always add `sourceURL`, `url`, `statusCode`
//!    and `contentType` from the call arguments.
//!
//! Parsing reuses the crate's existing [`scraper`] dependency (html5ever-based,
//! robust against messy real-world markup).

use scraper::{Html, Selector};
use serde_json::{Map, Value};

/// The output of [`scrape`]: clean Markdown plus a flat metadata object shaped
/// to match Firecrawl's `scrape` response.
#[derive(Debug, Clone)]
pub struct ScrapeResult {
    /// The main content (or whole body) rendered as GFM Markdown.
    pub markdown: String,
    /// A flat JSON object of page metadata (see the module docs for the keys).
    pub metadata: Value,
}

/// Scrape `html` into Markdown + metadata.
///
/// * `url` — the page URL, used both to absolutize relative links/images and as
///   the `sourceURL` / `url` metadata values.
/// * `status` — the HTTP status of the fetch, surfaced as `statusCode`.
/// * `content_type` — the response `Content-Type`, surfaced as `contentType`.
/// * `only_main_content` — when `true` (the default scrape mode) readability
///   strips boilerplate to the article body; when `false` the whole `<body>` is
///   converted.
///
/// Never panics: a readability failure or an `htmd` error degrades to the
/// whole-body conversion (or an empty string), and metadata extraction is
/// best-effort per field.
pub fn scrape(
    html: &str,
    url: &str,
    status: u16,
    content_type: &str,
    only_main_content: bool,
) -> ScrapeResult {
    let metadata = extract_metadata(html, url, status, content_type);

    // Choose the content HTML: readability's main content, or the whole body.
    let content_html = if only_main_content {
        main_content_html(html, url).unwrap_or_else(|| body_html(html))
    } else {
        body_html(html)
    };

    // Absolutize relative href/src against the page URL, then render Markdown.
    let absolutized = absolutize_urls(&content_html, url);
    let markdown = html_to_markdown(&absolutized);

    ScrapeResult { markdown, metadata }
}

/// Heuristic: does the readability output look like a thin SPA shell — i.e. did
/// extraction essentially find no article? Used by the caller to decide whether
/// to note that an SPA render pass would help. Kept here (next to the pipeline)
/// so the threshold lives with the extraction logic.
///
/// Returns `true` when the rendered Markdown has less than `min_chars` of
/// non-whitespace content.
pub fn is_thin_content(markdown: &str, min_chars: usize) -> bool {
    markdown.chars().filter(|c| !c.is_whitespace()).count() < min_chars
}

// ===================================================================
// Main-content extraction (readability)
// ===================================================================

/// Run readability over the document and return its main-content HTML, or
/// `None` if extraction fails (a thin SPA shell, or markup readability rejects).
fn main_content_html(html: &str, url: &str) -> Option<String> {
    use dom_smoothie::{Config, Readability, TextMode};

    // `TextMode::Markdown` only affects `text_content`; we render Markdown from
    // the `content` HTML ourselves via htmd, so the default text mode is fine.
    // Passing the document URL lets dom_smoothie absolutize links for us too.
    let cfg = Config {
        text_mode: TextMode::Formatted,
        ..Default::default()
    };
    let mut readability = Readability::new(html, Some(url), Some(cfg)).ok()?;
    let article = readability.parse().ok()?;
    let content: &str = article.content.as_ref();
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Return the inner HTML of `<body>`, or the whole document if there is no
/// `<body>` element. Used when `only_main_content` is false and as the fallback
/// when readability finds nothing.
fn body_html(html: &str) -> String {
    let document = Html::parse_document(html);
    if let Ok(sel) = Selector::parse("body") {
        if let Some(body) = document.select(&sel).next() {
            return body.inner_html();
        }
    }
    html.to_string()
}

// ===================================================================
// HTML → Markdown
// ===================================================================

/// Convert content HTML to Markdown with `htmd`. Scripts/styles are skipped so
/// stray inline JS/CSS never leaks into the Markdown. On an `htmd` error
/// (should not happen for parseable HTML) we degrade to the empty string rather
/// than panicking.
fn html_to_markdown(content_html: &str) -> String {
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "noscript", "iframe"])
        .build();
    converter
        .convert(content_html)
        .unwrap_or_default()
        .trim()
        .to_string()
}

// ===================================================================
// URL absolutization
// ===================================================================

/// Rewrite every `href="…"` / `src="…"` attribute value in `content_html` to an
/// absolute URL resolved against `base`. Values that are already absolute, are
/// fragment/`mailto:`/`javascript:`/`data:` URIs, or cannot be resolved are left
/// untouched.
///
/// Operates on the raw HTML string (a targeted attribute rewrite) rather than
/// re-serializing a parsed DOM: html5ever-based parsers here do not offer a
/// faithful round-trip serializer, and this keeps the transform cheap and
/// predictable.
fn absolutize_urls(content_html: &str, base: &str) -> String {
    let Ok(base_url) = url::Url::parse(base) else {
        // No usable base (relative/opaque input) — nothing to resolve against.
        return content_html.to_string();
    };

    let mut out = String::with_capacity(content_html.len() + 64);
    let bytes = content_html.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // Look for `href` or `src` attribute starts. Match case-insensitively
        // and only when preceded by whitespace (so we don't match inside a
        // longer attribute name like `data-src` — though those are harmless).
        if let Some((attr_len, quote_char, val_start)) = match_url_attr(content_html, i) {
            // Copy the attribute name + `=` + opening quote verbatim.
            out.push_str(&content_html[i..val_start]);
            // Find the closing quote.
            if let Some(rel_end) = content_html[val_start..].find(quote_char) {
                let value = &content_html[val_start..val_start + rel_end];
                out.push_str(&resolve_url(&base_url, value));
                out.push(quote_char);
                i = val_start + rel_end + 1;
                continue;
            }
            // Unterminated value — bail on the rewrite, copy the rest verbatim.
            out.push_str(&content_html[val_start..]);
            let _ = attr_len;
            return out;
        }
        // Not an attribute start: copy this char (respecting UTF-8 boundaries).
        let ch_len = utf8_char_len(bytes[i]);
        out.push_str(&content_html[i..i + ch_len]);
        i += ch_len;
    }

    out
}

/// If `s[i..]` begins with a whitespace-prefixed `href=`/`src=` attribute (any
/// case, optional whitespace around `=`) followed by a quote, return
/// `(matched_prefix_len, quote_char, value_start_index)`. The matched prefix is
/// the attribute name through the opening quote.
fn match_url_attr(s: &str, i: usize) -> Option<(usize, char, usize)> {
    let bytes = s.as_bytes();
    // Must be preceded by ASCII whitespace (attribute boundary).
    if i == 0 || !bytes[i - 1].is_ascii_whitespace() {
        return None;
    }
    let rest = &s[i..];
    let lower_start = rest.get(..4).map(|p| p.to_ascii_lowercase());
    let name_len = if rest.len() >= 5 && rest[..4].eq_ignore_ascii_case("href") {
        4
    } else if lower_start.is_some() && rest.len() >= 4 && rest[..3].eq_ignore_ascii_case("src") {
        3
    } else {
        return None;
    };

    // After the name: optional whitespace, `=`, optional whitespace, then a quote.
    let mut j = i + name_len;
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    if j >= bytes.len() || bytes[j] != b'=' {
        return None;
    }
    j += 1;
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    if j >= bytes.len() {
        return None;
    }
    let quote = bytes[j] as char;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let val_start = j + 1;
    Some((val_start - i, quote, val_start))
}

/// Resolve a single attribute value against `base`. Leaves already-absolute,
/// fragment, and non-navigational (`mailto:`/`javascript:`/`data:`/`tel:`) URIs
/// untouched.
fn resolve_url(base: &url::Url, value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return value.to_string();
    }
    let lower = trimmed.to_ascii_lowercase();
    // Skip non-navigational and already-absolute schemes, and pure fragments.
    if trimmed.starts_with('#')
        || lower.starts_with("mailto:")
        || lower.starts_with("javascript:")
        || lower.starts_with("data:")
        || lower.starts_with("tel:")
        || has_scheme(trimmed)
    {
        return value.to_string();
    }
    match base.join(trimmed) {
        Ok(abs) => abs.to_string(),
        Err(_) => value.to_string(),
    }
}

/// Does `s` start with an absolute-URL scheme (`scheme://` or `scheme:`)? A
/// lightweight check so we don't re-resolve values that are already absolute.
fn has_scheme(s: &str) -> bool {
    // Protocol-relative `//host/path` is treated as needing resolution (it picks
    // up the base scheme), so it is NOT considered to already have a scheme.
    if s.starts_with("//") {
        return false;
    }
    let scheme: String = s.chars().take_while(|c| *c != ':').collect();
    if scheme.is_empty() || scheme.len() == s.len() {
        return false;
    }
    let first = scheme.as_bytes()[0];
    first.is_ascii_alphabetic()
        && scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

/// Byte length of a UTF-8 code point given its leading byte.
fn utf8_char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1 // continuation/invalid byte: advance one to make progress
    }
}

// ===================================================================
// Metadata extraction
// ===================================================================

/// Build the flat metadata object. Best-effort per field; the four synthetic
/// keys (`sourceURL`, `url`, `statusCode`, `contentType`) are always present.
fn extract_metadata(html: &str, url: &str, status: u16, content_type: &str) -> Value {
    let document = Html::parse_document(html);
    let mut map = Map::new();

    // <title>
    if let Some(title) = select_text(&document, "title") {
        if !title.is_empty() {
            map.insert("title".to_string(), Value::String(title));
        }
    }

    // <html lang> → language
    if let Ok(sel) = Selector::parse("html") {
        if let Some(html_el) = document.select(&sel).next() {
            if let Some(lang) = html_el.value().attr("lang") {
                let lang = lang.trim();
                if !lang.is_empty() {
                    map.insert("language".to_string(), Value::String(lang.to_string()));
                }
            }
        }
    }

    // <meta name=…>/<meta property=…> → flatten each as name → content.
    // Later duplicates do not overwrite earlier ones (first wins), matching the
    // "first meaningful value" convention.
    if let Ok(sel) = Selector::parse("meta") {
        for meta in document.select(&sel) {
            let el = meta.value();
            let key = el
                .attr("name")
                .or_else(|| el.attr("property"))
                .or_else(|| el.attr("itemprop"));
            let Some(key) = key else { continue };
            let key = key.trim();
            if key.is_empty() {
                continue;
            }
            let content = el.attr("content").map(str::trim).filter(|c| !c.is_empty());
            let Some(content) = content else { continue };
            map.entry(key.to_string())
                .or_insert_with(|| Value::String(content.to_string()));
        }
    }

    // <link rel=canonical> → canonical (absolutized).
    if let Some(href) = select_link_href(&document, "canonical") {
        map.insert(
            "canonical".to_string(),
            Value::String(absolutize_one(url, &href)),
        );
    }

    // <link rel~=icon> → favicon (absolutized). Accept `icon`, `shortcut icon`,
    // `apple-touch-icon`, etc. Prefer the first plain `icon`, else the first
    // icon-ish rel.
    if let Some(href) = select_favicon_href(&document) {
        map.insert(
            "favicon".to_string(),
            Value::String(absolutize_one(url, &href)),
        );
    }

    // Synthetic keys — always present.
    map.insert("sourceURL".to_string(), Value::String(url.to_string()));
    map.insert("url".to_string(), Value::String(url.to_string()));
    map.insert(
        "statusCode".to_string(),
        Value::Number(serde_json::Number::from(status)),
    );
    map.insert(
        "contentType".to_string(),
        Value::String(content_type.to_string()),
    );

    Value::Object(map)
}

/// The trimmed text content of the first element matching `selector`.
fn select_text(document: &Html, selector: &str) -> Option<String> {
    let sel = Selector::parse(selector).ok()?;
    let el = document.select(&sel).next()?;
    let text: String = el.text().collect::<Vec<_>>().join(" ");
    Some(collapse_ws(&text))
}

/// The `href` of the first `<link rel="{rel}">`.
fn select_link_href(document: &Html, rel: &str) -> Option<String> {
    let sel = Selector::parse("link").ok()?;
    for link in document.select(&sel) {
        let el = link.value();
        let this_rel = el.attr("rel").unwrap_or("").trim();
        if this_rel.eq_ignore_ascii_case(rel) {
            if let Some(href) = el.attr("href") {
                let href = href.trim();
                if !href.is_empty() {
                    return Some(href.to_string());
                }
            }
        }
    }
    None
}

/// The `href` of the best favicon `<link>`. Prefers an exact `rel="icon"`, then
/// any rel whose whitespace-separated tokens include an icon token.
fn select_favicon_href(document: &Html) -> Option<String> {
    let sel = Selector::parse("link").ok()?;
    let mut fallback: Option<String> = None;
    for link in document.select(&sel) {
        let el = link.value();
        let rel = el.attr("rel").unwrap_or("").trim();
        let href = match el.attr("href").map(str::trim) {
            Some(h) if !h.is_empty() => h,
            _ => continue,
        };
        let tokens: Vec<String> = rel
            .split_whitespace()
            .map(|t| t.to_ascii_lowercase())
            .collect();
        if tokens.iter().any(|t| t == "icon") {
            return Some(href.to_string());
        }
        if tokens.iter().any(|t| t.contains("icon")) {
            fallback.get_or_insert_with(|| href.to_string());
        }
    }
    fallback
}

/// Absolutize a single URL against `base`, returning the input unchanged if the
/// base is unusable or resolution fails.
fn absolutize_one(base: &str, value: &str) -> String {
    match url::Url::parse(base) {
        Ok(b) => resolve_url(&b, value),
        Err(_) => value.to_string(),
    }
}

/// Collapse runs of whitespace (including newlines) into single spaces and trim.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ===================================================================
// Tests
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A representative article page with site chrome (nav/header/footer),
    /// rich metadata, and a body exercising every Markdown construct.
    const ARTICLE: &str = r##"<!doctype html>
<html lang="en">
<head>
  <title>The Great Article</title>
  <meta name="description" content="An article about interesting things.">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="theme-color" content="#ffffff">
  <meta property="og:title" content="The Great Article (OG)">
  <meta property="og:type" content="article">
  <meta name="twitter:card" content="summary_large_image">
  <link rel="canonical" href="/articles/great">
  <link rel="icon" href="/favicon.ico">
</head>
<body>
  <nav><a href="/">Home</a> <a href="/blog">Blog</a> <a href="/about">About</a></nav>
  <header><h1>Site Name</h1><p>Site tagline boilerplate that should be dropped.</p></header>
  <main>
    <article>
      <h1>The Great Article</h1>
      <p>An opening paragraph with a <a href="/relative/path">relative link</a>
         and an <a href="https://external.example/page">external link</a> to set the scene.</p>
      <h2>Section One</h2>
      <p>Some more prose here that continues the article body and gives readability
         enough signal to treat this region as the main content of the page.</p>
      <ul>
        <li>First bullet point</li>
        <li>Second bullet point</li>
      </ul>
      <blockquote>A memorable and sufficiently long pull-quote from the piece.</blockquote>
      <pre><code>fn main() { println!("hi"); }</code></pre>
      <table>
        <thead><tr><th>Name</th><th>Score</th></tr></thead>
        <tbody>
          <tr><td>Alpha</td><td>10</td></tr>
          <tr><td>Beta</td><td>20</td></tr>
        </tbody>
      </table>
      <p>A closing paragraph with an image <img src="/img/photo.png" alt="a photo">
         and yet more text to keep the article comfortably above the length threshold.</p>
    </article>
  </main>
  <footer><p>Copyright 2026 - footer boilerplate that must be stripped.</p></footer>
</body>
</html>"##;

    fn scrape_article(only_main: bool) -> ScrapeResult {
        scrape(
            ARTICLE,
            "https://site.example/articles/great",
            200,
            "text/html; charset=utf-8",
            only_main,
        )
    }

    // ---- Markdown structure survives -----------------------------------

    #[test]
    fn headings_survive() {
        let md = scrape_article(true).markdown;
        // At least one ATX heading is present (readability may re-level the H1).
        assert!(
            md.lines().any(|l| l.trim_start().starts_with('#')),
            "expected an ATX heading in:\n{md}"
        );
        assert!(
            md.contains("Section One"),
            "sub-heading text missing:\n{md}"
        );
    }

    #[test]
    fn links_survive_and_are_absolutized() {
        let md = scrape_article(true).markdown;
        // The relative link is absolutized against the page URL.
        assert!(
            md.contains("(https://site.example/relative/path)"),
            "relative link not absolutized:\n{md}"
        );
        // The external link is preserved verbatim.
        assert!(
            md.contains("(https://external.example/page)"),
            "external link missing:\n{md}"
        );
    }

    #[test]
    fn image_survives_and_is_absolutized() {
        let md = scrape_article(true).markdown;
        assert!(
            md.contains("![a photo](https://site.example/img/photo.png)"),
            "image not rendered/absolutized:\n{md}"
        );
    }

    #[test]
    fn list_survives() {
        let md = scrape_article(true).markdown;
        assert!(
            md.contains("First bullet point"),
            "list item missing:\n{md}"
        );
        assert!(
            md.contains("Second bullet point"),
            "list item missing:\n{md}"
        );
        // Rendered as a Markdown bullet (htmd uses `*` or `-`, followed by
        // spacing) — assert the marker precedes the item text.
        assert!(
            md.lines().any(|l| {
                let t = l.trim_start();
                (t.starts_with('*') || t.starts_with('-'))
                    && t.trim_start_matches(['*', '-', ' ']).starts_with("First")
            }),
            "bullet marker missing:\n{md}"
        );
    }

    #[test]
    fn code_block_survives() {
        let md = scrape_article(true).markdown;
        assert!(md.contains("```"), "fenced code block missing:\n{md}");
        assert!(
            md.contains(r#"println!("hi")"#),
            "code content missing:\n{md}"
        );
    }

    #[test]
    fn blockquote_survives() {
        let md = scrape_article(true).markdown;
        assert!(
            md.lines().any(|l| l.trim_start().starts_with('>')),
            "blockquote marker missing:\n{md}"
        );
    }

    #[test]
    fn gfm_table_survives() {
        let md = scrape_article(true).markdown;
        // Header row (htmd pads cells) + separator row + data.
        assert!(
            md.lines()
                .any(|l| l.contains('|') && l.contains("Name") && l.contains("Score")),
            "table header row missing:\n{md}"
        );
        assert!(
            md.lines().any(|l| l.contains('|')
                && l.trim_matches(['|', '-', ' ']).is_empty()
                && l.contains("---")),
            "table separator row missing:\n{md}"
        );
        assert!(md.contains("Alpha"), "table cell missing:\n{md}");
        assert!(md.contains("Beta"), "table cell missing:\n{md}");
    }

    // ---- Boilerplate is dropped under only_main_content ----------------

    #[test]
    fn nav_and_footer_dropped_under_main_content() {
        let md = scrape_article(true).markdown;
        assert!(
            !md.contains("footer boilerplate"),
            "footer should be stripped:\n{md}"
        );
        assert!(
            !md.contains("Site tagline boilerplate"),
            "header tagline should be stripped:\n{md}"
        );
        // The nav link labels should not survive as their own list of links.
        assert!(
            !md.contains("(https://site.example/blog)"),
            "nav links should be stripped:\n{md}"
        );
    }

    #[test]
    fn whole_body_kept_when_not_main_content() {
        let md = scrape(
            ARTICLE,
            "https://site.example/articles/great",
            200,
            "text/html",
            false,
        )
        .markdown;
        // With only_main_content=false, chrome is retained.
        assert!(
            md.contains("footer boilerplate"),
            "footer should be present in whole-body mode:\n{md}"
        );
        // And links are still absolutized on the whole-body path.
        assert!(
            md.contains("(https://site.example/blog)"),
            "nav link should be absolutized in whole-body mode:\n{md}"
        );
    }

    // ---- Metadata ------------------------------------------------------

    #[test]
    fn metadata_has_expected_keys() {
        let meta = scrape_article(true).metadata;
        assert_eq!(meta["title"], "The Great Article");
        assert_eq!(meta["description"], "An article about interesting things.");
        assert_eq!(meta["language"], "en");
        assert_eq!(meta["og:title"], "The Great Article (OG)");
        assert_eq!(meta["og:type"], "article");
        assert_eq!(meta["twitter:card"], "summary_large_image");
        assert_eq!(meta["viewport"], "width=device-width, initial-scale=1");
        assert_eq!(meta["theme-color"], "#ffffff");
        // canonical + favicon are absolutized against the page URL.
        assert_eq!(meta["canonical"], "https://site.example/articles/great");
        assert_eq!(meta["favicon"], "https://site.example/favicon.ico");
        // Synthetic keys.
        assert_eq!(meta["sourceURL"], "https://site.example/articles/great");
        assert_eq!(meta["url"], "https://site.example/articles/great");
        assert_eq!(meta["statusCode"], 200);
        assert_eq!(meta["contentType"], "text/html; charset=utf-8");
    }

    // ---- URL absolutization helper -------------------------------------

    #[test]
    fn absolutize_leaves_absolute_and_special_schemes() {
        let base = url::Url::parse("https://x.example/a/b").unwrap();
        assert_eq!(
            resolve_url(&base, "https://y.example/z"),
            "https://y.example/z"
        );
        assert_eq!(resolve_url(&base, "mailto:a@b.com"), "mailto:a@b.com");
        assert_eq!(resolve_url(&base, "#frag"), "#frag");
        assert_eq!(
            resolve_url(&base, "data:image/png;base64,AAAA"),
            "data:image/png;base64,AAAA"
        );
        // Relative + protocol-relative both resolve.
        assert_eq!(resolve_url(&base, "/c"), "https://x.example/c");
        assert_eq!(
            resolve_url(&base, "//cdn.example/x.js"),
            "https://cdn.example/x.js"
        );
        assert_eq!(resolve_url(&base, "d"), "https://x.example/a/d");
    }

    #[test]
    fn absolutize_urls_rewrites_href_and_src() {
        let html = r#"<a href="/rel">x</a><img src='sub/i.png'><a href="https://ok/z">y</a>"#;
        let out = absolutize_urls(html, "https://h.example/dir/page");
        assert!(out.contains(r#"href="https://h.example/rel""#), "{out}");
        assert!(
            out.contains(r#"src='https://h.example/dir/sub/i.png'"#),
            "{out}"
        );
        assert!(out.contains(r#"href="https://ok/z""#), "{out}");
    }

    #[test]
    fn absolutize_urls_noop_without_base() {
        let html = r#"<a href="/rel">x</a>"#;
        // Relative base → cannot resolve → unchanged.
        assert_eq!(absolutize_urls(html, "not a url"), html);
    }

    // ---- Degenerate inputs never panic ---------------------------------

    #[test]
    fn thin_spa_shell_still_returns_metadata() {
        let thin = r#"<!doctype html><html><head><title>App</title></head>
            <body><div id="root"></div><script src="/app.js"></script></body></html>"#;
        let res = scrape(thin, "https://spa.example/", 200, "text/html", true);
        // Metadata is always populated even when there is no article.
        assert_eq!(res.metadata["title"], "App");
        assert_eq!(res.metadata["statusCode"], 200);
        // Markdown may be near-empty; the helper flags it as thin.
        assert!(is_thin_content(&res.markdown, 50));
    }

    #[test]
    fn empty_and_garbage_never_panic() {
        let a = scrape("", "https://x/", 200, "text/html", true);
        assert_eq!(a.metadata["url"], "https://x/");
        let b = scrape("not html at all", "https://x/", 404, "text/plain", true);
        assert_eq!(b.metadata["statusCode"], 404);
    }
}
