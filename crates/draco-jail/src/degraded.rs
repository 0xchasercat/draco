//! Non-Linux path: **isolate** mode (first-class, not a fallback).
//!
//! The seccomp / Landlock / namespace layers are Linux-only. On macOS (and any
//! other non-Linux host) Tier 2 still runs the V8 isolate with **no
//! host-capability ops** — page JS can touch neither the network, the filesystem,
//! nor processes, because the isolate exposes only `op_raze_fetch`/`op_sleep`/
//! `op_resolve_url`. That is Puppeteer/Playwright-class containment and a normal,
//! supported posture (the `isolate` level), so this path runs **silently and
//! successfully** — no warning, no error.
//!
//! The mechanics mirror the Linux supervisor spawn minus the OS-sandbox arming:
//! a socketpair, a fork, the child dup's its end onto fd 3 and re-execs
//! `<self> __jail`, which routes back here into [`run_jail_child`].

use std::ffi::CString;
use std::os::fd::{IntoRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

use draco_types::JailKind;

use crate::level::SandboxLevel;
use crate::{runtime_payload, JailError, JailHandle, JAIL_IPC_FD};

/// Supervisor-side spawn (isolate mode). Socketpair → fork → (child) place the
/// socket on fd 3 and re-exec `<self> __jail`; (parent) keep the supervisor end.
///
/// Returns a live [`JailHandle`] just like the Linux jail: the caller drives the
/// same IPC exchange regardless of platform. No OS sandbox is armed here (there is
/// none on this platform); the isolate itself is the containment.
pub fn spawn_jail() -> Result<JailHandle, JailError> {
    let (sup, child) = UnixStream::pair()
        .map_err(|e| JailError::new(JailKind::Spawn, format!("socketpair: {e}")))?;

    let exe = std::env::current_exe()
        .map_err(|e| JailError::new(JailKind::Spawn, format!("current_exe: {e}")))?;
    let exe_c = CString::new(exe.as_os_str().as_encoded_bytes())
        .map_err(|e| JailError::new(JailKind::Spawn, format!("exe path has NUL: {e}")))?;
    let jail_arg = CString::new("__jail").expect("static literal \"__jail\" contains no NUL byte");

    let child_fd: OwnedFd = child.into();
    let child_raw = child_fd.into_raw_fd();

    // SAFETY: between fork and exec we call only async-signal-safe libc functions
    // (dup2/close/fcntl/execv/_exit) and touch no Rust runtime state.
    match unsafe { libc::fork() } {
        -1 => {
            // SAFETY: child_raw is a valid fd we still own here.
            unsafe { libc::close(child_raw) };
            drop(sup);
            Err(JailError::new(JailKind::Spawn, "fork failed"))
        }
        0 => {
            // SAFETY: async-signal-safe calls only; abort hard on any failure so we
            // never run supervisor code in the child.
            unsafe {
                if child_raw != JAIL_IPC_FD {
                    if libc::dup2(child_raw, JAIL_IPC_FD) < 0 {
                        libc::_exit(126);
                    }
                    libc::close(child_raw);
                } else if libc::fcntl(child_raw, libc::F_SETFD, 0) < 0 {
                    // Already on fd 3: clear CLOEXEC so it survives the exec.
                    libc::_exit(126);
                }
                let argv = [exe_c.as_ptr(), jail_arg.as_ptr(), std::ptr::null()];
                libc::execv(exe_c.as_ptr(), argv.as_ptr());
                // exec failed; abort so this arm never returns supervisor-side.
                libc::_exit(127)
            }
        }
        pid => {
            // Parent: close the child end, keep the supervisor end.
            // SAFETY: child_raw belongs to the child now; close our copy.
            unsafe { libc::close(child_raw) };
            Ok(JailHandle { pid, ipc: sup })
        }
    }
}

/// Non-Linux child entry: run the Tier 2 capture payload over fd 3 in isolate
/// mode. Plain sync — `draco-runtime`'s `run_capture` owns its own current-thread
/// tokio runtime, so we must not be inside one here. Reports the `isolate` level
/// to the supervisor (as a `Log` after `Ready`); no warning.
pub fn run_jail_child() -> ! {
    let level = SandboxLevel::isolate_macos();
    match runtime_payload::run_child_over_fd3(Some(&level.describe())) {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("draco-jail: child exiting on error: {e}");
            std::process::exit(1);
        }
    }
}
