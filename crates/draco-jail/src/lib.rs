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
//! ## Scope (Slice 4)
//!
//! The jailed child now hosts the **real** Tier 2 capture: after the sandbox is
//! armed, [`run_jail_child`] reads a `Hydrate` frame and drives
//! `draco_runtime::run_capture` (a V8 isolate + fetch/XHR interceptor), streaming
//! each captured request back as a `JailToSupervisor::Intercept` and a terminal
//! `Result` (see [`runtime_payload`]). The Slice 2 echo ([`payload`]) is retained
//! only for its portable frame-plumbing unit tests.
//!
//! Runtime enforcement of the sandbox (seccomp kills, netns air-gap, Landlock)
//! and the *jailed* V8 syscall surface can only be validated on bare-metal Linux
//! (kernel ≥ 5.13 with unprivileged user namespaces). Those behaviours cannot be
//! exercised in the build sandbox (kernel 5.10, no unprivileged userns), so their
//! tests are marked `#[ignore]` and the seccomp allowlist for V8 is built from
//! knowledge and MUST be validated/iterated on bare metal (run under seccomp,
//! observe `SIGSYS`, add the offending syscall, repeat).
//!
//! **Frozen public API** — the signatures of [`JailHandle`], [`JailError`],
//! [`spawn_jail`], and [`run_jail_child`] are fixed by the workspace contract.
#![allow(dead_code)]

pub mod frame;

pub(crate) mod payload;

/// Slice 4 runtime payload: the jailed child hosts `draco-runtime`'s V8 capture.
/// Replaces the Slice 2 echo ([`payload`]) at the real child entry points.
pub(crate) mod runtime_payload;

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
/// lockdown, then runs the Tier 2 capture payload: read a `Hydrate`, drive the V8
/// isolate via `draco-runtime`, stream `Intercept` frames + a terminal `Result`
/// ([`runtime_payload`]). Never returns: it exits the process on a clean
/// shutdown / completed capture or on any fatal setup error.
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

/// Environment variable the supervisor sets on the re-exec'd child to request the
/// **un-jailed** dev path (`--no-jail`): when present (any value), the Linux child
/// entry SKIPS the namespace/rlimit/Landlock/seccomp lockdown and runs the capture
/// payload directly. This exists so the `draco __jail` hook stays a single call
/// (`run_jail_child`) — the child decides whether to arm based on this marker,
/// which only the dev `no_jail` spawn ever sets.
///
/// SECURITY: production never sets this. It weakens the sandbox to nothing, so it
/// is strictly a local-debugging affordance (the spawn logs a loud warning).
pub const JAIL_NO_SANDBOX_ENV: &str = "DRACO_JAIL_NO_SANDBOX";
