//! # draco-jail
//!
//! Sandbox supervisor + jailed child for Draco's Tier 2 runtime. Implements the
//! security model of canonical spec §7:
//!
//! * a **user + network namespace air-gap** so the child has no routable network
//!   and no ambient host UID/GID,
//! * a **Landlock** filesystem lockdown (best-effort; needs kernel ≥ 5.13),
//! * a **two-phase seccomp-bpf** filter (default `KILL_PROCESS`) that whitelists
//!   only the syscalls the runtime needs, and
//! * the **self-re-exec host** (`draco __jail`) that turns the current binary into
//!   the jailed child, inheriting the IPC socket as **fd 3**.
//!
//! The IPC frame codec (spec §6) lives in [`frame`] and is fully portable and
//! unit-tested. The jail *mechanics* are Linux-only; on other platforms the crate
//! degrades to running the payload **un-jailed** with a loud warning (see
//! [`spawn_jail`] / [`run_jail_child`]).
//!
//! ## Slice 2 scope
//!
//! This is the sandbox *scaffold*. The child payload here is a trivial echo loop,
//! **not** a V8 isolate — deno_core/V8 arrives in Slice 3. The goal is a correct,
//! compiling jail whose runtime enforcement (seccomp kills, netns, Landlock) is
//! validated on bare-metal Linux (≥ 5.13 with unprivileged user namespaces);
//! those behaviours cannot be exercised in every CI sandbox and their tests are
//! marked `#[ignore]`.
//!
//! **Frozen public API** — the signatures of [`JailHandle`], [`JailError`],
//! [`spawn_jail`], and [`run_jail_child`] are fixed by the workspace contract.
#![allow(dead_code)]

pub mod frame;

pub(crate) mod payload;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(not(target_os = "linux"))]
mod degraded;

use std::os::unix::net::UnixStream;

use draco_types::JailKind;

/// Handle to a spawned jailed child, owned by the supervisor.
///
/// Holds the child pid and the supervisor's end of the fd-3 IPC socketpair.
/// Dropping the handle closes the IPC stream (signalling the child to exit) but
/// does **not** reap the child; call [`JailHandle::wait`] for that.
#[derive(Debug)]
pub struct JailHandle {
    /// PID of the jailed child process.
    pub pid: i32,
    /// Supervisor-side end of the bidirectional IPC channel. The child sees the
    /// peer as fd 3.
    pub ipc: UnixStream,
}

impl JailHandle {
    /// The child's process id.
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Borrow the supervisor-side IPC stream for reading/writing frames.
    pub fn ipc(&mut self) -> &mut UnixStream {
        &mut self.ipc
    }
}

/// Error from jail setup or operation. Mirrors the `DracoError::Jail` shape so
/// the supervisor can surface it directly in an `ExtractionResult`.
#[derive(Debug, Clone)]
pub struct JailError {
    /// Structured cause (spawn failure, seccomp install failure, killed, …).
    pub reason: JailKind,
    /// Human-readable detail.
    pub detail: String,
}

impl JailError {
    /// Construct a [`JailError`] with the given reason and detail.
    pub fn new(reason: JailKind, detail: impl Into<String>) -> Self {
        JailError {
            reason,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for JailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "jail error [{:?}]: {}", self.reason, self.detail)
    }
}

impl std::error::Error for JailError {}

/// Supervisor-side: create the socketpair, set up namespaces, and spawn the
/// jailed child by re-exec'ing this binary as `draco __jail`.
///
/// On Linux the child is placed in a fresh user + network namespace with tight
/// rlimits and its IPC socket dup'd to fd 3. On non-Linux hosts this returns a
/// handle to an un-jailed child after logging a warning (dev-only path).
pub fn spawn_jail() -> Result<JailHandle, JailError> {
    #[cfg(target_os = "linux")]
    {
        linux::spawn_jail()
    }
    #[cfg(not(target_os = "linux"))]
    {
        degraded::spawn_jail()
    }
}

/// Child-side entry, invoked when the binary re-execs itself as `draco __jail`.
///
/// Opens the inherited fd-3 socket, applies the namespace/Landlock/seccomp
/// lockdown, then runs the Slice 2 payload loop (read a frame, echo a reply).
/// Never returns: it exits the process on a clean shutdown or on any fatal setup
/// error.
pub fn run_jail_child() -> ! {
    #[cfg(target_os = "linux")]
    {
        linux::run_jail_child()
    }
    #[cfg(not(target_os = "linux"))]
    {
        degraded::run_jail_child()
    }
}

/// The raw fd the child inherits its IPC socket on (canonical spec §6/§7).
pub const JAIL_IPC_FD: i32 = 3;
