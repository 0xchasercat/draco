//! Integration tests for the content engine ([`draco_static::content::scrape`]).
//!
//! Each test loads a hand-authored **messy** HTML fixture from
//! `tests/fixtures/content/` and asserts that the rendered Markdown is clean and
//! Firecrawl-parity: boilerplate dropped, code fenced with the right language,
//! GFM tables/lists/blockquotes intact, links absolutized, base64 images
//! stripped, and scripts/styles/noscript never leaking. Fixtures are static so
//! the tests are deterministic (no live network).

use draco_static::content::scrape;
use serde_json::Value;

/// Load a fixture by file name from `tests/fixtures/content/`.
fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/content/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// Scrape a fixture in main-content mode against a given URL.
fn scrape_fixture(name: &str, url: &str) -> (String, Value) {
    let html = fixture(name);
    let r = scrape(&html, url, 200, "text/html; charset=utf-8", true);
    (r.markdown, r.metadata)
}

// ===================================================================
// Fixture 1 — article wrapped in nav/header/aside/footer boilerplate
// ===================================================================

#[test]
fn article_boilerplate_drops_chrome_keeps_body() {
    let (md, meta) = scrape_fixture(
        "article_boilerplate.html",
        "https://blog.example/articles/great",
    );

    // Article body kept.
    assert!(
        md.contains("# The Great Article"),
        "article h1 missing:\n{md}"
    );
    assert!(md.contains("## Section One"), "article h2 missing:\n{md}");
    assert!(md.contains("First bullet point"), "list missing:\n{md}");
    // Nested list rendered with dash markers and indentation.
    assert!(
        md.lines()
            .any(|l| l.starts_with("    -") && l.contains("nested a")),
        "nested list missing:\n{md}"
    );
    assert!(
        md.lines().any(|l| l.trim_start().starts_with("> ")),
        "blockquote missing:\n{md}"
    );

    // Boilerplate dropped.
    assert!(!md.contains("footer boilerplate"), "footer leaked:\n{md}");
    assert!(
        !md.contains("Site tagline boilerplate"),
        "header leaked:\n{md}"
    );
    assert!(!md.contains("Related"), "aside leaked:\n{md}");
    assert!(!md.contains("Buy now"), "ad leaked:\n{md}");
    assert!(
        !md.contains("(https://blog.example/blog)"),
        "nav leaked:\n{md}"
    );
    assert!(!md.contains("ads.example"), "ad link leaked:\n{md}");

    // Scripts/styles/noscript never leak.
    assert!(!md.contains("analytics"), "script leaked:\n{md}");
    assert!(
        !md.contains("Please enable JavaScript"),
        "noscript leaked:\n{md}"
    );

    // Links absolutized (relative, protocol-relative), fragment kept.
    assert!(
        md.contains("(https://blog.example/relative/path)"),
        "relative link:\n{md}"
    );
    assert!(
        md.contains("(https://external.example/page)"),
        "external link:\n{md}"
    );
    assert!(
        md.contains("(https://cdn.example/asset)"),
        "protocol-relative link:\n{md}"
    );
    assert!(md.contains("(#section-one)"), "fragment link:\n{md}");
    // Image absolutized.
    assert!(
        md.contains("![a photo](https://blog.example/img/photo.png)"),
        "image:\n{md}"
    );

    // Metadata parity: camelCase + raw keys, arrays, absolutized link meta.
    assert_eq!(meta["title"], "The Great Article — Example Blog");
    assert_eq!(meta["description"], "An article about interesting things.");
    assert_eq!(meta["keywords"], "rust, extraction, markdown");
    assert_eq!(meta["robots"], "index,follow");
    assert_eq!(meta["language"], "en");
    assert_eq!(meta["ogTitle"], "The Great Article (OG)");
    assert_eq!(meta["ogDescription"], "OG description of the article.");
    assert_eq!(meta["articleSection"], "Engineering");
    assert_eq!(meta["articleTag"], "scraping");
    assert_eq!(meta["publishedTime"], "2026-01-02T10:00:00Z");
    assert_eq!(meta["modifiedTime"], "2026-01-03T12:00:00Z");
    assert_eq!(meta["dcDescription"], "Dublin Core description.");
    assert_eq!(
        meta["ogLocaleAlternate"],
        serde_json::json!(["fr_FR", "de_DE"])
    );
    // Raw flattened keys also present.
    assert_eq!(meta["og:title"], "The Great Article (OG)");
    assert_eq!(meta["twitter:card"], "summary_large_image");
    assert_eq!(meta["viewport"], "width=device-width, initial-scale=1");
    assert_eq!(meta["theme-color"], "#0a0a0a");
    // Absolutized canonical + favicon.
    assert_eq!(meta["canonical"], "https://blog.example/articles/great");
    assert_eq!(meta["favicon"], "https://blog.example/favicon.ico");
    // Synthetic.
    assert_eq!(meta["sourceURL"], "https://blog.example/articles/great");
    assert_eq!(meta["statusCode"], 200);
    assert_eq!(meta["contentType"], "text/html; charset=utf-8");
}

// ===================================================================
// Fixture 2 — docs page with fenced code in several language-class styles
// ===================================================================

#[test]
fn docs_code_fences_with_inferred_languages() {
    let (md, _) = scrape_fixture("docs_code.html", "https://docs.example/install");

    // `language-rust` on <code>.
    assert!(md.contains("```rust"), "rust fence missing:\n{md}");
    // `lang-python` normalized.
    assert!(md.contains("```python"), "python fence missing:\n{md}");
    // `hljs javascript` normalized.
    assert!(
        md.contains("```javascript"),
        "javascript fence missing:\n{md}"
    );
    // Plain block (no language).
    assert!(
        md.contains("```\n$ cargo build --release"),
        "plain fence missing:\n{md}"
    );

    // Code preserved verbatim: entity decoded, indentation kept, a leading-space
    // capitalized line survives.
    assert!(
        md.contains("for i in &items {"),
        "entity/verbatim broken:\n{md}"
    );
    assert!(
        md.lines().any(|l| l == "   Compiling widget v0.1.0"),
        "indented pre line dropped:\n{md}"
    );
    // Inline code preserved; the `*star*` inside <code> is not emphasized.
    assert!(md.contains("`Widget::new()`"), "inline code missing:\n{md}");
    assert!(
        md.contains("`*star*`"),
        "inline code with literal stars missing:\n{md}"
    );

    // Chrome dropped even though header wraps #main-bearing region.
    assert!(
        !md.contains("Docs footer boilerplate"),
        "footer leaked:\n{md}"
    );
    assert!(!md.contains("Docs Home"), "nav leaked:\n{md}");
}

// ===================================================================
// Fixture 3 — GFM-rich page (table, nested lists, blockquote, code, hr, strike)
// ===================================================================

#[test]
fn gfm_rich_all_constructs() {
    let (md, _) = scrape_fixture("gfm_rich.html", "https://docs.example/features");

    // GFM table: header + separator + data rows.
    assert!(
        md.lines()
            .any(|l| l.contains('|') && l.contains("Feature") && l.contains("Since")),
        "table header missing:\n{md}"
    );
    assert!(
        md.lines().any(|l| l.contains('|') && l.contains("---")),
        "table separator missing:\n{md}"
    );
    assert!(
        md.contains("Strikethrough") && md.contains("Beta"),
        "table cells missing:\n{md}"
    );

    // Nested list with dash markers.
    assert!(
        md.lines()
            .any(|l| l.starts_with("    -") && l.contains("nested item a")),
        "nested list missing:\n{md}"
    );

    // Blockquote.
    assert!(
        md.lines()
            .any(|l| l.trim_start().starts_with("> ") && l.contains("common case")),
        "blockquote missing:\n{md}"
    );

    // Inline code + emphasis + strikethrough.
    assert!(md.contains("`convert(html)`"), "inline code missing:\n{md}");
    assert!(md.contains("**strong**"), "strong missing:\n{md}");
    assert!(md.contains("_emphasis_"), "em missing:\n{md}");
    assert!(
        md.contains("~~deprecated wording~~"),
        "strikethrough missing:\n{md}"
    );
    assert!(
        !md.contains(r"\~~"),
        "strikethrough tilde was escaped:\n{md}"
    );

    // Horizontal rule.
    assert!(md.lines().any(|l| l.trim() == "* * *"), "hr missing:\n{md}");
}

// ===================================================================
// Fixture 4 — links + base64 image + responsive image (with <base href>)
// ===================================================================

#[test]
fn links_images_absolutized_and_base64_removed() {
    let (md, _) = scrape_fixture("links_images.html", "https://www.example/gallery");

    // Resolved against the document's <base href> (cdn.example/site/).
    assert!(
        md.contains("(https://cdn.example/site/rel/path)"),
        "base-relative link:\n{md}"
    );
    assert!(
        md.contains("(https://external.example/x)"),
        "external link:\n{md}"
    );
    assert!(
        md.contains("(https://other.example/y)"),
        "protocol-relative link:\n{md}"
    );
    assert!(md.contains("(#top)"), "fragment link:\n{md}");
    assert!(md.contains("(mailto:hi@example.com)"), "mailto link:\n{md}");

    // Base64 image replaced (alt kept), real + responsive images absolutized.
    assert!(
        md.contains("![tracking pixel](<Base64-Image-Removed>)"),
        "base64 not removed:\n{md}"
    );
    assert!(!md.contains("base64,"), "base64 payload leaked:\n{md}");
    assert!(
        md.contains("![hero](https://cdn.example/site/images/hero.png)"),
        "real image:\n{md}"
    );
    // Responsive image resolves to the largest candidate (1200w).
    assert!(
        md.contains("![responsive](https://cdn.example/site/images/large.jpg)"),
        "responsive image should pick largest:\n{md}"
    );
}

// ===================================================================
// Fixture 5 — script/style/noscript/svg/template noise must never leak
// ===================================================================

#[test]
fn script_noise_never_leaks() {
    let (md, meta) = scrape_fixture("script_noise.html", "https://www.example/noisy");

    // Real content kept.
    assert!(md.contains("# Noisy Page"), "heading missing:\n{md}");
    assert!(md.contains("real prose"), "article body missing:\n{md}");

    // None of the noise markers leak.
    assert!(
        !md.contains("STYLE_AND_SCRIPT_LEAK_MARKER"),
        "script/style leaked:\n{md}"
    );
    assert!(
        !md.contains("NOSCRIPT_LEAK_MARKER"),
        "noscript leaked:\n{md}"
    );
    assert!(
        !md.contains("TEMPLATE_LEAK_MARKER"),
        "template leaked:\n{md}"
    );
    assert!(
        !md.contains("do-not-leak"),
        "head JSON script leaked:\n{md}"
    );
    assert!(!md.contains("console.log"), "inline script leaked:\n{md}");

    // Skip-to-content link removed; SVG title not surfaced as content.
    assert!(!md.contains("Skip to Content"), "skip link leaked:\n{md}");

    // Metadata still extracted despite the noise.
    assert_eq!(meta["title"], "Noisy Page");
    assert_eq!(meta["description"], "A page full of non-content noise.");
}
