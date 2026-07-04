//! # draco-runtime (STUB — Slice 3)
//!
//! Tier 2 V8 isolate + interceptor. Implement against canonical spec §8:
//! boot a jitless, single-threaded isolate from a pre-warmed snapshot (built in
//! build.rs from a vendored DOM polyfill + scheduler shims), override
//! `fetch`/`XMLHttpRequest` to route through `op_raze_fetch`, run the capture
//! window, rank-agnostically report every intercept over IPC, then terminate.
//!
//! **Frozen public API** — fill in the bodies; do not change the signatures.
#![allow(dead_code, unused_variables)]

/// Run the Tier 2 runtime loop inside the jailed child: restore the isolate from
/// the snapshot, install interceptors, drive the capture window, and emit
/// `JailToSupervisor` frames. Returns via process exit.
pub fn run_runtime() -> ! {
    todo!("Slice 3: V8 runtime + capture window per canonical spec §8")
}
