//! Slice 4 child payload: host the Tier 2 V8 capture runtime.
//!
//! This replaces the Slice 2 trivial echo ([`crate::payload`]) as the *real*
//! payload the jailed child runs once its sandbox is armed. The flow mirrors the
//! frozen IPC contract (canonical §6) and the jail-child-per-hydrate model:
//!
//! 1. Announce readiness with [`JailToSupervisor::Ready`].
//! 2. Read **one** [`SupervisorToJail::Hydrate`] frame — its body carries the raw
//!    page HTML.
//! 3. Map the Hydrate fields onto a [`draco_runtime::CaptureConfig`] and call
//!    [`draco_runtime::run_capture`], which boots a V8 isolate, evaluates the
//!    page's inline scripts, and returns every fetch/XHR the SPA attempted.
//! 4. Stream one [`JailToSupervisor::Intercept`] per [`CapturedRequest`] (the
//!    request body, if any, rides that frame's body), then a terminal
//!    [`JailToSupervisor::Result`], then return so the process exits.
//!
//! ## Why one capture per process
//!
//! [`draco_runtime::run_capture`] builds its **own** current-thread tokio runtime
//! internally and sets V8 flags process-globally exactly once. Calling it twice
//! in one process is unsupported (V8 flags are latched; a fresh isolate per
//! process is the design). The supervisor therefore spawns a new jailed child for
//! every `Hydrate`, and this payload handles a single one and exits. A `Shutdown`
//! (or a clean channel EOF) before any `Hydrate` is an orderly no-op close.
//!
//! ## Threading / async
//!
//! The child entry (`draco __jail`) is plain **sync** — we do NOT wrap this in a
//! tokio runtime. `run_capture` owns its runtime; nesting a second one would
//! panic. Everything here is blocking frame I/O over the inherited fd-3 socket.

use std::os::fd::FromRawFd;
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixStream;

use draco_runtime::{CaptureConfig, CaptureReport, CapturedRequest};
use draco_types::{JailToSupervisor, LogLevel, SupervisorToJail};

use crate::frame::{self, FrameError};
use crate::level::LEVEL_LOG_PREFIX;
use crate::JAIL_IPC_FD;

/// Errors from running the runtime payload loop.
#[derive(Debug)]
pub enum PayloadError {
    /// The IPC frame layer failed.
    Frame(FrameError),
}

impl std::fmt::Display for PayloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PayloadError::Frame(e) => write!(f, "runtime payload IPC error: {e}"),
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

/// Run the runtime child payload over the inherited fd-3 IPC socket.
///
/// `sandbox_level` is the achieved posture (e.g. `"hardened: seccomp+netns+
/// landlock"` or `"isolate: v8 no host bindings (macos)"`); when `Some`, it is
/// reported to the supervisor as a `Log` frame right after `Ready` so the ladder
/// can record a `runtime.sandbox` trace step. `None` (used by tests) skips it.
///
/// Returns `Ok(())` on an orderly completion (a serviced `Hydrate`, a `Shutdown`,
/// or a clean channel EOF) and an error on any framing/protocol failure.
pub fn run_child_over_fd3(sandbox_level: Option<&str>) -> Result<(), PayloadError> {
    // SAFETY: `draco __jail` is only ever reached via the supervisor's re-exec,
    // which dup2()'s the child socketpair end onto fd 3 (JAIL_IPC_FD) and sets
    // CLOEXEC on every other inherited fd. Nothing else in this process owns
    // fd 3, so adopting it here is sound.
    let stream = unsafe { stream_from_fd(JAIL_IPC_FD) };
    run_capture_loop(stream, sandbox_level)
}

/// The transport-agnostic payload, split out so tests can drive it over an
/// in-process socketpair without the fd-3 / re-exec machinery.
///
/// Handles exactly one `Hydrate` (see the module docs on why capture is
/// once-per-process), emitting `Intercept` frames + a terminal `Result`.
/// `sandbox_level`, when `Some`, is reported as a `Log` frame after `Ready`.
pub fn run_capture_loop(
    mut stream: UnixStream,
    sandbox_level: Option<&str>,
) -> Result<(), PayloadError> {
    // Announce readiness. deno_core executes the polyfill at isolate boot rather
    // than restoring a heap snapshot, so there is no snapshot-restore cost to
    // report here; 0 ms is honest.
    frame::write_jail_frame(
        &mut stream,
        &JailToSupervisor::Ready {
            snapshot_restore_ms: 0,
        },
        &[],
    )?;

    // Report the achieved sandbox level as an informational Log frame (a frozen
    // frame type), prefixed so the supervisor can pick it out and surface it in
    // the `runtime.sandbox` trace step.
    if let Some(level) = sandbox_level {
        frame::write_jail_frame(
            &mut stream,
            &JailToSupervisor::Log {
                level: LogLevel::Info,
                msg: format!("{LEVEL_LOG_PREFIX}{level}"),
            },
            &[],
        )?;
    }

    // Read a single control frame. A capture is once-per-process, so we do not
    // loop over multiple Hydrates.
    let msg = match frame::read_supervisor_frame(&mut stream) {
        Ok(f) => f,
        // Supervisor hung up without a Shutdown frame: orderly close.
        Err(FrameError::Eof) => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    match msg.header {
        SupervisorToJail::Hydrate {
            url,
            capture_window_ms,
            quiesce_ms,
            max_intercepts,
            stub_response_json,
        } => {
            // The frame body carries the raw page HTML (lossy-decoded: page bytes
            // may not be valid UTF-8, and the capture engine wants a &str).
            let html = String::from_utf8_lossy(&msg.body).into_owned();
            let cfg = CaptureConfig {
                capture_window_ms,
                quiesce_ms,
                max_intercepts,
                stub_response_json,
            };

            // capture() owns its OWN current-thread tokio runtime and inits V8
            // flags process-globally. We are plain sync here — do NOT wrap it.
            let report = capture(&url, &html, &cfg);
            emit_report(&mut stream, report)?;
            // One capture per process: return so the child exits.
            Ok(())
        }
        SupervisorToJail::Shutdown => Ok(()),
    }
}

/// Indirection over [`draco_runtime::run_capture`] so the wiring tests can inject
/// a canned [`CaptureReport`] instead of booting a real V8 isolate (which is
/// exercised by draco-runtime's own integration tests and the `#[ignore]`d
/// full-jail smoke test). Production always calls the real capture engine.
#[cfg(not(test))]
fn capture(url: &str, html: &str, cfg: &CaptureConfig) -> CaptureReport {
    draco_runtime::run_capture(url, html, cfg)
}

/// Test seam: return the [`CaptureReport`] queued by the current test.
#[cfg(test)]
fn capture(_url: &str, _html: &str, _cfg: &CaptureConfig) -> CaptureReport {
    tests::CANNED
        .lock()
        .unwrap()
        .take()
        .expect("test must queue a canned CaptureReport before Hydrate")
}

/// Serialize a [`CaptureReport`] onto the wire: one `Intercept` per captured
/// request (its optional body rides the frame body), then a terminal `Result`.
///
/// `seq` is the 0-based capture index; header order within each request is
/// preserved verbatim by `run_capture`, and we do not reorder it.
fn emit_report(stream: &mut UnixStream, report: CaptureReport) -> Result<(), PayloadError> {
    let intercept_count = report.requests.len() as u32;

    for (seq, req) in report.requests.into_iter().enumerate() {
        let CapturedRequest {
            method,
            url,
            headers,
            body,
            via,
        } = req;
        let has_body = body.is_some();
        let frame_body = body.unwrap_or_default();
        frame::write_jail_frame(
            stream,
            &JailToSupervisor::Intercept {
                seq: seq as u32,
                method,
                url,
                headers,
                has_body,
                via,
            },
            &frame_body,
        )?;
    }

    frame::write_jail_frame(
        stream,
        &JailToSupervisor::Result {
            outcome: report.outcome,
            intercept_count,
        },
        &[],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{read_jail_frame, write_supervisor_frame};
    use draco_types::{InterceptVia, RuntimeOutcome};
    use std::sync::Mutex;
    use std::thread;

    // The canned report the `#[cfg(test)]` `capture()` above hands back.
    pub(super) static CANNED: Mutex<Option<CaptureReport>> = Mutex::new(None);
    // Serializes the tests that queue a canned report, so parallel test threads
    // cannot cross-consume each other's `CANNED` slot.
    static CAPTURE_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn set_canned(report: CaptureReport) {
        *CANNED.lock().unwrap() = Some(report);
    }

    fn cap(url: &str, via: InterceptVia, body: Option<&str>) -> CapturedRequest {
        CapturedRequest {
            method: "GET".to_string(),
            url: url.to_string(),
            headers: vec![("accept".to_string(), "application/json".to_string())],
            body: body.map(|b| b.as_bytes().to_vec()),
            via,
        }
    }

    #[test]
    fn ready_then_hydrate_emits_intercepts_then_result() {
        let _guard = CAPTURE_TEST_LOCK.lock().unwrap();
        set_canned(CaptureReport {
            outcome: RuntimeOutcome::Quiesced,
            requests: vec![
                cap("https://x.com/api/a", InterceptVia::Fetch, None),
                cap("https://x.com/api/b", InterceptVia::Xhr, Some("{\"q\":1}")),
            ],
        });

        let (sup, child) = UnixStream::pair().unwrap();
        let child_handle = thread::spawn(move || run_capture_loop(child, None));
        let mut sup = sup;

        // 1. Ready.
        let ready = read_jail_frame(&mut sup).unwrap();
        assert_eq!(
            ready.header,
            JailToSupervisor::Ready {
                snapshot_restore_ms: 0
            }
        );

        // 2. Drive a Hydrate with HTML in the body.
        write_supervisor_frame(
            &mut sup,
            &SupervisorToJail::Hydrate {
                url: "https://x.com".into(),
                capture_window_ms: 500,
                quiesce_ms: 50,
                max_intercepts: 8,
                stub_response_json: "{}".into(),
            },
            b"<html><script>fetch('/api/a')</script></html>",
        )
        .unwrap();

        // 3. Two Intercept frames, in capture order with ascending seq.
        let i0 = read_jail_frame(&mut sup).unwrap();
        match i0.header {
            JailToSupervisor::Intercept {
                seq,
                url,
                has_body,
                via,
                ..
            } => {
                assert_eq!(seq, 0);
                assert_eq!(url, "https://x.com/api/a");
                assert!(!has_body);
                assert_eq!(via, InterceptVia::Fetch);
                assert!(i0.body.is_empty());
            }
            other => panic!("expected Intercept, got {other:?}"),
        }
        let i1 = read_jail_frame(&mut sup).unwrap();
        match i1.header {
            JailToSupervisor::Intercept {
                seq,
                url,
                has_body,
                via,
                ..
            } => {
                assert_eq!(seq, 1);
                assert_eq!(url, "https://x.com/api/b");
                assert!(has_body, "request body should be flagged");
                assert_eq!(via, InterceptVia::Xhr);
                assert_eq!(i1.body, b"{\"q\":1}", "request body rides the frame body");
            }
            other => panic!("expected Intercept, got {other:?}"),
        }

        // 4. Terminal Result with the count + outcome.
        let result = read_jail_frame(&mut sup).unwrap();
        assert_eq!(
            result.header,
            JailToSupervisor::Result {
                outcome: RuntimeOutcome::Quiesced,
                intercept_count: 2,
            }
        );

        drop(sup);
        child_handle.join().unwrap().unwrap();
    }

    #[test]
    fn sandbox_level_is_reported_as_a_log_after_ready() {
        let _guard = CAPTURE_TEST_LOCK.lock().unwrap();
        set_canned(CaptureReport {
            outcome: RuntimeOutcome::NoIntercepts,
            requests: vec![],
        });

        let (sup, child) = UnixStream::pair().unwrap();
        // Pass a level so the payload emits the prefixed Log frame after Ready.
        let child_handle = thread::spawn(move || {
            run_capture_loop(child, Some("hardened: seccomp+netns+landlock"))
        });
        let mut sup = sup;

        // 1. Ready first.
        let ready = read_jail_frame(&mut sup).unwrap();
        assert!(matches!(ready.header, JailToSupervisor::Ready { .. }));

        // 2. Then the sandbox-level Log, prefixed for supervisor recognition.
        let log = read_jail_frame(&mut sup).unwrap();
        match log.header {
            JailToSupervisor::Log { level, msg } => {
                assert_eq!(level, LogLevel::Info);
                assert_eq!(msg, "sandbox:hardened: seccomp+netns+landlock");
                assert!(msg.starts_with(crate::level::LEVEL_LOG_PREFIX));
            }
            other => panic!("expected a sandbox-level Log, got {other:?}"),
        }

        // 3. Drive a Hydrate; the terminal Result still follows.
        write_supervisor_frame(
            &mut sup,
            &SupervisorToJail::Hydrate {
                url: "https://x.com".into(),
                capture_window_ms: 500,
                quiesce_ms: 50,
                max_intercepts: 8,
                stub_response_json: "{}".into(),
            },
            b"<html></html>",
        )
        .unwrap();
        let result = read_jail_frame(&mut sup).unwrap();
        assert!(matches!(
            result.header,
            JailToSupervisor::Result {
                outcome: RuntimeOutcome::NoIntercepts,
                ..
            }
        ));
        drop(sup);
        child_handle.join().unwrap().unwrap();
    }

    #[test]
    fn no_intercepts_still_emits_a_result() {
        let _guard = CAPTURE_TEST_LOCK.lock().unwrap();
        set_canned(CaptureReport {
            outcome: RuntimeOutcome::NoIntercepts,
            requests: vec![],
        });

        let (sup, child) = UnixStream::pair().unwrap();
        let child_handle = thread::spawn(move || run_capture_loop(child, None));
        let mut sup = sup;

        let _ready = read_jail_frame(&mut sup).unwrap();
        write_supervisor_frame(
            &mut sup,
            &SupervisorToJail::Hydrate {
                url: "https://x.com".into(),
                capture_window_ms: 500,
                quiesce_ms: 50,
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
        drop(sup);
        child_handle.join().unwrap().unwrap();
    }

    #[test]
    fn shutdown_before_hydrate_is_orderly() {
        let (sup, child) = UnixStream::pair().unwrap();
        let child_handle = thread::spawn(move || run_capture_loop(child, None));
        let mut sup = sup;

        let _ready = read_jail_frame(&mut sup).unwrap();
        write_supervisor_frame(&mut sup, &SupervisorToJail::Shutdown, &[]).unwrap();
        drop(sup);

        child_handle.join().unwrap().unwrap();
    }

    #[test]
    fn channel_close_before_hydrate_is_orderly() {
        let (sup, child) = UnixStream::pair().unwrap();
        let child_handle = thread::spawn(move || run_capture_loop(child, None));
        let mut sup = sup;
        let _ready = read_jail_frame(&mut sup).unwrap();
        drop(sup); // hang up without Shutdown
        child_handle.join().unwrap().unwrap();
    }
}
