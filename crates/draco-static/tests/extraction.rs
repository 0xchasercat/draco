//! Golden tests for `draco-static` Tier 0 extraction and Tier 1 build-id /
//! URL helpers. Fully offline: every input is a checked-in fixture.

use draco_static::{
    discover_build_id, extract_static, is_app_router, next_data_url, StaticOutcome,
};
use draco_types::{ExtractOrigin, SourceTier};

/// Load a fixture from `tests/fixtures/` at compile time.
macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!("fixtures/", $name))
    };
}

const NEXT_PAGES: &str = fixture!("next_pages.html");
const JSONLD_PRODUCT: &str = fixture!("jsonld_product.html");
const NUXT_OBJECT: &str = fixture!("nuxt_object.html");
const NUXT_FACTORY: &str = fixture!("nuxt_factory.html");
const NEXT_APP_ROUTER: &str = fixture!("next_app_router.html");
const PLAIN_SPA: &str = fixture!("plain_spa.html");

/// Unwrap a `Hit`, asserting the tier + origin, and hand back the payload.
fn expect_hit(outcome: StaticOutcome, origin: ExtractOrigin) -> serde_json::Value {
    match outcome {
        StaticOutcome::Hit(data) => {
            assert_eq!(data.tier, SourceTier::Static, "Tier 0 always emits Static");
            assert_eq!(data.origin, origin, "unexpected extraction origin");
            data.data
        }
        StaticOutcome::Miss => panic!("expected Hit({origin:?}), got Miss"),
    }
}

// -------------------------------------------------------------------
// Tier 0: __NEXT_DATA__
// -------------------------------------------------------------------

#[test]
fn next_data_is_extracted() {
    let data = expect_hit(extract_static(NEXT_PAGES), ExtractOrigin::NextData);
    assert_eq!(data["buildId"], "aBcD1234xyz789KLMNOpq");
    assert_eq!(data["props"]["pageProps"]["product"]["name"], "Widget 4000");
    assert_eq!(data["props"]["pageProps"]["product"]["price"], 19.99);
    assert_eq!(data["query"]["id"], "4000");
}

#[test]
fn next_data_beats_other_paradigms() {
    // A doc with both __NEXT_DATA__ and JSON-LD must resolve to NextData.
    let mixed = "<html><head>\
         <script type=\"application/ld+json\">{\"@type\":\"Product\"}</script>\
         <script id=\"__NEXT_DATA__\" type=\"application/json\">{\"buildId\":\"x1y2z3\",\"props\":{}}</script>\
         </head><body></body></html>";
    let data = expect_hit(extract_static(mixed), ExtractOrigin::NextData);
    assert_eq!(data["buildId"], "x1y2z3");
}

// -------------------------------------------------------------------
// Tier 0: JSON-LD
// -------------------------------------------------------------------

#[test]
fn json_ld_blocks_are_collected_into_an_array() {
    let data = expect_hit(extract_static(JSONLD_PRODUCT), ExtractOrigin::JsonLd);
    let arr = data.as_array().expect("JSON-LD payload is an array");
    assert_eq!(arr.len(), 2, "both ld+json blocks are collected");
    assert_eq!(arr[0]["@type"], "Product");
    assert_eq!(arr[0]["name"], "Deluxe Kettle");
    assert_eq!(arr[0]["offers"]["price"], "49.95");
    assert_eq!(arr[1]["@type"], "BreadcrumbList");
    assert_eq!(arr[1]["itemListElement"][1]["name"], "Kitchen");
}

#[test]
fn json_ld_ignores_other_script_types() {
    let html = "<html><head>\
        <script type=\"application/json\">{\"not\":\"ld\"}</script>\
        <script type=\"application/ld+json\">{\"@type\":\"Thing\",\"n\":1}</script>\
        <script>var x = {\"@type\":\"Nope\"};</script>\
        </head></html>";
    let data = expect_hit(extract_static(html), ExtractOrigin::JsonLd);
    let arr = data.as_array().unwrap();
    assert_eq!(arr.len(), 1, "only the ld+json block is picked up");
    assert_eq!(arr[0]["@type"], "Thing");
}

// -------------------------------------------------------------------
// Tier 0: Nuxt
// -------------------------------------------------------------------

#[test]
fn nuxt_object_literal_is_extracted() {
    let data = expect_hit(extract_static(NUXT_OBJECT), ExtractOrigin::NuxtWindow);
    assert_eq!(data["layout"], "default");
    assert_eq!(data["serverRendered"], true);
    assert_eq!(data["data"][0]["products"][0]["title"], "Alpha");
    // Brace-inside-a-string must not confuse balanced-span extraction.
    assert_eq!(data["data"][0]["products"][1]["title"], "Beta { special }");
    assert_eq!(data["state"]["cartCount"], 0);
}

#[test]
fn nuxt_factory_form_is_a_miss() {
    // The IIFE/factory form cannot be evaluated statically → escalate.
    match extract_static(NUXT_FACTORY) {
        StaticOutcome::Miss => {}
        StaticOutcome::Hit(d) => panic!("factory __NUXT__ must Miss, got {:?}", d.origin),
    }
}

// -------------------------------------------------------------------
// Tier 0: Miss / escalate
// -------------------------------------------------------------------

#[test]
fn plain_spa_is_a_miss() {
    assert!(matches!(extract_static(PLAIN_SPA), StaticOutcome::Miss));
}

#[test]
fn app_router_page_has_no_static_payload() {
    // App-router streams via flight chunks; there is no Tier 0 blob to lift.
    assert!(matches!(
        extract_static(NEXT_APP_ROUTER),
        StaticOutcome::Miss
    ));
}

#[test]
fn empty_and_garbage_inputs_miss() {
    assert!(matches!(extract_static(""), StaticOutcome::Miss));
    assert!(matches!(
        extract_static("not html at all"),
        StaticOutcome::Miss
    ));
    // Malformed JSON in a __NEXT_DATA__ tag must not panic and must Miss.
    let broken = "<script id=\"__NEXT_DATA__\">{ this is : not json </script>";
    assert!(matches!(extract_static(broken), StaticOutcome::Miss));
}

// -------------------------------------------------------------------
// Tier 1: build-id discovery
// -------------------------------------------------------------------

#[test]
fn build_id_from_next_data() {
    assert_eq!(
        discover_build_id(NEXT_PAGES).as_deref(),
        Some("aBcD1234xyz789KLMNOpq")
    );
}

#[test]
fn build_id_from_explicit_assignment() {
    let html = "<html><body><script>self.__BUILD_ID = \"deploy-42abc\";</script></body></html>";
    assert_eq!(discover_build_id(html).as_deref(), Some("deploy-42abc"));
}

#[test]
fn build_id_from_static_asset_path() {
    // No __NEXT_DATA__, no __BUILD_ID — recover from the asset path, preferring
    // the _buildManifest.js marker over unrelated chunk paths.
    let html = "<html><head>\
        <script src=\"/_next/static/chunks/main.js\"></script>\
        <link href=\"/_next/static/css/app.css\" />\
        <script src=\"/_next/static/Xy9_Zk-buildhash01/_buildManifest.js\"></script>\
        </head></html>";
    assert_eq!(
        discover_build_id(html).as_deref(),
        Some("Xy9_Zk-buildhash01")
    );
}

#[test]
fn build_id_absent_returns_none() {
    assert_eq!(discover_build_id(PLAIN_SPA), None);
    assert_eq!(discover_build_id(""), None);
}

// -------------------------------------------------------------------
// Tier 1: _next/data URL construction
// -------------------------------------------------------------------

#[test]
fn next_data_url_basic_route() {
    assert_eq!(
        next_data_url("BID123", "/products/42", &[]),
        "/_next/data/BID123/products/42.json"
    );
}

#[test]
fn next_data_url_root_maps_to_index() {
    assert_eq!(
        next_data_url("BID123", "/", &[]),
        "/_next/data/BID123/index.json"
    );
    assert_eq!(
        next_data_url("BID123", "", &[]),
        "/_next/data/BID123/index.json"
    );
}

#[test]
fn next_data_url_strips_trailing_slash() {
    assert_eq!(
        next_data_url("BID123", "/blog/post/", &[]),
        "/_next/data/BID123/blog/post.json"
    );
}

#[test]
fn next_data_url_appends_encoded_query() {
    let query = vec![
        ("id".to_string(), "42".to_string()),
        ("q".to_string(), "hi there/&x".to_string()),
    ];
    assert_eq!(
        next_data_url("BID123", "/search", &query),
        "/_next/data/BID123/search.json?id=42&q=hi+there%2F%26x"
    );
}

#[test]
fn next_data_url_end_to_end_with_discovered_build_id() {
    // Discover the id from the pages fixture, then build the replay URL.
    let build_id = discover_build_id(NEXT_PAGES).expect("build id present");
    let query = vec![("id".to_string(), "4000".to_string())];
    assert_eq!(
        next_data_url(&build_id, "/products/4000", &query),
        "/_next/data/aBcD1234xyz789KLMNOpq/products/4000.json?id=4000"
    );
}

// -------------------------------------------------------------------
// Tier 1: app-router detection
// -------------------------------------------------------------------

#[test]
fn app_router_is_detected() {
    assert!(is_app_router(NEXT_APP_ROUTER));
}

#[test]
fn pages_router_is_not_app_router() {
    assert!(!is_app_router(NEXT_PAGES));
}

#[test]
fn non_next_pages_are_not_app_router() {
    assert!(!is_app_router(JSONLD_PRODUCT));
    assert!(!is_app_router(NUXT_OBJECT));
    assert!(!is_app_router(PLAIN_SPA));
    assert!(!is_app_router(""));
}
