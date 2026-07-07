//! ESM dynamic-import regression tests (v0.13.8).
//!
//! Field failures these encode:
//!
//! 1. **chaser.sh** — `RefCell already borrowed` abort in deno_core's dynamic
//!    import host callback (`ModuleMap::next_load_id`, map.rs:290) when a
//!    SvelteKit-shaped module graph fires nested `import()` while an earlier
//!    dynamic import is still being serviced. The child aborted
//!    (`panic in a function that cannot unwind`), surfacing as
//!    `jail/Protocol: child closed IPC before sending a Result`.
//!
//! 2. **stake.com** — a dynamically imported chunk with a *static* dependency
//!    that was not in the prefetch map got that dependency served as a silent
//!    **empty module**, producing the phantom
//!    `SyntaxError: The requested module './X.js' does not provide an export
//!    named 'l'` and killing hydration (0 intercepts).
//!
//! All fixtures are offline; graphs are shaped like real SvelteKit/Vite output:
//! inline `<script type="module">` bootstrap -> static import diamond ->
//! `Promise.all([import(), import()])` route nodes -> nested `import()` inside
//! chunk top-level evaluation.

use std::collections::HashMap;
use std::sync::Arc;

use draco_runtime::{
    run_capture_with_resources, run_capture_with_resources_and_loader, CaptureConfig,
};

fn cfg() -> CaptureConfig {
    CaptureConfig {
        capture_window_ms: 3000,
        quiesce_ms: 200,
        max_intercepts: 64,
        stub_response_json: r#"{"ok":true}"#.to_string(),
    }
}

fn res(pairs: &[(&str, &str)]) -> HashMap<String, Vec<u8>> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
        .collect()
}

fn urls(report: &draco_runtime::CaptureReport) -> Vec<String> {
    report.requests.iter().map(|r| r.url.clone()).collect()
}

/// chaser.sh shape: inline module bootstrap statically imports the entry, whose
/// evaluation kicks off `Promise.all` dynamic imports; those chunks fire further
/// `import()` at *top level of their own evaluation* (the exact re-entrancy that
/// aborted the child), plus a static diamond underneath.
#[test]
fn nested_dynamic_imports_sveltekit_shape_do_not_abort() {
    let html = r#"<!doctype html><html><head></head><body>
<div id="app"></div>
<script type="module">
  import { start } from "/_app/entry/start.js";
  start({ node_ids: [0, 2] });
</script>
</body></html>"#;

    let resources = res(&[
        (
            "https://chaser.test/_app/entry/start.js",
            r#"import { shared } from "/_app/chunks/shared.js";
export function start(opts) {
  Promise.all([import("/_app/nodes/0.js"), import("/_app/nodes/2.js")])
    .then(([a, b]) => { a.mount(); b.mount(); shared(); });
}"#,
        ),
        (
            "https://chaser.test/_app/chunks/shared.js",
            r#"import { util } from "/_app/chunks/util.js";
export function shared() { fetch("/api/from-shared"); util(); }"#,
        ),
        (
            "https://chaser.test/_app/chunks/util.js",
            r#"export function util() {}"#,
        ),
        (
            // node 0: fires a further dynamic import DURING its own top-level
            // evaluation — this is the frame that re-entered the host callback.
            "https://chaser.test/_app/nodes/0.js",
            r#"import { util } from "/_app/chunks/util.js";
import("/_app/chunks/lazy-a.js");
export function mount() { fetch("/api/from-node0"); }"#,
        ),
        (
            "https://chaser.test/_app/nodes/2.js",
            r#"import("/_app/chunks/lazy-b.js").then((m) => m.late());
export function mount() { fetch("/api/from-node2"); }"#,
        ),
        (
            // lazy-a itself dynamically imports lazy-c: two levels of nesting.
            "https://chaser.test/_app/chunks/lazy-a.js",
            r#"import("/_app/chunks/lazy-c.js");
fetch("/api/from-lazy-a");"#,
        ),
        (
            "https://chaser.test/_app/chunks/lazy-b.js",
            r#"export function late() { fetch("/api/from-lazy-b"); }"#,
        ),
        (
            "https://chaser.test/_app/chunks/lazy-c.js",
            r#"fetch("/api/from-lazy-c");"#,
        ),
    ]);

    let report = run_capture_with_resources("https://chaser.test/", html, &cfg(), resources);

    let got = urls(&report);
    for want in [
        "/api/from-shared",
        "/api/from-node0",
        "/api/from-node2",
        "/api/from-lazy-a",
        "/api/from-lazy-b",
        "/api/from-lazy-c",
    ] {
        assert!(
            got.iter().any(|u| u.contains(want)),
            "missing {want}; got {got:?}; logs: {:?}",
            report.logs
        );
    }
}

/// Rapid-fire import() from a *classic* script while module evaluation from a
/// prior import is still pending — the interleaving stress case.
#[test]
fn interleaved_classic_and_module_dynamic_imports_do_not_abort() {
    let html = r#"<!doctype html><html><body>
<script>
  Promise.all([import("/app/x.js"), import("/app/y.js")]).then(() => {});
</script>
<script type="module">
  import("/app/x.js");
  import("/app/z.js");
</script>
</body></html>"#;

    let resources = res(&[
        (
            "https://interleave.test/app/x.js",
            r#"import("/app/z.js"); fetch("/api/from-x");"#,
        ),
        (
            "https://interleave.test/app/y.js",
            r#"fetch("/api/from-y");"#,
        ),
        (
            "https://interleave.test/app/z.js",
            r#"fetch("/api/from-z");"#,
        ),
    ]);

    let report = run_capture_with_resources("https://interleave.test/", html, &cfg(), resources);
    let got = urls(&report);
    for want in ["/api/from-x", "/api/from-y", "/api/from-z"] {
        assert!(
            got.iter().any(|u| u.contains(want)),
            "missing {want}; got {got:?}; logs: {:?}",
            report.logs
        );
    }
}

/// stake.com shape *without* a supervisor loader: a dynamically imported chunk
/// has a static dep missing from the map. The import must reject with an honest
/// module-load error — NOT hydrate a silent empty module and blame the page
/// with a phantom "does not provide an export named" SyntaxError. And the rest
/// of the page must keep running.
#[test]
fn missing_static_dep_rejects_honestly_without_loader() {
    let html = r#"<!doctype html><html><body>
<script type="module">
  import("/app/chunk.js").then(
    () => fetch("/api/unexpected-ok"),
    (e) => fetch("/api/err?m=" + encodeURIComponent(String(e)))
  );
  fetch("/api/page-alive");
</script>
</body></html>"#;

    let resources = res(&[(
        "https://stake.test/app/chunk.js",
        r#"import { l } from "/app/missing.js";
export const x = l;
fetch("/api/from-chunk");"#,
    )]);

    let report = run_capture_with_resources("https://stake.test/", html, &cfg(), resources);
    let got = urls(&report);

    assert!(
        got.iter().any(|u| u.contains("/api/page-alive")),
        "page should keep running; got {got:?}"
    );
    let err = got
        .iter()
        .find(|u| u.contains("/api/err"))
        .unwrap_or_else(|| panic!("import() should reject; got {got:?} logs {:?}", report.logs));
    assert!(
        !err.contains("does%20not%20provide%20an%20export"),
        "phantom export error leaked through: {err}"
    );
    assert!(
        !got.iter().any(|u| u.contains("/api/unexpected-ok")),
        "import() of a chunk with a missing static dep must not resolve; got {got:?}"
    );
}

/// stake.com shape *with* the supervisor loader: the missing static dep is
/// fetchable on demand (that is exactly what the supervisor's LoadScript path
/// is for) — the dynamic import must consult it and hydrate for real.
#[test]
fn missing_deps_are_fetched_through_script_loader() {
    let html = r#"<!doctype html><html><body>
<script type="module">
  import("/app/chunk.js").then((m) => m.go());
</script>
</body></html>"#;

    let resources = res(&[(
        "https://stake.test/app/chunk.js",
        r#"import { l } from "/app/missing-dep.js";
export function go() { fetch("/api/chunk-ok?l=" + l); }"#,
    )]);

    let loader: Arc<draco_runtime::ScriptLoader> = Arc::new(|url: &str| {
        if url == "https://stake.test/app/missing-dep.js" {
            Some(b"export const l = 42; fetch(\"/api/from-missing-dep\");".to_vec())
        } else if url == "https://stake.test/app/lazy-route.js" {
            Some(b"fetch(\"/api/from-lazy-route\");".to_vec())
        } else {
            None
        }
    });

    let report = run_capture_with_resources_and_loader(
        "https://stake.test/",
        html,
        &cfg(),
        resources,
        Some(loader),
    );
    let got = urls(&report);
    for want in ["/api/chunk-ok?l=42", "/api/from-missing-dep"] {
        assert!(
            got.iter().any(|u| u.contains(want)),
            "missing {want}; got {got:?}; logs: {:?}",
            report.logs
        );
    }
}

/// A whole lazy route chunk absent from the prefetch map, resolvable only via
/// the supervisor loader (SvelteKit lazy routes on stake.com: 37 prefetched
/// scripts, many more pulled at runtime through import()).
#[test]
fn dynamic_import_of_unprefetched_chunk_uses_script_loader() {
    let html = r#"<!doctype html><html><body>
<script type="module">
  import("/app/lazy-route.js").then(
    () => fetch("/api/route-loaded"),
    (e) => fetch("/api/route-err?m=" + encodeURIComponent(String(e)))
  );
</script>
</body></html>"#;

    let loader: Arc<draco_runtime::ScriptLoader> = Arc::new(|url: &str| {
        if url == "https://stake.test/app/lazy-route.js" {
            Some(b"fetch(\"/api/from-lazy-route\");".to_vec())
        } else {
            None
        }
    });

    let report = run_capture_with_resources_and_loader(
        "https://stake.test/",
        html,
        &cfg(),
        res(&[]),
        Some(loader),
    );
    let got = urls(&report);
    for want in ["/api/from-lazy-route", "/api/route-loaded"] {
        assert!(
            got.iter().any(|u| u.contains(want)),
            "missing {want}; got {got:?}; logs: {:?}",
            report.logs
        );
    }
}
