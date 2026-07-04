//! # draco-jail (STUB — Slice 2 spike)
//!
//! Sandbox supervisor + jailed child. Implement against canonical spec §7:
//! user+network namespace air-gap, Landlock FS lockdown, two-phase seccomp-bpf
//! (default KILL), and the self-re-exec host for `draco __jail`. Linux-only;
//! degrade to un-jailed with a loud warning on macOS.
//!
//! **Frozen public API** — fill in the bodies; do not change the signatures.
#![allow(dead_code, unused_variables)]

use draco_types::JailKind;

/// Handle to a spawned jailed child (pid + IPC endpoint), owned by the supervisor.
#[derive(Debug)]
pub struct JailHandle {
    // Slice 2 spike: pid, the fd-3 IPC stream, teardown state, etc.
}

/// Error from jail setup/operation.
#[derive(Debug, Clone)]
pub struct JailError {
    pub reason: JailKind,
    pub detail: String,
}

/// Supervisor-side: create the socketpair, set up namespaces, and spawn the child.
pub fn spawn_jail() -> Result<JailHandle, JailError> {
    todo!("Slice 2 spike: supervisor-side spawn per canonical spec §7")
}

/// Child-side entry, invoked when the binary re-execs itself as `draco __jail`.
/// Applies namespaces/Landlock/seccomp, then hosts the runtime and speaks IPC over fd 3.
pub fn run_jail_child() -> ! {
    todo!("Slice 2 spike: child-side jail bootstrap per canonical spec §7")
}
