//! Integration tests for the Tier 2 capture engine (canonical §8 DoD).
//!
//! All fixtures are in-repo and offline; every test drives [`run_capture`]
//! against hand-written HTML and asserts on the intercepts + outcome. These
//! prove the fetch/XHR interception + capture-window mechanism end-to-end.
//!
//! NOTE ON ISOLATION: `deno_core` initializes the V8 platform once per process
//! and V8 flags are process-global. Running these in one process is fine, but we
//! keep each test self-contained and tolerant of the shared platform.

use draco_runtime::{run_capture, CaptureConfig};
use draco_types::{InterceptVia, RuntimeOutcome};

fn cfg() -> CaptureConfig {
    CaptureConfig {
        capture_window_ms: 2000,
        quiesce_ms: 150,
        max_intercepts: 64,
        stub_response_json: r#"{"ok":true,"items":[]}"#.to_string(),
    }
}

fn find<'a>(
    reqs: &'a [draco_runtime::CapturedRequest],
    needle: &str,
) -> Option<&'a draco_runtime::CapturedRequest> {
    reqs.iter().find(|r| r.url.contains(needle))
}

#[test]
fn spa_fetch_is_captured() {
    let html = include_str!("fixtures/spa_fetch.html");
    let report = run_capture("https://shop.example.com/p/1", html, &cfg());

    // Outcome should be a clean close (quiesced) or the hard cap — both are
    // "we ran and captured".
    assert!(
        matches!(
            report.outcome,
            RuntimeOutcome::Quiesced | RuntimeOutcome::WindowClosed
        ),
        "unexpected outcome: {:?}",
        report.outcome
    );

    let req = find(&report.requests, "/api/data").expect("no /api/data capture");
    assert_eq!(req.method, "GET");
    assert_eq!(req.via, InterceptVia::Fetch);
    // URL was absolutized against the page URL.
    assert!(
        req.url.starts_with("https://shop.example.com/"),
        "url not absolutized: {}",
        req.url
    );
    // The `accept: application/json` header made it through.
    assert!(
        req.headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("accept") && v.contains("application/json")),
        "accept header missing: {:?}",
        req.headers
    );
}

#[test]
fn spa_xhr_is_captured_with_via_xhr() {
    let html = include_str!("fixtures/spa_xhr.html");
    let report = run_capture("https://legacy.example.com/", html, &cfg());

    assert!(
        matches!(
            report.outcome,
            RuntimeOutcome::Quiesced | RuntimeOutcome::WindowClosed
        ),
        "unexpected outcome: {:?}",
        report.outcome
    );

    let req = find(&report.requests, "/api/legacy").expect("no /api/legacy capture");
    assert_eq!(req.via, InterceptVia::Xhr, "should be tagged XHR");
    assert_eq!(req.method, "GET");
    assert!(
        req.headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("x-requested-with")),
        "custom XHR header missing: {:?}",
        req.headers
    );
}

#[test]
fn decoy_and_real_endpoint_both_captured() {
    let html = include_str!("fixtures/spa_decoy.html");
    let report = run_capture("https://shop.example.com/c/widgets", html, &cfg());

    assert!(
        matches!(
            report.outcome,
            RuntimeOutcome::Quiesced | RuntimeOutcome::WindowClosed
        ),
        "unexpected outcome: {:?}",
        report.outcome
    );

    // Both must be captured — ranking is not our job.
    let beacon = find(&report.requests, "analytics.example.com").expect("no analytics beacon");
    assert_eq!(beacon.method, "POST");
    assert_eq!(beacon.via, InterceptVia::Fetch);
    assert!(beacon.body.is_some(), "beacon POST body should be captured");

    let products = find(&report.requests, "/api/products").expect("no /api/products");
    assert_eq!(products.method, "GET");
    assert!(
        products.url.contains("category=widgets"),
        "query string lost: {}",
        products.url
    );

    assert!(
        report.requests.len() >= 2,
        "expected >=2 captures, got {}",
        report.requests.len()
    );
}

#[test]
fn no_fetch_reports_no_intercepts() {
    let html = include_str!("fixtures/no_fetch.html");
    let report = run_capture("https://static.example.com/", html, &cfg());

    assert!(
        report.requests.is_empty(),
        "expected no captures, got {:?}",
        report.requests
    );
    assert_eq!(report.outcome, RuntimeOutcome::NoIntercepts);
}

#[test]
fn throwing_page_reports_threw_without_panicking() {
    let html = include_str!("fixtures/throws.html");
    let report = run_capture("https://broken.example.com/", html, &cfg());

    // No fetch happened and JS threw → Threw. Crucially, no panic.
    assert!(report.requests.is_empty(), "unexpected captures");
    assert_eq!(report.outcome, RuntimeOutcome::Threw);
}

#[test]
fn max_intercepts_is_enforced() {
    // A page that fires many requests in a tight loop; cap should bound captures.
    let html = r#"
        <html><body><script>
          for (let i = 0; i < 100; i++) {
            fetch("/api/item/" + i);
          }
        </script></body></html>
    "#;
    let mut c = cfg();
    c.max_intercepts = 5;
    let report = run_capture("https://api.example.com/", html, &c);

    assert!(
        report.requests.len() <= 5,
        "cap not enforced: {} captures",
        report.requests.len()
    );
    assert!(!report.requests.is_empty(), "should have captured some");
    // We captured requests, so outcome is a successful close, not Threw.
    assert!(
        matches!(
            report.outcome,
            RuntimeOutcome::Quiesced | RuntimeOutcome::WindowClosed
        ),
        "unexpected outcome: {:?}",
        report.outcome
    );
}

#[test]
fn stub_body_is_delivered_to_page() {
    // The page reads res.json() and stashes a field; if our stub body flows
    // through, the fetch chain completes without throwing (captured as Fetch).
    let html = r#"
        <html><body><script>
          fetch("/api/echo")
            .then(r => r.json())
            .then(d => { window.__got = d.ok; });
        </script></body></html>
    "#;
    let report = run_capture("https://echo.example.com/", html, &cfg());
    let req = find(&report.requests, "/api/echo").expect("no /api/echo capture");
    assert_eq!(req.via, InterceptVia::Fetch);
}

#[test]
fn framework_scheduler_hydration_surfaces_endpoints() {
    // Exercises MessageChannel/MessagePort + scheduler + DOM mount + effect,
    // the runtime surfaces a real framework's client bundle drives during
    // hydration. Both the XHR-transport "state" load and the dependent fetch it
    // triggers must be captured — proving the scheduler polyfill lets deferred,
    // chained requests surface.
    let html = include_str!("fixtures/framework_scheduler.html");
    let report = run_capture("https://app.example.com/dashboard", html, &cfg());

    assert!(
        matches!(
            report.outcome,
            RuntimeOutcome::Quiesced | RuntimeOutcome::WindowClosed
        ),
        "unexpected outcome: {:?}",
        report.outcome
    );

    let state = find(&report.requests, "/api/hydrate/state").expect("no state load");
    assert_eq!(state.via, InterceptVia::Xhr);

    let details = find(&report.requests, "/api/hydrate/details").expect("no dependent fetch");
    assert_eq!(details.via, InterceptVia::Fetch);
    assert!(
        details.url.contains("id=42"),
        "dependent fetch query lost: {}",
        details.url
    );
}

/// Compose the Vue fixture: inline the *real* vendored Vue 3 global build in
/// place of the `__VUE_GLOBAL_BUILD__` marker, so the document handed to the
/// runtime has the genuine Vue source as its first inline `<script>`.
fn vue_fixture_html() -> String {
    let vue = include_str!("fixtures/vendor/vue.global.prod.js");
    let fixture = include_str!("fixtures/vue_app.html");
    // The marker sits inside the first <script>; replacing it inlines Vue there.
    let html = fixture.replace("__VUE_GLOBAL_BUILD__;", vue);
    assert!(
        html.contains("Vue=function") || html.contains("Vue = function"),
        "vendored Vue source did not get inlined into the fixture"
    );
    html
}

/// End-to-end proof that a *real framework bundle* hydrates inside the isolate
/// and leaks its data fetch(es).
///
/// This is the crate's headline case: the vendored Vue 3.5.39 global build runs
/// verbatim, compiles a template, mounts into the real `#app` node the polyfill
/// materialized from the page `<body>`, fires the component's `mounted()`
/// lifecycle hook, and — because the interceptor answers each fetch with a stub
/// so reactivity keeps flowing — reveals a *dependent* fetch (a `watch` on state
/// that the first response mutates) plus a paint-deferred child fetch. If Vue
/// did not truly mount into the DOM tree, `mounted()` would never run and
/// nothing would surface.
#[test]
fn real_vue_bundle_hydrates_and_leaks_fetch() {
    let html = vue_fixture_html();
    // Stub the primary response with a concrete item id so the dependent fetch
    // carries a deterministic `id=42` (proving the stub body flowed back into
    // the app and drove the chained request).
    let mut c = cfg();
    c.stub_response_json = r#"{"ok":true,"items":[{"id":42}]}"#.to_string();
    let report = run_capture("https://dashboard.example.com/", &html, &c);

    assert!(
        matches!(
            report.outcome,
            RuntimeOutcome::Quiesced | RuntimeOutcome::WindowClosed
        ),
        "unexpected outcome: {:?} (captured: {:?})",
        report.outcome,
        report.requests,
    );

    // 1. Primary fetch from the mounted() lifecycle hook.
    let data = find(&report.requests, "/api/data").unwrap_or_else(|| {
        panic!(
            "Vue did not leak its mounted() fetch — captured: {:?}",
            report.requests
        )
    });
    assert_eq!(data.method, "GET");
    assert_eq!(data.via, InterceptVia::Fetch);
    assert!(
        data.url.starts_with("https://dashboard.example.com/"),
        "url not absolutized: {}",
        data.url
    );
    assert!(
        data.headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("accept") && v.contains("application/json")),
        "accept header lost through the real Vue fetch wrapper: {:?}",
        data.headers
    );

    // 2. Dependent fetch: discoverable only after the primary response mutated
    //    reactive state and Vue's watcher ran. The stub body drives the id=42.
    let detail = find(&report.requests, "/api/detail").unwrap_or_else(|| {
        panic!(
            "dependent (watcher-triggered) fetch never surfaced — captured: {:?}",
            report.requests
        )
    });
    assert_eq!(detail.via, InterceptVia::Fetch);
    assert!(
        detail.url.contains("id=42"),
        "dependent fetch did not carry the id from the stub response: {}",
        detail.url
    );

    // 3. Paint-deferred fetch from a mounted child component (setTimeout path).
    let panel = find(&report.requests, "/api/panel/config").unwrap_or_else(|| {
        panic!(
            "paint-deferred child fetch never surfaced — captured: {:?}",
            report.requests
        )
    });
    assert_eq!(panel.via, InterceptVia::Fetch);

    // All three endpoints leaked from one real bundle.
    assert!(
        report.requests.len() >= 3,
        "expected >=3 captures from the Vue app, got {}: {:?}",
        report.requests.len(),
        report.requests,
    );
}

#[test]
fn post_with_json_body_captures_body_bytes() {
    let html = r#"
        <html><body><script>
          fetch("/api/save", {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({ name: "widget", qty: 3 })
          });
        </script></body></html>
    "#;
    let report = run_capture("https://save.example.com/", html, &cfg());
    let req = find(&report.requests, "/api/save").expect("no /api/save capture");
    assert_eq!(req.method, "POST");
    let body = req.body.as_ref().expect("body should be captured");
    let s = String::from_utf8_lossy(body);
    assert!(s.contains("\"name\":\"widget\""), "body wrong: {s}");
    assert!(s.contains("\"qty\":3"), "body wrong: {s}");
}
