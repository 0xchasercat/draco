//! Build-time V8 snapshot generation for the Tier 2 DOM engine.
//!
//! The Tier 2 isolate runs a real DOM — **happy-dom** — on a base of ecosystem
//! web-primitive polyfills (see `vendor/happy-dom/`). Parsing those ~2.6 MB of
//! JS on every isolate spawn costs ~95 ms; instead we evaluate them **once** here
//! and serialize the V8 heap into a startup snapshot. At runtime the isolate
//! restores the snapshot in ~single-digit ms with the whole DOM engine resident.
//!
//! The snapshot is heap + compiled-code only — **no ops are baked in**. The base
//! bundle's timer scheduler references `Deno.core.ops.op_sleep` lazily (at
//! call-time, never during snapshot evaluation), so the ops the runtime registers
//! (`op_sleep`, `op_raze_fetch`, `op_resolve_url`, `op_raze_dom`) resolve after
//! restore. Everything evaluated here (polyfills, fake-indexeddb) obeys the same
//! rule: it may *define* functions that call ops later, but must not call an op
//! during evaluation.

use std::path::PathBuf;

use deno_core::{JsRuntimeForSnapshot, RuntimeOptions};

fn main() {
    println!("cargo:rerun-if-changed=js/base.iife.js");
    println!("cargo:rerun-if-changed=js/happydom.iife.js");
    println!("cargo:rerun-if-changed=js/polyfills.js");
    println!("cargo:rerun-if-changed=js/fake-indexeddb.iife.js");
    println!("cargo:rerun-if-changed=build.rs");

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("DRACO_SNAPSHOT.bin");

    let mut rt = JsRuntimeForSnapshot::new(RuntimeOptions::default());
    // Order matters. The base establishes the Node/web global environment (incl.
    // the op_sleep-backed timers happy-dom binds at load); then happy-dom; then
    // our web-platform-completeness layer:
    //   * polyfills.js — inert, install-if-absent stubs for standard globals an
    //     SPA touches at boot (matchMedia, Intersection/ResizeObserver,
    //     requestIdleCallback, storage, …) so hydration never hard-crashes on
    //     `X is not defined`.
    //   * fake-indexeddb.iife.js — a full in-memory IndexedDB engine (+ a
    //     structuredClone polyfill it needs, which bare V8 lacks), install-if-
    //     absent, so IndexedDB-using frameworks (SvelteKit, telemetry) hydrate.
    // Both only fill genuine gaps; real base/happy-dom impls win.
    rt.execute_script("draco:base", include_str!("js/base.iife.js"))
        .expect("evaluate base.iife.js into snapshot");
    rt.execute_script("draco:happydom", include_str!("js/happydom.iife.js"))
        .expect("evaluate happydom.iife.js into snapshot");
    rt.execute_script("draco:polyfills", include_str!("js/polyfills.js"))
        .expect("evaluate polyfills.js into snapshot");
    rt.execute_script(
        "draco:fake-indexeddb",
        include_str!("js/fake-indexeddb.iife.js"),
    )
    .expect("evaluate fake-indexeddb.iife.js into snapshot");

    let blob = rt.snapshot();
    std::fs::write(&out, &blob).expect("write snapshot");
}
