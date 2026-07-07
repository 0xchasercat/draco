//! Real-world reproduction harness (not a CI test — gated on `CHASER_DIR`).
//!
//! Loads a real SvelteKit chunk graph captured from disk and drives the Tier 2
//! engine against it, reproducing the field panic on chaser.sh
//! (`RefCell already borrowed` in deno_core's dynamic-import host callback).
//!
//! Run with:
//!   CHASER_DIR=/tmp/chaser_chunks cargo test -p draco-runtime \
//!     --test chaser_repro -- --nocapture --test-threads=1
//!
//! The directory must contain `index.html` and `manifest.json`
//! ({ absolute_url -> local_file }). Absent env var => the test no-ops.

use std::collections::HashMap;
use std::sync::Arc;

use draco_runtime::{run_capture_with_resources_and_loader, CaptureConfig};

#[test]
fn chaser_sh_real_chunk_graph_does_not_abort() {
    let Ok(dir) = std::env::var("CHASER_DIR") else {
        eprintln!("CHASER_DIR unset; skipping real-world repro");
        return;
    };

    let html = std::fs::read_to_string(format!("{dir}/index.html")).expect("index.html");
    let manifest: HashMap<String, String> =
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/manifest.json")).unwrap())
            .unwrap();

    let mut full: HashMap<String, Vec<u8>> = HashMap::new();
    for (url, file) in &manifest {
        let bytes = std::fs::read(file).unwrap_or_else(|_| panic!("read {file}"));
        full.insert(url.clone(), bytes);
    }

    // CHASER_SPLIT=1 reproduces the ACTUAL field state: draco's static prefetch
    // scanner misses the minified `../chunks/HASH.js` static imports, so the
    // prefetch map holds only entry + nodes; the on-demand supervisor loader must
    // recover every missing `chunks/` module. Pre-fix this produced the phantom
    // "does not provide an export named 's'" and 0 endpoints.
    let split = std::env::var("CHASER_SPLIT").is_ok();
    let resources: HashMap<String, Vec<u8>> = if split {
        full.iter()
            .filter(|(u, _)| !u.contains("/immutable/chunks/"))
            .map(|(u, b)| (u.clone(), b.clone()))
            .collect()
    } else {
        full.clone()
    };
    eprintln!(
        "prefetch map: {} chunks ({}); on-demand loader can serve all {}",
        resources.len(),
        if split {
            "SPLIT: chunks/ withheld"
        } else {
            "full"
        },
        full.len()
    );

    // The supervisor-backed on-demand loader: serves any chunk by URL.
    let map_for_loader = full.clone();
    let loader: Arc<draco_runtime::ScriptLoader> =
        Arc::new(move |url: &str| map_for_loader.get(url).cloned());

    let cfg = CaptureConfig {
        capture_window_ms: 5000,
        quiesce_ms: 250,
        max_intercepts: 64,
        stub_response_json: r#"{"ok":true,"items":[],"data":{}}"#.to_string(),
    };

    let report = run_capture_with_resources_and_loader(
        "https://chaser.sh/",
        &html,
        &cfg,
        resources,
        Some(loader),
    );

    eprintln!("outcome: {:?}", report.outcome);
    eprintln!("intercepts: {}", report.requests.len());
    for r in &report.requests {
        eprintln!("  {} {}", r.method, r.url);
    }
    eprintln!("--- runtime logs ({}) ---", report.logs.len());
    for l in &report.logs {
        eprintln!("  {l}");
    }
    // The bar: we must not abort. Any outcome (even Threw) beats a child panic.
    // If deno_core re-enters and panics, this test aborts the process — which is
    // exactly the field failure we are hunting.
}
