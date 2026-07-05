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
//! restore.

use std::path::PathBuf;

use deno_core::{JsRuntimeForSnapshot, RuntimeOptions};

fn main() {
    println!("cargo:rerun-if-changed=js/base.iife.js");
    println!("cargo:rerun-if-changed=js/happydom.iife.js");
    println!("cargo:rerun-if-changed=build.rs");

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("DRACO_SNAPSHOT.bin");

    let mut rt = JsRuntimeForSnapshot::new(RuntimeOptions::default());
    // Order matters: the base establishes the Node/web global environment (incl.
    // the op_sleep-backed timers happy-dom binds at load), then happy-dom.
    rt.execute_script("draco:base", include_str!("js/base.iife.js"))
        .expect("evaluate base.iife.js into snapshot");
    rt.execute_script("draco:happydom", include_str!("js/happydom.iife.js"))
        .expect("evaluate happydom.iife.js into snapshot");

    let blob = rt.snapshot();
    std::fs::write(&out, &blob).expect("write snapshot");
}
