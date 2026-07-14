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

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use draco_runtime::{run_capture, CaptureConfig, ScriptFetcher, SharedSource};

mod common;
use common::{fn_fetcher, map_fetcher};

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

struct DelayedCountingFetcher {
    resources: HashMap<String, SharedSource>,
    counts: Rc<RefCell<HashMap<String, usize>>>,
}

struct CancellationFetcher;

impl ScriptFetcher for CancellationFetcher {
    fn fetch<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<SharedSource>> + 'a>> {
        Box::pin(async move {
            if url.ends_with("/slow.js") {
                tokio::time::sleep(Duration::from_secs(2)).await;
                return Some(SharedSource::from(&b"export const slow = true;"[..]));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
            None
        })
    }
}

impl ScriptFetcher for DelayedCountingFetcher {
    fn fetch<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<SharedSource>> + 'a>> {
        *self.counts.borrow_mut().entry(url.to_string()).or_default() += 1;
        let source = self.resources.get(url).map(Arc::clone);
        let delayed = url.ends_with("/x.js");
        Box::pin(async move {
            if delayed {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            source
        })
    }
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

    let report = run_capture("https://chaser.test/", html, &cfg(), map_fetcher(resources));

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
  Promise.all([import("/app/x.js"), import("/app/y.js")]).then(
    () => fetch("/api/classic-imports-ok"),
    () => fetch("/api/classic-imports-failed")
  );
</script>
<script type="module">
  Promise.all([import("/app/x.js"), import("/app/z.js")]).then(
    () => fetch("/api/module-imports-ok"),
    () => fetch("/api/module-imports-failed")
  );
</script>
</body></html>"#;

    let resources: HashMap<String, SharedSource> = res(&[
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
    ])
    .into_iter()
    .map(|(url, source)| (url, source.into()))
    .collect();
    let fetch_counts = Rc::new(RefCell::new(HashMap::<String, usize>::new()));

    let report = run_capture(
        "https://interleave.test/",
        html,
        &cfg(),
        Rc::new(DelayedCountingFetcher {
            resources,
            counts: Rc::clone(&fetch_counts),
        }),
    );
    let got = urls(&report);
    for want in [
        "/api/from-x",
        "/api/from-y",
        "/api/from-z",
        "/api/classic-imports-ok",
        "/api/module-imports-ok",
    ] {
        assert!(
            got.iter().any(|u| u.contains(want)),
            "missing {want}; got {got:?}; logs: {:?}",
            report.logs
        );
    }
    assert!(
        !got.iter().any(|url| url.contains("imports-failed")),
        "a concurrent import promise rejected: {got:?}"
    );
    assert_eq!(
        fetch_counts
            .borrow()
            .get("https://interleave.test/app/x.js")
            .copied(),
        Some(1),
        "shared module source should be fetched once"
    );
    assert!(
        !report
            .logs
            .iter()
            .any(|line| line.contains("module loader was asked to reload")
                || line.contains("[raze.module] MISS")),
        "concurrent import hit a loader error path: {:?}",
        report.logs
    );
    let scripts_run = report
        .logs
        .iter()
        .find(|line| line.starts_with("[raze.memory] phase=scripts-run "))
        .expect("scripts-run memory sample");
    assert!(scripts_run.contains("module_registry_bytes=0"));
    let coalescer_settled = report
        .logs
        .iter()
        .find(|line| line.starts_with("[raze.module-fetches] phase=settled "))
        .expect("settled module-fetch coalescer telemetry");
    assert!(
        coalescer_settled.contains("entries=0") && coalescer_settled.contains("retained_bytes=0"),
        "module fetch coalescer retained state after settle: {coalescer_settled}"
    );
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

    let report = run_capture("https://stake.test/", html, &cfg(), map_fetcher(resources));
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

    let loader = |url: &str| {
        if url == "https://stake.test/app/missing-dep.js" {
            Some(b"export const l = 42; fetch(\"/api/from-missing-dep\");".to_vec())
        } else if url == "https://stake.test/app/lazy-route.js" {
            Some(b"fetch(\"/api/from-lazy-route\");".to_vec())
        } else {
            None
        }
    };

    // Old resolution order preserved: prefetch map first, then on-demand loader.
    let report = run_capture(
        "https://stake.test/",
        html,
        &cfg(),
        fn_fetcher(move |url| resources.get(url).cloned().or_else(|| loader(url))),
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

    let loader = |url: &str| {
        if url == "https://stake.test/app/lazy-route.js" {
            Some(b"fetch(\"/api/from-lazy-route\");".to_vec())
        } else {
            None
        }
    };

    // Prefetch map was empty (`res(&[])`), so the closure alone is the fetcher.
    let report = run_capture("https://stake.test/", html, &cfg(), fn_fetcher(loader));
    let got = urls(&report);
    for want in ["/api/from-lazy-route", "/api/route-loaded"] {
        assert!(
            got.iter().any(|u| u.contains(want)),
            "missing {want}; got {got:?}; logs: {:?}",
            report.logs
        );
    }
}

#[test]
fn duplicate_static_and_dynamic_imports_use_v8_module_map_without_refetch() {
    let html = r#"<!doctype html><html><body>
<script type="module">
  import { a } from "/app/a.js";
  import { b } from "/app/b.js";
  Promise.all([import("/app/shared.js"), import("/app/shared.js")])
    .then(([first, second]) => fetch(`/api/done?v=${a + b + first.value + second.value}`));
</script>
</body></html>"#;
    let resources = Rc::new(res(&[
        (
            "https://dedup.test/app/a.js",
            r#"import { value } from "/app/shared.js"; export const a = value;"#,
        ),
        (
            "https://dedup.test/app/b.js",
            r#"import { value } from "/app/shared.js"; export const b = value;"#,
        ),
        (
            "https://dedup.test/app/shared.js",
            "export const value = 3;",
        ),
    ]));
    let fetch_counts = Rc::new(RefCell::new(HashMap::<String, usize>::new()));
    let resources_for_fetch = Rc::clone(&resources);
    let counts_for_fetch = Rc::clone(&fetch_counts);

    let report = run_capture(
        "https://dedup.test/",
        html,
        &cfg(),
        fn_fetcher(move |url| {
            *counts_for_fetch
                .borrow_mut()
                .entry(url.to_string())
                .or_default() += 1;
            resources_for_fetch.get(url).cloned()
        }),
    );

    assert!(
        urls(&report)
            .iter()
            .any(|url| url.contains("/api/done?v=12")),
        "duplicate imports did not evaluate correctly: {:?}",
        report.logs
    );
    assert_eq!(
        fetch_counts
            .borrow()
            .get("https://dedup.test/app/shared.js")
            .copied(),
        Some(1),
        "V8's module map should own duplicate imports after the first load"
    );
    let scripts_run = report
        .logs
        .iter()
        .find(|line| line.starts_with("[raze.memory] phase=scripts-run "))
        .expect("scripts-run memory sample");
    assert!(
        scripts_run.contains("module_registry_bytes=0"),
        "raw module sources remained after V8 accepted them: {scripts_run}"
    );
}

#[test]
fn failed_module_graph_does_not_retain_raw_entry_source() {
    let html = r#"<!doctype html><html><body>
<script type="module" src="/app/bad-entry.js"></script>
</body></html>"#;
    let resources = res(&[(
        "https://failed-load.test/app/bad-entry.js",
        r#"import "/app/missing.js"; export const unreachable = true;"#,
    )]);

    let report = run_capture(
        "https://failed-load.test/",
        html,
        &cfg(),
        map_fetcher(resources),
    );
    assert!(
        report
            .logs
            .iter()
            .any(|line| line.contains("failed to load module")),
        "fixture did not exercise a module load failure: {:?}",
        report.logs
    );
    let scripts_run = report
        .logs
        .iter()
        .find(|line| line.starts_with("[raze.memory] phase=scripts-run "))
        .expect("scripts-run memory sample");
    assert!(
        scripts_run.contains("module_registry_bytes=0"),
        "failed graph retained its raw entry source: {scripts_run}"
    );
}

#[test]
fn failed_graph_cancels_sibling_fetch_without_leaking_inflight_state() {
    let html = r#"<!doctype html><html><body>
<script type="module">
  import "/app/missing.js";
  import "/app/slow.js";
</script>
</body></html>"#;
    let mut config = cfg();
    config.capture_window_ms = 800;
    config.quiesce_ms = 50;

    let started = Instant::now();
    let report = run_capture(
        "https://cancel.test/",
        html,
        &config,
        Rc::new(CancellationFetcher),
    );

    assert!(
        started.elapsed() < Duration::from_millis(500),
        "cancelled sibling pinned capture until the hard cap: {:?}",
        started.elapsed()
    );
    assert!(
        report
            .logs
            .iter()
            .any(|line| line.contains("failed to load module")),
        "fixture did not fail its module graph: {:?}",
        report.logs
    );
    let settled = report
        .logs
        .iter()
        .find(|line| line.starts_with("[raze.module-fetches] phase=settled "))
        .expect("settled coalescer telemetry");
    assert!(
        settled.contains("entries=0")
            && settled.contains("retained_bytes=0")
            && settled.contains("inflight=0"),
        "cancelled module fetch leaked state: {settled}"
    );
}
