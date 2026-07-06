//! # Content engine — URL → clean Markdown + metadata (Firecrawl-parity scrape).
//!
//! Pure and synchronous: HTML bytes in, [`ScrapeResult`] (Markdown + metadata
//! JSON) out. No I/O, no async, no global state — the same discipline as the
//! rest of `draco-static`.
//!
//! The pipeline mirrors Firecrawl's open-source HTML→Markdown path
//! (`apps/api/native/src/html.rs` in `github.com/firecrawl/firecrawl`, the Rust
//! N-API crate `@mendable/firecrawl-rs`) as closely as the Rust ecosystem
//! allows. It is four passes over the source HTML:
//!
//! 1. **DOM pre-processing** ([`preprocess_dom`]). We parse the document with
//!    [`scraper`] (html5ever) and, exactly like Firecrawl's `_transform_html_inner`:
//!    detach `head`/`meta`/`noscript`/`style`/`script` (plus `template`/`svg`/
//!    `iframe`/`object`/`embed` and comment nodes, which never carry article
//!    text); under `only_main_content` remove Firecrawl's 42
//!    `EXCLUDE_NON_MAIN_TAGS` boilerplate selectors (`header`/`footer`/`nav`/
//!    `aside` + `.header`/`.navbar`/`#footer`/`.sidebar`/`.modal`/`.ad`/
//!    `.social`/`.menu`/`.breadcrumbs`/`.cookie`/…), skipping any node that
//!    contains a `FORCE_INCLUDE_MAIN_TAGS` marker (`#main`, `.swoogo-*`);
//!    collapse each `<img srcset>` / `data-srcset` / `data-src` to its largest
//!    candidate; and absolutize every `img[src]` / `a[href]` against the
//!    document's base href. We additionally normalise code-block language hints
//!    (`lang-x` / `hljs x` → `language-x`) so the converter can infer the fenced
//!    block language.
//!
//! 2. **Readability fallback.** Firecrawl falls back to whole-content extraction
//!    when main-content stripping yields empty Markdown. We mirror that: when
//!    `only_main_content` stripping leaves the document thin, we run
//!    [`dom_smoothie`] (a Mozilla Readability port, configured to the
//!    `@mozilla/readability` defaults — `char_threshold: 500`, five top
//!    candidates, Readability candidate-selection) over the *original* HTML and
//!    convert whatever article body it recovers. If that also fails we keep the
//!    stripped body.
//!
//! 3. **HTML → Markdown** ([`html_to_markdown`]). [`htmd`] (a Turndown-style
//!    converter) renders the cleaned HTML with GFM tables, fenced code blocks
//!    (language inferred from `class="language-…"` on the `<code>`/`<pre>`),
//!    ATX headings, `-` bullet markers, `**`/`_` emphasis, `* * *` rules,
//!    verbatim `<pre>`, and GFM strikethrough (`<del>`/`<s>`/`<strike>` →
//!    `~~…~~`, via a custom rule since htmd has none) — matching Firecrawl's
//!    Turndown + `turndown-plugin-gfm`
//!    configuration.
//!
//! 4. **Markdown post-processing** ([`post_process_markdown`]). Mirrors
//!    Firecrawl's `post_process_markdown` + `removeBase64Images`: base64/`data:`
//!    images become `![alt](<Base64-Image-Removed>)`, multi-line link text gets
//!    a trailing `\` per wrapped line, `[Skip to Content](#…)` anchors are
//!    dropped, runs of 3+ blank lines collapse to one, and the result is
//!    trimmed.
//!
//! 5. **Metadata** ([`extract_metadata`]). Mirrors Firecrawl's
//!    `_extract_metadata`: `<title>`, `<html lang>` → `language`, favicon
//!    (`link[rel=icon]`/`rel*=icon`, absolutized), the canonical camelCase Open
//!    Graph / Twitter / `article:*` / Dublin Core keys, **and** every remaining
//!    `<meta name|property|itemprop>` flattened as `name → content` (so
//!    `og:*` / `twitter:*` / `description` / `viewport` also appear under their
//!    raw names). We add `sourceURL`, `url`, `statusCode` and `contentType`.

use scraper::{Html, Selector};
use serde_json::{Map, Value};

/// The output of [`scrape`]: clean Markdown plus a flat metadata object shaped
/// to match Firecrawl's `scrape` response.
#[derive(Debug, Clone)]
pub struct ScrapeResult {
    /// The main content (or whole body) rendered as GFM Markdown. Skeleton/
    /// placeholder lines (e.g. `Loading…`) are stripped from this output.
    pub markdown: String,
    /// A flat JSON object of page metadata (see the module docs for the keys).
    pub metadata: Value,
    /// `true` when the source looked like an **incomplete client-side render** —
    /// a skeleton screen whose real content had not yet loaded (many repeated
    /// `Loading…` placeholders). Length-independent: a chrome-heavy shell can be
    /// well over the thin-content threshold yet still be a skeleton. The caller
    /// uses this (alongside [`is_thin_content`]) to decide whether to escalate to
    /// the render-then-Markdown pass. Computed *before* the placeholders are
    /// stripped from [`ScrapeResult::markdown`].
    pub incomplete: bool,
}

/// Scrape `html` into Markdown + metadata.
///
/// * `url` — the page URL, used both to absolutize relative links/images and as
///   the `sourceURL` / `url` metadata values.
/// * `status` — the HTTP status of the fetch, surfaced as `statusCode`.
/// * `content_type` — the response `Content-Type`, surfaced as `contentType`.
/// * `only_main_content` — when `true` (the default scrape mode) boilerplate
///   selectors are stripped to the article body; when `false` the whole
///   document (minus scripts/styles) is converted.
///
/// Never panics: a readability failure or an `htmd` error degrades to the
/// cleaned-body conversion (or an empty string), and metadata extraction is
/// best-effort per field.
pub fn scrape(
    html: &str,
    url: &str,
    status: u16,
    content_type: &str,
    only_main_content: bool,
) -> ScrapeResult {
    let metadata = extract_metadata(html, url, status, content_type);

    // Pass 1: DOM pre-processing — strip chrome/scripts, absolutize URLs,
    // normalise code languages and strikethrough (Firecrawl's transform_html).
    let cleaned = preprocess_dom(html, url, only_main_content);
    let mut markdown = post_process_markdown(&html_to_markdown(&cleaned));

    // Pass 2: Readability fallback. Firecrawl falls back to full-content
    // extraction when main-content stripping produces empty Markdown; we go one
    // better and try Readability over the *original* document first (it often
    // recovers a clean article from pages our selector list can't isolate),
    // then fall back to the un-stripped body.
    if only_main_content && is_thin_content(&markdown, MIN_MAIN_CONTENT_CHARS) {
        if let Some(article) = readability_html(html, url) {
            let alt = post_process_markdown(&html_to_markdown(&article));
            if alt.chars().filter(|c| !c.is_whitespace()).count()
                > markdown.chars().filter(|c| !c.is_whitespace()).count()
            {
                markdown = alt;
            }
        }
        if is_thin_content(&markdown, MIN_MAIN_CONTENT_CHARS) {
            let whole = preprocess_dom(html, url, false);
            let alt = post_process_markdown(&html_to_markdown(&whole));
            if alt.chars().filter(|c| !c.is_whitespace()).count()
                > markdown.chars().filter(|c| !c.is_whitespace()).count()
            {
                markdown = alt;
            }
        }
    }

    // Skeleton detection + cleanup. A client-rendered page often serializes as a
    // skeleton screen — repeated `Loading…` placeholders where content will
    // mount. We (a) flag it as an incomplete render so the caller can escalate to
    // the render-then-Markdown pass even though the chrome makes it non-thin, and
    // (b) strip the placeholder lines so `Loading…` noise never reaches the user
    // regardless of whether the render pass runs or succeeds. Detection runs on
    // the pre-strip Markdown.
    let incomplete = is_incomplete_render(&markdown);
    let markdown = strip_incomplete_markers(&markdown);

    ScrapeResult {
        markdown,
        metadata,
        incomplete,
    }
}

/// Minimum number of skeleton placeholder lines before a page is judged an
/// incomplete client-side render. One or two `Loading…` labels are normal (a lazy
/// widget on an otherwise-rendered page); a wall of them is a skeleton screen.
const SKELETON_MIN_MARKERS: usize = 3;

/// Does this Markdown look like an **incomplete client-side render** — a skeleton
/// screen whose real content has not loaded yet?
///
/// Detects repeated placeholder lines (`Loading…`, `Loading...`, `Please wait`)
/// that frameworks emit while data is in flight. Length-independent by design: a
/// page like a large retail homepage carries enough nav/promo chrome to clear the
/// thin-content bar while its actual product rails are still `Loading…`. Returns
/// `true` once at least [`SKELETON_MIN_MARKERS`] such lines are present.
pub fn is_incomplete_render(markdown: &str) -> bool {
    markdown.lines().filter(|l| is_skeleton_line(l)).count() >= SKELETON_MIN_MARKERS
}

/// Remove skeleton placeholder lines (`Loading…` etc.) from `markdown`, then
/// collapse the blank runs their removal leaves behind. Always safe to run: a
/// line is only removed when, stripped of Markdown structure and emphasis, it is
/// *exactly* a known placeholder token — never when the word merely appears
/// inside real text (`Loading dock tours`) or an image (`![loading](spinner.gif)`).
pub fn strip_incomplete_markers(markdown: &str) -> String {
    let kept: Vec<&str> = markdown.lines().filter(|l| !is_skeleton_line(l)).collect();
    collapse_blank_lines(&kept.join("\n"))
}

/// Is `line`, once stripped of leading Markdown structure (heading `#`, list
/// markers, blockquote `>`, ordered-list `N.`) and surrounding emphasis/backticks,
/// *exactly* a skeleton placeholder token (case-insensitive, trailing dots/ellipsis
/// ignored)? Anchored to the whole line so real prose containing the word is safe.
fn is_skeleton_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    // Strip leading Markdown structural characters (heading/list/quote markers,
    // ordered-list digits + dot, and whitespace).
    let core = t.trim_start_matches(|c: char| {
        c == '#'
            || c == '>'
            || c == '-'
            || c == '*'
            || c == '+'
            || c == '.'
            || c == ' '
            || c == '\t'
            || c.is_ascii_digit()
    });
    // Strip surrounding emphasis / code ticks / whitespace.
    let core = core.trim().trim_matches(['*', '_', '`', ' ', '\t']);
    // Ignore trailing dots / unicode ellipsis.
    let core = core.trim_end_matches(['.', '\u{2026}', ' ']);
    let lc = core.to_ascii_lowercase();
    matches!(lc.as_str(), "loading" | "please wait" | "loading content")
}

/// Below this many non-whitespace characters, main-content stripping is treated
/// as having found essentially nothing and the Readability / whole-body
/// fallback kicks in. Chosen to line up with Mozilla Readability's own
/// `charThreshold` sensibility while staying well under a real article.
const MIN_MAIN_CONTENT_CHARS: usize = 200;

/// Heuristic: does the rendered Markdown look like a thin SPA shell — i.e. did
/// extraction essentially find no article? Used by the caller to decide whether
/// to note that an SPA render pass would help. Kept here (next to the pipeline)
/// so the threshold lives with the extraction logic.
///
/// Returns `true` when the rendered Markdown has less than `min_chars` of
/// non-whitespace content.
pub fn is_thin_content(markdown: &str, min_chars: usize) -> bool {
    markdown.chars().filter(|c| !c.is_whitespace()).count() < min_chars
}

/// Merge the original shell's `<head>` with a hydrated document's `<body>` into a
/// single document suitable for [`scrape`].
///
/// This is the join point of the render-then-Markdown escalation. The Tier 2
/// isolate hydrates a client-rendered SPA and serializes the live DOM
/// (`document.documentElement.outerHTML`) — that markup carries the *content*
/// the framework mounted, but a near-empty `<head>` (the isolate does not
/// materialize the shell's metadata). The originally fetched shell is the
/// reverse: a rich `<head>` (title, Open Graph, canonical, `<base>`) wrapped
/// around an empty mount point. Splicing the shell head onto the hydrated body
/// gives [`scrape`] one coherent document that yields both faithful metadata and
/// the hydrated article — mirroring how a real browser render feeds the same
/// HTML→Markdown transform.
///
/// The shell head is preferred verbatim (so `<base href>` and SEO tags survive);
/// the hydrated body replaces the shell's empty one. If either part cannot be
/// located the function degrades gracefully: a missing shell head yields the
/// hydrated document's own head (or none), and a hydrated document with no
/// `<body>` is treated as body-only markup.
pub fn merge_rendered_document(shell_html: &str, rendered_html: &str) -> String {
    let head = slice_element(shell_html, "head")
        .or_else(|| slice_element(rendered_html, "head"))
        .unwrap_or_default();

    // The hydrated content: prefer the rendered `<body>…</body>`; if the isolate
    // emitted no body wrapper, treat the whole serialized string as body markup.
    let body = slice_element(rendered_html, "body")
        .map(str::to_string)
        .unwrap_or_else(|| format!("<body>{rendered_html}</body>"));

    format!("<!doctype html><html>{head}{body}</html>")
}

/// Return the full `<tag …>…</tag>` span (opening tag through closing tag) for
/// the first occurrence of `tag`, case-insensitively. Byte-oriented and
/// allocation-light: it lowercases only for matching and slices the original.
/// Returns `None` when the element (or its closing tag) is absent.
fn slice_element<'a>(html: &'a str, tag: &str) -> Option<&'a str> {
    let lower = html.to_ascii_lowercase();
    let open_needle = format!("<{tag}");
    let close_needle = format!("</{tag}>");

    let open = lower.find(&open_needle)?;
    // The opening tag must end in `>` (guards against `<bodyfoo`); find it.
    let after_open = &lower[open + open_needle.len()..];
    // The next byte after the tag name must be `>`, whitespace, or `/`.
    let first = after_open.as_bytes().first().copied();
    if !matches!(
        first,
        Some(b'>')
            | Some(b'/')
            | Some(b' ')
            | Some(b'\t')
            | Some(b'\n')
            | Some(b'\r')
            | Some(b'\x0c')
    ) {
        return None;
    }
    let close = lower[open..].find(&close_needle)? + open + close_needle.len();
    Some(&html[open..close])
}

// ===================================================================
// Pass 1 — DOM pre-processing (Firecrawl `transform_html` parity)
// ===================================================================

/// Elements that never carry article text and are always removed before
/// conversion. Firecrawl detaches `head`/`meta`/`noscript`/`style`/`script`;
/// we additionally drop `template`/`svg`/`iframe`/`object`/`embed`, which are
/// non-textual and would otherwise leak stray attributes or alt text.
const ALWAYS_STRIP: [&str; 10] = [
    "head", "meta", "noscript", "style", "script", "template", "svg", "iframe", "object", "embed",
];

/// Firecrawl's `EXCLUDE_NON_MAIN_TAGS`: boilerplate regions removed when
/// `only_main_content` is on. Kept byte-for-byte in sync with
/// `apps/api/native/src/html.rs` so our main-content view matches theirs.
const EXCLUDE_NON_MAIN_TAGS: [&str; 42] = [
    "header",
    "footer",
    "nav",
    "aside",
    ".header",
    ".top",
    ".navbar",
    "#header",
    ".footer",
    ".bottom",
    "#footer",
    ".sidebar",
    ".side",
    ".aside",
    "#sidebar",
    ".modal",
    ".popup",
    "#modal",
    ".overlay",
    ".ad",
    ".ads",
    ".advert",
    "#ad",
    ".lang-selector",
    ".language",
    "#language-selector",
    ".social",
    ".social-media",
    ".social-links",
    "#social",
    ".menu",
    ".navigation",
    "#nav",
    ".breadcrumbs",
    "#breadcrumbs",
    ".share",
    "#share",
    ".widget",
    "#widget",
    ".cookie",
    "#cookie",
    ".fc-decoration",
];

/// Firecrawl's `FORCE_INCLUDE_MAIN_TAGS`: a boilerplate node that *contains* one
/// of these is kept even if it matches an exclude selector (e.g. a `<header>`
/// that wraps `#main`). Kept in sync with the reference.
const FORCE_INCLUDE_MAIN_TAGS: [&str; 13] = [
    "#main",
    ".swoogo-cols",
    ".swoogo-text",
    ".swoogo-table-div",
    ".swoogo-space",
    ".swoogo-alert",
    ".swoogo-sponsors",
    ".swoogo-title",
    ".swoogo-tabs",
    ".swoogo-logo",
    ".swoogo-image",
    ".swoogo-button",
    ".swoogo-agenda",
];

/// Parse `html`, strip chrome/scripts (and, under `only_main_content`, the
/// non-main boilerplate), collapse responsive images, absolutize URLs, and
/// normalise code-language + strikethrough markup. Returns the cleaned inner
/// `<body>` HTML ready for [`html_to_markdown`].
///
/// Operates on a real DOM (via [`scraper`]) so removals are structural, exactly
/// like Firecrawl's `kuchikiki`-based `_transform_html_inner`.
fn preprocess_dom(html: &str, url: &str, only_main_content: bool) -> String {
    let mut document = Html::parse_document(html);
    let base = base_href(&document, url);

    // Always strip non-content elements + comment nodes.
    detach_matching(&mut document, &ALWAYS_STRIP);
    detach_comment_nodes(&mut document);

    // Under only_main_content, drop the boilerplate regions unless they contain
    // a force-include marker.
    if only_main_content {
        detach_non_main(&mut document);
    }

    // Resolve responsive images to their largest candidate, then absolutize
    // img[src] / a[href] against the base href.
    collapse_srcset_images(&mut document);
    absolutize_attrs(&mut document, base.as_deref());

    // Normalise code-language hints the converter can't otherwise infer.
    // (Strikethrough is handled by a custom converter rule, not here.)
    normalize_code_languages(&mut document);

    body_inner_html(&document)
}

/// Cleaned, absolutized HTML of the page — the `html` scrape format.
///
/// This is exactly the DOM pre-processing that feeds the Markdown transform
/// (scripts/styles/chrome stripped, responsive images collapsed, `a[href]`/
/// `img[src]` absolutized), serialized back to HTML instead of converted to
/// Markdown. With `only_main_content` the boilerplate regions are dropped to the
/// article body; otherwise the whole cleaned document is returned.
pub fn clean_html(html: &str, url: &str, only_main_content: bool) -> String {
    preprocess_dom(html, url, only_main_content)
}

/// Every absolutized `<a href>` on the page, de-duplicated in document order —
/// the `links` scrape format. Hrefs are resolved against `<base href>` (or the
/// page URL); `javascript:`/`mailto:`/`tel:` and empty/fragment-only hrefs are
/// skipped. Never panics: an unparseable href is simply dropped.
pub fn extract_links(html: &str, url: &str) -> Vec<String> {
    let document = Html::parse_document(html);
    let base = base_href(&document, url);
    let base_url = base.as_deref().and_then(|b| url::Url::parse(b).ok());
    let Ok(sel) = Selector::parse("a[href]") else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for el in document.select(&sel) {
        let Some(href) = el.value().attr("href") else {
            continue;
        };
        let href = href.trim();
        if href.is_empty() || href.starts_with('#') {
            continue;
        }
        let lower = href.to_ascii_lowercase();
        if lower.starts_with("javascript:")
            || lower.starts_with("mailto:")
            || lower.starts_with("tel:")
        {
            continue;
        }
        // Absolutize against the base when possible; keep already-absolute URLs.
        let resolved = match &base_url {
            Some(b) => b.join(href).map(|u| u.to_string()).unwrap_or_default(),
            None => href.to_string(),
        };
        if resolved.is_empty() {
            continue;
        }
        if seen.insert(resolved.clone()) {
            out.push(resolved);
        }
    }
    out
}

/// Detach every element matching any selector in `selectors`.
fn detach_matching(document: &mut Html, selectors: &[&str]) {
    let mut ids = Vec::new();
    for sel in selectors {
        if let Ok(selector) = Selector::parse(sel) {
            for el in document.select(&selector) {
                ids.push(el.id());
            }
        }
    }
    for id in ids {
        if let Some(mut node) = document.tree.get_mut(id) {
            node.detach();
        }
    }
}

/// Detach the boilerplate regions (Firecrawl's `EXCLUDE_NON_MAIN_TAGS`),
/// skipping any node that contains a `FORCE_INCLUDE_MAIN_TAGS` marker — the
/// exact `:not(:has(marker))` filter Firecrawl applies.
fn detach_non_main(document: &mut Html) {
    // Pre-parse the force-include selectors once.
    let force: Vec<Selector> = FORCE_INCLUDE_MAIN_TAGS
        .iter()
        .filter_map(|s| Selector::parse(s).ok())
        .collect();

    let mut ids = Vec::new();
    for sel in EXCLUDE_NON_MAIN_TAGS {
        let Ok(selector) = Selector::parse(sel) else {
            continue;
        };
        for el in document.select(&selector) {
            // Keep the node if it contains any force-include marker.
            let protected = force.iter().any(|f| el.select(f).next().is_some());
            if !protected {
                ids.push(el.id());
            }
        }
    }
    for id in ids {
        if let Some(mut node) = document.tree.get_mut(id) {
            node.detach();
        }
    }
}

/// Detach every HTML comment node (`<!-- … -->`). Comments never carry article
/// text but can hold conditional-comment markup or tracking snippets.
fn detach_comment_nodes(document: &mut Html) {
    let ids: Vec<_> = document
        .tree
        .nodes()
        .filter(|n| n.value().is_comment())
        .map(|n| n.id())
        .collect();
    for id in ids {
        if let Some(mut node) = document.tree.get_mut(id) {
            node.detach();
        }
    }
}

/// Resolve the document's base href: `<base href>` joined against `url`, or
/// `url` itself. Returns `None` when `url` is not an absolute base (nothing to
/// resolve relative links against).
fn base_href(document: &Html, url: &str) -> Option<String> {
    let base_url = url::Url::parse(url).ok()?;
    if let Ok(sel) = Selector::parse("base[href]") {
        if let Some(el) = document.select(&sel).next() {
            if let Some(href) = el.value().attr("href") {
                if let Ok(joined) = base_url.join(href.trim()) {
                    return Some(joined.to_string());
                }
            }
        }
    }
    Some(base_url.to_string())
}

/// For each `<img>`, resolve the largest responsive candidate into `src`,
/// mirroring Firecrawl: prefer `srcset` (then `data-srcset`), pick the biggest
/// declared width/density; if every candidate is a density (`x`) descriptor,
/// also consider the existing `src`. When no usable `srcset` exists, fall back
/// to `data-src` (the common single-URL lazy-load case).
fn collapse_srcset_images(document: &mut Html) {
    let Ok(sel) = Selector::parse("img") else {
        return;
    };
    let ids: Vec<_> = document.select(&sel).map(|el| el.id()).collect();
    for id in ids {
        // Read the candidate attributes.
        let (srcset, data_srcset, data_src, cur_src) = {
            let Some(node) = document.tree.get(id) else {
                continue;
            };
            let Some(el) = node.value().as_element() else {
                continue;
            };
            (
                el.attr("srcset").map(str::to_string),
                el.attr("data-srcset").map(str::to_string),
                el.attr("data-src").map(str::to_string),
                el.attr("src").map(str::to_string),
            )
        };

        let chosen = if let Some(set) = srcset
            .filter(|s| !s.trim().is_empty())
            .or(data_srcset.filter(|s| !s.trim().is_empty()))
        {
            largest_srcset_url(&set, cur_src.as_deref())
        } else {
            data_src.filter(|s| !s.trim().is_empty())
        };

        if let Some(new_src) = chosen {
            set_attr(document, id, "src", &new_src);
        }
    }
}

/// Given a `srcset` value, return its largest candidate URL. If every candidate
/// carries a density (`x`) descriptor, `current_src` (if any) is folded in as a
/// `1x` candidate — matching Firecrawl's behaviour for `<img src srcset>`.
fn largest_srcset_url(srcset: &str, current_src: Option<&str>) -> Option<String> {
    struct Cand {
        url: String,
        size: f64,
        is_x: bool,
    }
    let mut cands: Vec<Cand> = srcset
        .split(',')
        .filter_map(|part| {
            let tok: Vec<&str> = part.split_whitespace().collect();
            if tok.is_empty() {
                return None;
            }
            let last = *tok.last().unwrap();
            let (desc, used) = if tok.len() > 1 && (last.ends_with('x') || last.ends_with('w')) {
                (last, true)
            } else {
                ("1x", false)
            };
            let num: f64 = desc[..desc.len() - 1].parse().ok()?;
            let url = if used {
                tok[..tok.len() - 1].join(" ")
            } else {
                tok.join(" ")
            };
            Some(Cand {
                url,
                size: num,
                is_x: desc.ends_with('x'),
            })
        })
        .collect();

    if cands.iter().all(|c| c.is_x) {
        if let Some(src) = current_src.filter(|s| !s.trim().is_empty()) {
            cands.push(Cand {
                url: src.to_string(),
                size: 1.0,
                is_x: true,
            });
        }
    }

    cands.sort_by(|a, b| {
        b.size
            .partial_cmp(&a.size)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    cands.into_iter().next().map(|c| c.url)
}

/// Absolutize every `img[src]` and `a[href]` against `base`. No-op when `base`
/// is `None`. Values that don't resolve (fragments, `mailto:`, `data:`, already
/// absolute) are left untouched by [`resolve_url`].
fn absolutize_attrs(document: &mut Html, base: Option<&str>) {
    let Some(base) = base else { return };
    let Ok(base_url) = url::Url::parse(base) else {
        return;
    };

    for (selector, attr) in [("img[src]", "src"), ("a[href]", "href")] {
        let Ok(sel) = Selector::parse(selector) else {
            continue;
        };
        let ids: Vec<_> = document.select(&sel).map(|el| el.id()).collect();
        for id in ids {
            let old = {
                let Some(node) = document.tree.get(id) else {
                    continue;
                };
                match node.value().as_element().and_then(|e| e.attr(attr)) {
                    Some(v) => v.to_string(),
                    None => continue,
                }
            };
            let resolved = resolve_url(&base_url, &old);
            if resolved != old {
                set_attr(document, id, attr, &resolved);
            }
        }
    }
}

/// Normalise code-language hints so the Markdown converter can infer the fenced
/// block language. htmd only understands `class="language-x"`; many sites ship
/// `class="lang-x"` or highlight.js `class="hljs x"`. We rewrite the `class` on
/// every `<code>` / `<pre>` so a language token is exposed as `language-x`.
fn normalize_code_languages(document: &mut Html) {
    let Ok(sel) = Selector::parse("pre, code") else {
        return;
    };
    let ids: Vec<_> = document.select(&sel).map(|el| el.id()).collect();
    for id in ids {
        let class = {
            let Some(node) = document.tree.get(id) else {
                continue;
            };
            match node.value().as_element().and_then(|e| e.attr("class")) {
                Some(c) => c.to_string(),
                None => continue,
            }
        };
        if let Some(normalized) = normalize_class_languages(&class) {
            set_attr(document, id, "class", &normalized);
        }
    }
}

/// If `class` carries a language hint that isn't already `language-*`, return a
/// rewritten class string that adds `language-<lang>`. Recognises `lang-<x>`,
/// highlight.js `hljs <x>`, and bare well-known language tokens sitting next to
/// `hljs`. Returns `None` when no rewrite is warranted.
fn normalize_class_languages(class: &str) -> Option<String> {
    let tokens: Vec<&str> = class.split_whitespace().collect();
    // Already has an explicit language- token → nothing to do.
    if tokens.iter().any(|t| t.starts_with("language-")) {
        return None;
    }

    // `lang-x` → `language-x`.
    if let Some(lang) = tokens.iter().find_map(|t| t.strip_prefix("lang-")) {
        if !lang.is_empty() {
            return Some(format!("{class} language-{lang}"));
        }
    }

    // highlight.js: `hljs <lang>` (or `<lang> hljs`). The language is the first
    // non-`hljs`, non-empty token.
    if tokens.contains(&"hljs") {
        if let Some(lang) = tokens.iter().find(|t| **t != "hljs" && !t.is_empty()) {
            return Some(format!("{class} language-{lang}"));
        }
    }

    None
}

/// Set (or insert) attribute `name` = `value` on the element at `id`. Mutates
/// the DOM in place through `scraper`'s public `Node`/`Element` fields.
fn set_attr(document: &mut Html, id: ego_tree::NodeId, name: &str, value: &str) {
    let Some(mut node) = document.tree.get_mut(id) else {
        return;
    };
    if let scraper::Node::Element(el) = node.value() {
        let qname =
            html5ever::QualName::new(None, html5ever::ns!(), html5ever::LocalName::from(name));
        if let Some(slot) = el.attrs.iter_mut().find(|(k, _)| k.local.as_ref() == name) {
            slot.1 = value.into();
        } else {
            el.attrs.push((qname, value.into()));
        }
    }
}

/// Serialize the inner HTML of `<body>` (or the whole document if there is no
/// `<body>`). This is the cleaned HTML handed to the Markdown converter.
fn body_inner_html(document: &Html) -> String {
    if let Ok(sel) = Selector::parse("body") {
        if let Some(body) = document.select(&sel).next() {
            return body.inner_html();
        }
    }
    document.root_element().inner_html()
}

// ===================================================================
// Pass 2 — Readability fallback (dom_smoothie)
// ===================================================================

/// Run readability over the *original* document and return its main-content
/// HTML, or `None` if extraction fails. Configured to the `@mozilla/readability`
/// defaults Firecrawl's Readability path relied on: `char_threshold: 500`, five
/// top candidates, Readability-style candidate selection. Used only as a
/// fallback when selector-based main-content stripping comes up thin.
fn readability_html(html: &str, url: &str) -> Option<String> {
    use dom_smoothie::{CandidateSelectMode, Config, Readability, TextMode};

    let cfg = Config {
        // Mirror @mozilla/readability defaults (the values dom_smoothie already
        // defaults to, made explicit so intent is clear and stable).
        char_threshold: 500,
        n_top_candidates: 5,
        candidate_select_mode: CandidateSelectMode::Readability,
        // We render Markdown from the `content` HTML ourselves; the text mode
        // only affects `text_content`, so any value is fine.
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

// ===================================================================
// Pass 3 — HTML → Markdown (htmd, Turndown-equivalent config)
// ===================================================================

/// Convert cleaned content HTML to Markdown with `htmd`, configured to match
/// Firecrawl's Turndown + `turndown-plugin-gfm` output: ATX headings, fenced
/// code blocks (language from `class="language-…"`), `-` bullet markers, `**` /
/// `_` emphasis, `* * *` thematic breaks, and GFM tables. `<pre>` content is
/// preserved verbatim. On an `htmd` error (should not happen for parseable
/// HTML) we degrade to the empty string rather than panicking.
fn html_to_markdown(content_html: &str) -> String {
    use htmd::options::{
        BrStyle, BulletListMarker, CodeBlockFence, CodeBlockStyle, HeadingStyle, HrStyle,
        LinkStyle, Options,
    };

    let options = Options {
        heading_style: HeadingStyle::Atx,
        hr_style: HrStyle::Asterisks, // `* * *`, matching Turndown's default.
        br_style: BrStyle::TwoSpaces,
        link_style: LinkStyle::Inlined,
        code_block_style: CodeBlockStyle::Fenced,
        code_block_fence: CodeBlockFence::Backticks,
        bullet_list_marker: BulletListMarker::Dash, // Firecrawl's shipping output.
        ..Default::default()
    };

    let converter = htmd::HtmlToMarkdown::builder()
        .options(options)
        // Belt-and-suspenders: these are already detached in preprocess_dom,
        // but skipping them here guards the Readability-fallback path too.
        .skip_tags(vec!["script", "style", "noscript", "iframe"])
        // GFM strikethrough: htmd has no `<del>/<s>/<strike>` rule (Turndown
        // gets it from `turndown-plugin-gfm`), so wrap the already-converted
        // inner content in `~~…~~`. Returning the string from a handler inserts
        // it verbatim, avoiding htmd's leading-`~` text escaping.
        .add_handler(vec!["del", "s", "strike"], strikethrough_handler)
        .build();
    converter.convert(content_html).unwrap_or_default()
}

/// htmd handler for `<del>`/`<s>`/`<strike>`: render the inner Markdown wrapped
/// in GFM `~~…~~`. Empty elements collapse to nothing.
fn strikethrough_handler(element: htmd::Element) -> Option<String> {
    let content = element.content.trim();
    if content.is_empty() {
        None
    } else {
        Some(format!("~~{content}~~"))
    }
}

// ===================================================================
// Pass 4 — Markdown post-processing (Firecrawl `post_process_markdown`)
// ===================================================================

/// Post-process rendered Markdown to match Firecrawl's `post_process_markdown`
/// (+ `removeBase64Images`), then normalise whitespace:
///
/// 1. base64/`data:` images → `![alt](<Base64-Image-Removed>)`;
/// 2. multi-line link *text* gets a trailing `\` on each wrapped line so the
///    link survives (Firecrawl's `processMultiLineLinks`);
/// 3. `[Skip to Content](#…)` anchors are removed (case-insensitive);
/// 4. runs of 3+ blank lines collapse to a single blank line;
/// 5. leading/trailing whitespace is trimmed.
fn post_process_markdown(markdown: &str) -> String {
    let s = remove_base64_images(markdown);
    let s = process_multiline_links(&s);
    let s = remove_skip_to_content_links(&s);
    let s = collapse_blank_lines(&s);
    s.trim().to_string()
}

/// Replace `![alt](data:…;base64,…)` (and any `data:` image URL) with
/// `![alt](<Base64-Image-Removed>)`, keeping the alt text. Mirrors Firecrawl's
/// `removeBase64Images` regex without pulling in a regex dependency.
fn remove_base64_images(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let bytes = markdown.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for the image-link start `![`.
        if bytes[i] == b'!' && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            if let Some((alt_end, url_start)) = image_alt_span(markdown, i) {
                // url_start points just past the `(`.
                if let Some(close_rel) = markdown[url_start..].find(')') {
                    let url = &markdown[url_start..url_start + close_rel];
                    if url.trim_start().to_ascii_lowercase().starts_with("data:") {
                        // Emit `![alt](<Base64-Image-Removed>)`.
                        out.push_str(&markdown[i..alt_end]); // `![alt]`
                        out.push_str("(<Base64-Image-Removed>)");
                        i = url_start + close_rel + 1;
                        continue;
                    }
                }
            }
        }
        let ch_len = utf8_char_len(bytes[i]);
        out.push_str(&markdown[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// For an image link starting at `start` (`![`), return `(index_after_alt_close,
/// index_after_open_paren)` when the shape is `![alt](`. `None` otherwise.
fn image_alt_span(s: &str, start: usize) -> Option<(usize, usize)> {
    let after_bracket = start + 2; // past `![`
    let rel_close = s[after_bracket..].find(']')?;
    let alt_close = after_bracket + rel_close; // index of `]`
    let after_alt = alt_close + 1;
    // Next char must be `(`.
    if s.as_bytes().get(after_alt) != Some(&b'(') {
        return None;
    }
    Some((after_alt, after_alt + 1))
}

/// Firecrawl's `processMultiLineLinks`: while inside link *text* (unbalanced
/// `[`), a newline is emitted as `\` + newline so a wrapped link label doesn't
/// break the Markdown link.
fn process_multiline_links(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut link_open: usize = 0;
    for ch in markdown.chars() {
        match ch {
            '[' => link_open += 1,
            ']' => link_open = link_open.saturating_sub(1),
            _ => {}
        }
        if link_open > 0 && ch == '\n' {
            out.push('\\');
            out.push('\n');
        } else {
            out.push(ch);
        }
    }
    out
}

/// Firecrawl's `removeSkipToContentLinks`: drop `[Skip to Content](#…)` anchors
/// (case-insensitive on the label), which are accessibility jump-links, not
/// content.
fn remove_skip_to_content_links(markdown: &str) -> String {
    const LABEL: &str = "Skip to Content";
    let bytes = markdown.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0;

    'outer: while i < len {
        if bytes[i] == b'[' {
            let label_start = i + 1;
            let label_end = label_start + LABEL.len();
            if label_end <= len
                && markdown[label_start..label_end].eq_ignore_ascii_case(LABEL)
                && label_end + 2 < len
                && bytes[label_end] == b']'
                && bytes[label_end + 1] == b'('
                && bytes[label_end + 2] == b'#'
            {
                // Skip through the closing `)`.
                let mut j = label_end + 3;
                while j < len {
                    let ch = markdown[j..].chars().next().unwrap();
                    if ch == ')' {
                        i = j + ch.len_utf8();
                        continue 'outer;
                    }
                    j += ch.len_utf8();
                }
            }
        }
        let ch = markdown[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Collapse any run of 3+ consecutive newlines (i.e. 2+ blank lines) down to a
/// single blank line (`\n\n`), so paragraph spacing is uniform.
fn collapse_blank_lines(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut newline_run = 0usize;
    for ch in markdown.chars() {
        if ch == '\n' {
            newline_run += 1;
            if newline_run <= 2 {
                out.push('\n');
            }
        } else {
            newline_run = 0;
            out.push(ch);
        }
    }
    out
}

// ===================================================================
// URL resolution helpers
// ===================================================================

/// Resolve a single attribute value against `base`. Leaves already-absolute,
/// fragment, and non-navigational (`mailto:`/`javascript:`/`data:`/`tel:`) URIs
/// untouched.
fn resolve_url(base: &url::Url, value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return value.to_string();
    }
    let lower = trimmed.to_ascii_lowercase();
    if trimmed.starts_with('#')
        || lower.starts_with("mailto:")
        || lower.starts_with("javascript:")
        || lower.starts_with("data:")
        || lower.starts_with("tel:")
        || lower.starts_with("blob:")
        || has_scheme(trimmed)
    {
        return value.to_string();
    }
    match base.join(trimmed) {
        Ok(abs) => abs.to_string(),
        Err(_) => value.to_string(),
    }
}

/// Does `s` start with an absolute-URL scheme (`scheme://` or `scheme:`)?
/// Protocol-relative `//host/path` is treated as *needing* resolution (it picks
/// up the base scheme), so it is NOT considered to already have a scheme.
fn has_scheme(s: &str) -> bool {
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

/// Absolutize a single URL against `base`, returning the input unchanged if the
/// base is unusable or resolution fails.
fn absolutize_one(base: &str, value: &str) -> String {
    match url::Url::parse(base) {
        Ok(b) => resolve_url(&b, value),
        Err(_) => value.to_string(),
    }
}

// ===================================================================
// Pass 5 — Metadata extraction (Firecrawl `_extract_metadata` parity)
// ===================================================================

/// The canonical Open Graph / `article:*` / Dublin Core keys Firecrawl emits
/// under camelCase names, paired with the `meta` selector that feeds each. The
/// first matching element wins (Firecrawl reads `.attr("content")`, i.e. first).
const NAMED_META: [(&str, &str); 26] = [
    ("og:title", "ogTitle"),
    ("og:description", "ogDescription"),
    ("og:url", "ogUrl"),
    ("og:image", "ogImage"),
    ("og:audio", "ogAudio"),
    ("og:determiner", "ogDeterminer"),
    ("og:locale", "ogLocale"),
    ("og:site_name", "ogSiteName"),
    ("og:video", "ogVideo"),
    ("article:section", "articleSection"),
    ("article:tag", "articleTag"),
    ("article:published_time", "publishedTime"),
    ("article:modified_time", "modifiedTime"),
    ("dcterms.keywords", "dcTermsKeywords"),
    ("dc.description", "dcDescription"),
    ("dc.subject", "dcSubject"),
    ("dcterms.subject", "dcTermsSubject"),
    ("dcterms.audience", "dcTermsAudience"),
    ("dc.type", "dcType"),
    ("dcterms.type", "dcTermsType"),
    ("dc.date", "dcDate"),
    ("dc.date.created", "dcDateCreated"),
    ("dcterms.created", "dcTermsCreated"),
    ("keywords", "keywords"),
    ("robots", "robots"),
    ("description", "description"),
];

/// Build the flat metadata object, matching Firecrawl's `_extract_metadata`
/// shape. Best-effort per field; the four synthetic keys (`sourceURL`, `url`,
/// `statusCode`, `contentType`) are always present.
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

    // Canonical camelCase OG/article/DC keys + description/keywords/robots.
    // Each reads by property first, then name (og:* live on `property`, dc.*/
    // article:*/description/keywords/robots on `name`), mirroring Firecrawl.
    for (meta_key, out_key) in NAMED_META {
        if let Some(content) = meta_content(&document, meta_key) {
            map.entry(out_key.to_string())
                .or_insert_with(|| Value::String(content));
        }
    }

    // og:locale:alternate → array (Firecrawl emits this as a list).
    if let Ok(sel) = Selector::parse(r#"meta[property="og:locale:alternate"]"#) {
        let alts: Vec<Value> = document
            .select(&sel)
            .filter_map(|m| {
                m.value()
                    .attr("content")
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
            })
            .map(|c| Value::String(c.to_string()))
            .collect();
        if !alts.is_empty() {
            map.insert("ogLocaleAlternate".to_string(), Value::Array(alts));
        }
    }

    // Flatten every remaining <meta name|property|itemprop> as name → content.
    // First meaningful value wins (matching the "first wins" convention); this
    // is what surfaces raw keys like `og:title`, `twitter:*`, `viewport`,
    // `theme-color` alongside the camelCase keys above.
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

    // Backfill title from ogTitle / twitter:title if primary extraction missed.
    if !map.contains_key("title") {
        let fallback = map
            .get("ogTitle")
            .or_else(|| map.get("og:title"))
            .or_else(|| map.get("twitter:title"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if let Some(title) = fallback {
            map.insert("title".to_string(), Value::String(title));
        }
    }

    // <link rel=canonical> → canonical (absolutized).
    if let Some(href) = select_link_href(&document, "canonical") {
        map.insert(
            "canonical".to_string(),
            Value::String(absolutize_one(url, &href)),
        );
    }

    // <link rel~=icon> → favicon (absolutized).
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

/// Read the `content` of the first `<meta property="{key}">` or, failing that,
/// `<meta name="{key}">`. `og:*` keys sit on `property`; `article:*` / `dc.*` /
/// `description` etc. sit on `name` — checking both covers Firecrawl's mix.
fn meta_content(document: &Html, key: &str) -> Option<String> {
    for attr in ["property", "name"] {
        if let Ok(sel) = Selector::parse(&format!(r#"meta[{attr}="{key}"]"#)) {
            for m in document.select(&sel) {
                if let Some(content) = m.value().attr("content").map(str::trim) {
                    if !content.is_empty() {
                        return Some(content.to_string());
                    }
                }
            }
        }
    }
    None
}

/// The trimmed, whitespace-collapsed text content of the first element matching
/// `selector`.
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

/// Collapse runs of whitespace (including newlines) into single spaces and trim.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ===================================================================
// Unit tests (in-crate: exercise helpers + a representative article)
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A representative article page with site chrome (nav/header/aside/footer),
    /// rich metadata, and a body exercising every Markdown construct.
    const ARTICLE: &str = r##"<!doctype html>
<html lang="en">
<head>
  <title>The Great Article</title>
  <meta name="description" content="An article about interesting things.">
  <meta name="keywords" content="a, b, c">
  <meta name="robots" content="index,follow">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="theme-color" content="#ffffff">
  <meta property="og:title" content="The Great Article (OG)">
  <meta property="og:type" content="article">
  <meta property="og:locale:alternate" content="fr_FR">
  <meta property="og:locale:alternate" content="de_DE">
  <meta name="twitter:card" content="summary_large_image">
  <meta name="article:section" content="Eng">
  <link rel="canonical" href="/articles/great">
  <link rel="icon" href="/favicon.ico">
</head>
<body>
  <nav class="navbar"><a href="/">Home</a> <a href="/blog">Blog</a> <a href="/about">About</a></nav>
  <header class="header"><h1>Site Name</h1><p>Site tagline boilerplate that should be dropped.</p></header>
  <aside class="sidebar"><div class="ad"><a href="https://ads.example/x">ad</a></div></aside>
  <main>
    <article>
      <h1>The Great Article</h1>
      <p>An opening paragraph with a <a href="/relative/path">relative link</a>,
         an <a href="https://external.example/page">external link</a>, a
         <a href="//cdn.example/x">protocol-relative link</a> and a
         <a href="#frag">jump link</a> to set the scene.</p>
      <h2>Section One</h2>
      <p>Some more prose here that continues the article body and gives the extractor
         enough signal to treat this region as the main content of the page and keep it.</p>
      <ul>
        <li>First bullet point</li>
        <li>Second bullet point</li>
      </ul>
      <blockquote><p>A memorable and sufficiently long pull-quote from the piece.</p></blockquote>
      <pre><code class="language-rust">fn main() { println!("hi"); }</code></pre>
      <table>
        <thead><tr><th>Name</th><th>Score</th></tr></thead>
        <tbody>
          <tr><td>Alpha</td><td>10</td></tr>
          <tr><td>Beta</td><td>20</td></tr>
        </tbody>
      </table>
      <p>A closing paragraph with an image <img src="/img/photo.png" alt="a photo">,
         some <del>struck</del> text, and more prose to stay above the threshold.</p>
    </article>
  </main>
  <footer class="footer"><p>Copyright 2026 - footer boilerplate that must be stripped.</p></footer>
  <script>tracker()</script>
  <style>.x{}</style>
  <noscript>NOSCRIPT</noscript>
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
    fn headings_are_atx() {
        let md = scrape_article(true).markdown;
        assert!(
            md.lines().any(|l| l.trim_start().starts_with("# ")),
            "expected an ATX h1 in:\n{md}"
        );
        assert!(
            md.lines()
                .any(|l| l.trim_start().starts_with("## Section One")),
            "expected an ATX h2 in:\n{md}"
        );
    }

    #[test]
    fn links_absolutized_all_forms() {
        let md = scrape_article(true).markdown;
        // relative → absolute
        assert!(md.contains("(https://site.example/relative/path)"), "{md}");
        // external → verbatim
        assert!(md.contains("(https://external.example/page)"), "{md}");
        // protocol-relative → picks up base scheme
        assert!(md.contains("(https://cdn.example/x)"), "{md}");
        // fragment → left as-is
        assert!(md.contains("(#frag)"), "{md}");
    }

    #[test]
    fn image_absolutized() {
        let md = scrape_article(true).markdown;
        assert!(
            md.contains("![a photo](https://site.example/img/photo.png)"),
            "{md}"
        );
    }

    #[test]
    fn bullet_marker_is_dash() {
        let md = scrape_article(true).markdown;
        assert!(
            md.lines().any(|l| {
                let t = l.trim_start();
                t.starts_with('-') && t.trim_start_matches(['-', ' ']).starts_with("First")
            }),
            "expected a `-` bullet before 'First':\n{md}"
        );
        assert!(!md.contains("* First"), "bullets should not use `*`:\n{md}");
    }

    #[test]
    fn fenced_code_with_language() {
        let md = scrape_article(true).markdown;
        assert!(md.contains("```rust"), "expected ```rust fence:\n{md}");
        assert!(
            md.contains(r#"println!("hi")"#),
            "code content missing:\n{md}"
        );
    }

    #[test]
    fn blockquote_survives() {
        let md = scrape_article(true).markdown;
        assert!(
            md.lines().any(|l| l.trim_start().starts_with("> ")),
            "blockquote marker missing:\n{md}"
        );
    }

    #[test]
    fn strikethrough_renders_as_gfm() {
        let md = scrape_article(true).markdown;
        assert!(md.contains("~~struck~~"), "expected ~~struck~~ in:\n{md}");
        // No stray escaping of the leading tilde.
        assert!(
            !md.contains(r"\~~struck"),
            "tilde should not be escaped:\n{md}"
        );
    }

    #[test]
    fn strong_and_em_delimiters() {
        // A focused doc so we assert exact delimiters.
        let html = "<article><p>x <strong>bold</strong> and <em>ital</em> y and a bit more \
                     body text so this counts as content for extraction purposes here.</p></article>";
        let md = scrape(html, "https://x.example/", 200, "text/html", true).markdown;
        assert!(md.contains("**bold**"), "strong should be ** :\n{md}");
        assert!(md.contains("_ital_"), "em should be _ :\n{md}");
    }

    #[test]
    fn gfm_table_survives() {
        let md = scrape_article(true).markdown;
        assert!(
            md.lines()
                .any(|l| l.contains('|') && l.contains("Name") && l.contains("Score")),
            "table header row missing:\n{md}"
        );
        assert!(
            md.lines().any(|l| l.contains('|') && l.contains("---")),
            "table separator row missing:\n{md}"
        );
        assert!(
            md.contains("Alpha") && md.contains("Beta"),
            "table cells missing:\n{md}"
        );
    }

    // ---- Boilerplate is dropped under only_main_content ----------------

    #[test]
    fn boilerplate_dropped_under_main_content() {
        let md = scrape_article(true).markdown;
        assert!(!md.contains("footer boilerplate"), "footer leaked:\n{md}");
        assert!(
            !md.contains("Site tagline boilerplate"),
            "header leaked:\n{md}"
        );
        assert!(
            !md.contains("(https://site.example/blog)"),
            "nav leaked:\n{md}"
        );
        assert!(!md.contains("ads.example"), "aside ad leaked:\n{md}");
    }

    #[test]
    fn scripts_styles_never_leak() {
        for only_main in [true, false] {
            let md = scrape_article(only_main).markdown;
            assert!(
                !md.contains("tracker()"),
                "script leaked (main={only_main}):\n{md}"
            );
            assert!(
                !md.contains(".x{"),
                "style leaked (main={only_main}):\n{md}"
            );
            assert!(
                !md.contains("NOSCRIPT"),
                "noscript leaked (main={only_main}):\n{md}"
            );
        }
    }

    #[test]
    fn whole_body_keeps_chrome_and_absolutizes() {
        let md = scrape(
            ARTICLE,
            "https://site.example/articles/great",
            200,
            "text/html",
            false,
        )
        .markdown;
        assert!(
            md.contains("footer boilerplate"),
            "footer should be kept:\n{md}"
        );
        assert!(
            md.contains("(https://site.example/blog)"),
            "nav link should be absolutized:\n{md}"
        );
    }

    // ---- Metadata parity -----------------------------------------------

    #[test]
    fn metadata_named_and_raw_keys() {
        let meta = scrape_article(true).metadata;
        // Primary
        assert_eq!(meta["title"], "The Great Article");
        assert_eq!(meta["description"], "An article about interesting things.");
        assert_eq!(meta["language"], "en");
        assert_eq!(meta["keywords"], "a, b, c");
        assert_eq!(meta["robots"], "index,follow");
        // camelCase OG/article keys (Firecrawl parity)
        assert_eq!(meta["ogTitle"], "The Great Article (OG)");
        assert_eq!(meta["articleSection"], "Eng");
        assert_eq!(
            meta["ogLocaleAlternate"],
            serde_json::json!(["fr_FR", "de_DE"])
        );
        // Raw flattened keys still present
        assert_eq!(meta["og:title"], "The Great Article (OG)");
        assert_eq!(meta["twitter:card"], "summary_large_image");
        assert_eq!(meta["viewport"], "width=device-width, initial-scale=1");
        assert_eq!(meta["theme-color"], "#ffffff");
        // Absolutized link metadata
        assert_eq!(meta["canonical"], "https://site.example/articles/great");
        assert_eq!(meta["favicon"], "https://site.example/favicon.ico");
        // Synthetic keys
        assert_eq!(meta["sourceURL"], "https://site.example/articles/great");
        assert_eq!(meta["url"], "https://site.example/articles/great");
        assert_eq!(meta["statusCode"], 200);
        assert_eq!(meta["contentType"], "text/html; charset=utf-8");
    }

    #[test]
    fn title_backfills_from_og_when_missing() {
        let html = r##"<html><head>
            <meta property="og:title" content="OG Only Title">
            </head><body><article><p>Body text long enough to be treated as the main \
            article content of this page for extraction purposes here now.</p></article></body></html>"##;
        let meta = scrape(html, "https://x.example/", 200, "text/html", true).metadata;
        assert_eq!(meta["title"], "OG Only Title");
    }

    // ---- Post-processing helpers ---------------------------------------

    #[test]
    fn base64_images_removed_alt_kept() {
        let md = post_process_markdown("before ![pixel](data:image/png;base64,AAAA) after");
        assert_eq!(md, "before ![pixel](<Base64-Image-Removed>) after");
        // Non-data images are untouched.
        let md2 = post_process_markdown("![x](https://h/i.png)");
        assert_eq!(md2, "![x](https://h/i.png)");
    }

    #[test]
    fn skip_to_content_links_removed() {
        let md = post_process_markdown("[Skip to Content](#main) Real text");
        assert_eq!(md.trim(), "Real text");
        // Case-insensitive.
        let md2 = post_process_markdown("[skip to content](#x)Body");
        assert_eq!(md2, "Body");
    }

    #[test]
    fn blank_lines_collapsed() {
        let md = post_process_markdown("a\n\n\n\n\nb");
        assert_eq!(md, "a\n\nb");
    }

    #[test]
    fn multiline_link_text_gets_backslash() {
        // A newline inside link text is escaped so the link survives.
        let out = process_multiline_links("[line one\nline two](http://x)");
        assert_eq!(out, "[line one\\\nline two](http://x)");
        // Text outside links is untouched.
        assert_eq!(process_multiline_links("a\nb"), "a\nb");
    }

    // ---- Code-language normalization -----------------------------------

    #[test]
    fn code_language_from_lang_and_hljs() {
        assert_eq!(
            normalize_class_languages("lang-python"),
            Some("lang-python language-python".to_string())
        );
        assert_eq!(
            normalize_class_languages("hljs javascript"),
            Some("hljs javascript language-javascript".to_string())
        );
        // Already language- → no rewrite.
        assert_eq!(normalize_class_languages("language-rust hljs"), None);
        // hljs with no language token → no rewrite.
        assert_eq!(normalize_class_languages("hljs"), None);
        // Unrelated class → no rewrite.
        assert_eq!(normalize_class_languages("prose"), None);
    }

    #[test]
    fn srcset_picks_largest() {
        assert_eq!(
            largest_srcset_url("/s.jpg 480w, /l.jpg 1200w", None).as_deref(),
            Some("/l.jpg")
        );
        // All density descriptors → current src folded in as 1x.
        assert_eq!(
            largest_srcset_url("/a.jpg 2x, /b.jpg 3x", Some("/c.jpg")).as_deref(),
            Some("/b.jpg")
        );
    }

    // ---- URL resolution helper -----------------------------------------

    #[test]
    fn resolve_url_special_schemes_and_relatives() {
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
        assert_eq!(resolve_url(&base, "/c"), "https://x.example/c");
        assert_eq!(
            resolve_url(&base, "//cdn.example/x.js"),
            "https://cdn.example/x.js"
        );
        assert_eq!(resolve_url(&base, "d"), "https://x.example/a/d");
    }

    // ---- Degenerate inputs never panic ---------------------------------

    #[test]
    fn thin_spa_shell_still_returns_metadata() {
        let thin = r#"<!doctype html><html><head><title>App</title></head>
            <body><div id="root"></div><script src="/app.js"></script></body></html>"#;
        let res = scrape(thin, "https://spa.example/", 200, "text/html", true);
        assert_eq!(res.metadata["title"], "App");
        assert_eq!(res.metadata["statusCode"], 200);
        assert!(is_thin_content(&res.markdown, 50));
    }

    #[test]
    fn empty_and_garbage_never_panic() {
        let a = scrape("", "https://x/", 200, "text/html", true);
        assert_eq!(a.metadata["url"], "https://x/");
        let b = scrape("not html at all", "https://x/", 404, "text/plain", true);
        assert_eq!(b.metadata["statusCode"], 404);
    }

    #[test]
    fn merge_rendered_splices_shell_head_onto_hydrated_body() {
        // The render-then-Markdown join: a thin shell (rich head, empty mount) +
        // a hydrated DOM (thin head, real body) → one document that scrapes to
        // both the shell's metadata and the hydrated article.
        let shell = r#"<!doctype html><html><head><title>Guide</title>
            <meta property="og:title" content="The Guide"><base href="https://d.example/">
            </head><body><div id="app"></div></body></html>"#;
        let hydrated = "<html><head></head><body><main><article>\
            <h1>The Guide</h1><p>Rendered body content that only exists after hydration.</p>\
            </article></main></body></html>";

        let merged = merge_rendered_document(shell, hydrated);
        // Shell head (title/OG/base) is preserved; hydrated body replaces the shell's.
        assert!(merged.contains("<title>Guide</title>"));
        assert!(merged.contains(r#"property="og:title""#));
        assert!(merged.contains("Rendered body content"));
        assert!(
            !merged.contains(r#"id="app""#),
            "shell's empty mount is replaced"
        );

        let res = scrape(&merged, "https://d.example/guide", 200, "text/html", true);
        assert_eq!(res.metadata["title"], "Guide");
        assert_eq!(res.metadata["og:title"], "The Guide");
        assert!(res.markdown.contains("Rendered body content"));
        assert!(res.markdown.contains("# The Guide"));
    }

    #[test]
    fn merge_rendered_degrades_without_shell_head_or_rendered_body() {
        // No shell head → fall back to the rendered document's own head; a
        // rendered string with no <body> is treated as body-only markup.
        let merged = merge_rendered_document("<div>shell</div>", "<p>just a fragment</p>");
        assert!(merged.contains("<p>just a fragment</p>"));
        // Must remain parseable/usable by the engine.
        let res = scrape(&merged, "https://x/", 200, "text/html", false);
        assert!(res.markdown.contains("just a fragment"));
    }

    #[test]
    fn slice_element_is_case_insensitive_and_tag_bounded() {
        // Matches regardless of case, and does not mistake `<bodyguard>` for `<body>`.
        let html = "<HTML><BODY class=x>hi</BODY></HTML>";
        assert_eq!(slice_element(html, "body"), Some("<BODY class=x>hi</BODY>"));
        assert_eq!(slice_element("<bodyguard>no</bodyguard>", "body"), None);
        assert_eq!(
            slice_element("<head><title>t</title></head>", "head"),
            Some("<head><title>t</title></head>")
        );
    }

    #[test]
    fn skeleton_line_matches_only_pure_placeholders() {
        assert!(is_skeleton_line("Loading..."));
        assert!(is_skeleton_line("### Loading…"));
        assert!(is_skeleton_line("- Loading"));
        assert!(is_skeleton_line("1. **Loading...**"));
        assert!(is_skeleton_line("> please wait"));
        // Real content that merely contains the word must NOT match.
        assert!(!is_skeleton_line("Loading dock tours available"));
        assert!(!is_skeleton_line("Load More"));
        assert!(!is_skeleton_line("Downloading the report"));
        assert!(!is_skeleton_line("![loading](https://x/spinner.gif)"));
        assert!(!is_skeleton_line(""));
    }

    #[test]
    fn incomplete_render_needs_several_markers() {
        let one = "# Real Title\n\nSome content.\n\nLoading...";
        assert!(
            !is_incomplete_render(one),
            "a single Loading is not a skeleton"
        );
        let skeleton = "# Deals\n\n### Loading...\n\n### Loading...\n\n### Loading...\n";
        assert!(is_incomplete_render(skeleton));
    }

    #[test]
    fn strip_incomplete_markers_removes_only_placeholders() {
        let md = "# Deals\n\n### Loading...\n\n### Loading...\n\nReal promo copy here.\n\n### Loading…\n\n![loading](https://x/spinner.gif)";
        let out = strip_incomplete_markers(md);
        assert!(!out.contains("Loading"), "placeholder lines removed: {out}");
        assert!(out.contains("# Deals"));
        assert!(out.contains("Real promo copy here."));
        // The image with alt="loading" is preserved (not a bare placeholder line).
        assert!(out.contains("![loading](https://x/spinner.gif)"));
    }

    #[test]
    fn scrape_flags_skeleton_and_strips_loading_even_when_not_thin() {
        // A chrome-heavy shell (well over the thin bar) whose content rails are all
        // `Loading…` — the Target.com failure mode. Must be flagged incomplete and
        // must not emit `Loading` noise.
        let mut html =
            String::from("<!doctype html><html><head><title>Shop</title></head><body><main>");
        html.push_str("<h1>Featured categories</h1>");
        for c in [
            "Women",
            "Men",
            "Kids",
            "Home",
            "Grocery",
            "Beauty",
            "Toys",
            "Electronics",
        ] {
            html.push_str(&format!(
                "<a href=\"/c/{c}\">{c} department landing page</a>"
            ));
        }
        html.push_str("<section><h2>Just in for summer</h2>");
        for _ in 0..10 {
            html.push_str("<li><h3>Loading...</h3></li>");
        }
        html.push_str("</section></main></body></html>");

        let res = scrape(&html, "https://shop.example/", 200, "text/html", true);
        assert!(res.incomplete, "skeleton page should be flagged incomplete");
        assert!(
            !res.markdown.contains("Loading"),
            "no Loading noise: {}",
            res.markdown
        );
        // The real chrome survives.
        assert!(res.markdown.contains("Featured categories"));
        // And it is NOT thin (chrome alone clears the bar) — proving length-based
        // detection alone would have missed it.
        assert!(!is_thin_content(&res.markdown, MIN_MAIN_CONTENT_CHARS));
    }
}
