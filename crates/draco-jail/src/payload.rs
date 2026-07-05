//! Slice 2 child payload.
//!
//! This is the *trivial* payload the jailed child runs once its sandbox is armed.
//! It is deliberately **not** a V8 isolate — deno_core/V8 is Slice 3. The purpose
//! here is to prove the plumbing end-to-end: the child inherits the IPC socket on
//! fd 3, announces itself with [`JailToSupervisor::Ready`], then services frames
//! from the supervisor by echoing a structured reply until it is told to shut
//! down (or the channel closes).
//!
//! Everything here is portable (no Linux syscalls), so both the Linux and the
//! degraded (`macOS`/other) child entry points reuse it.

use std::os::fd::FromRawFd;
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixStream;

use draco_types::{JailToSupervisor, RuntimeOutcome, SupervisorToJail};

use crate::frame::{self, FrameError};
use crate::JAIL_IPC_FD;

/// Errors from running the payload loop.
#[derive(Debug)]
pub enum PayloadError {
    /// The IPC frame layer failed.
    Frame(FrameError),
}

impl std::fmt::Display for PayloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PayloadError::Frame(e) => write!(f, "payload IPC error: {e}"),
        }
    }
}

impl std::error::Error for PayloadError {}

impl From<FrameError> for PayloadError {
    fn from(e: FrameError) -> Self {
        PayloadError::Frame(e)
    }
}

/// Adopt the inherited fd-3 socket as an owned [`UnixStream`].
///
/// # Safety
///
/// The caller must guarantee that `fd` is a valid, open socket owned by this
/// process (the supervisor dup'd the child socketpair end onto it before exec)
/// and that no other `UnixStream` already owns it. Violating either invariant is
/// undefined behaviour (double-close / use-after-close).
unsafe fn stream_from_fd(fd: RawFd) -> UnixStream {
    // SAFETY: upheld by the caller per the doc comment above.
    unsafe { UnixStream::from_raw_fd(fd) }
}

/// Run the child payload loop over the inherited fd-3 IPC socket.
///
/// Returns `Ok(())` on an orderly shutdown (a `Shutdown` frame or a clean
/// channel EOF) and an error on any framing/protocol failure.
pub fn run_child_over_fd3() -> Result<(), PayloadError> {
    // SAFETY: `draco __jail` is only ever reached via the supervisor's re-exec,
    // which dup2()'s the child socketpair end onto fd 3 (JAIL_IPC_FD) and sets
    // CLOEXEC on every other inherited fd. Nothing else in this process owns
    // fd 3, so adopting it here is sound.
    let stream = unsafe { stream_from_fd(JAIL_IPC_FD) };
    run_loop(stream)
}

/// The transport-agnostic loop, split out so tests can drive it over an
/// in-process socketpair.
pub fn run_loop(mut stream: UnixStream) -> Result<(), PayloadError> {
    // Announce readiness. In Slice 3 this happens after the V8 snapshot restore;
    // here the restore is a no-op, so report 0 ms.
    frame::write_jail_frame(
        &mut stream,
        &JailToSupervisor::Ready {
            snapshot_restore_ms: 0,
        },
        &[],
    )?;

    loop {
        let msg = match frame::read_supervisor_frame(&mut stream) {
            Ok(f) => f,
            // Supervisor hung up without a Shutdown frame: treat as orderly.
            Err(FrameError::Eof) => return Ok(()),
            Err(e) => return Err(e.into()),
        };

        match msg.header {
            SupervisorToJail::Hydrate { .. } => {
                // Slice 2 stand-in for a real hydration: acknowledge with a
                // terminal Result reporting that nothing was intercepted. The
                // request body (raw HTML) is available in `msg.body` for Slice 3.
                frame::write_jail_frame(
                    &mut stream,
                    &JailToSupervisor::Result {
                        outcome: RuntimeOutcome::NoIntercepts,
                        intercept_count: 0,
                    },
                    &[],
                )?;
            }
            SupervisorToJail::Shutdown => {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{read_jail_frame, write_supervisor_frame};
    use std::thread;

    #[test]
    fn payload_announces_ready_then_answers_hydrate_and_shuts_down() {
        let (sup, child) = UnixStream::pair().unwrap();

        // Run the child loop on its own thread against one end of the pair.
        let child_handle = thread::spawn(move || run_loop(child));

        let mut sup = sup;

        // 1. Child must announce Ready first.
        let ready = read_jail_frame(&mut sup).unwrap();
        assert_eq!(
            ready.header,
            JailToSupervisor::Ready {
                snapshot_restore_ms: 0
            }
        );

        // 2. Drive a Hydrate; expect a Result reply.
        write_supervisor_frame(
            &mut sup,
            &SupervisorToJail::Hydrate {
                url: "https://example.com".into(),
                capture_window_ms: 1000,
                quiesce_ms: 100,
                max_intercepts: 8,
                stub_response_json: "{}".into(),
            },
            b"<html></html>",
        )
        .unwrap();
        let result = read_jail_frame(&mut sup).unwrap();
        assert_eq!(
            result.header,
            JailToSupervisor::Result {
                outcome: RuntimeOutcome::NoIntercepts,
                intercept_count: 0,
            }
        );

        // 3. Shutdown ends the loop cleanly.
        write_supervisor_frame(&mut sup, &SupervisorToJail::Shutdown, &[]).unwrap();
        drop(sup);

        child_handle.join().unwrap().unwrap();
    }

    #[test]
    fn payload_treats_channel_close_as_orderly() {
        let (sup, child) = UnixStream::pair().unwrap();
        let child_handle = thread::spawn(move || run_loop(child));

        let mut sup = sup;
        // Consume Ready, then hang up without a Shutdown frame.
        let _ = read_jail_frame(&mut sup).unwrap();
        drop(sup);

        child_handle.join().unwrap().unwrap();
    }
}
