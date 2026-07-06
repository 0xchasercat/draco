//! Slice 4 child payload: host the Tier 2 V8 capture runtime.
//!
//! This replaces the Slice 2 trivial echo ([`crate::payload`]) as the *real*
//! payload the jailed child runs once its sandbox is armed. The flow mirrors the
//! frozen IPC contract (canonical §6) and the jail-child-per-hydrate model:
//!
//! 1. Announce readiness with [`JailToSupervisor::Ready`] (once, at boot).
//! 2. Read a [`SupervisorToJail::Hydrate`] frame — its body carries the raw
//!    page HTML (preceded by zero or more `Resource` frames).
//! 3. Map the Hydrate fields onto a [`draco_runtime::CaptureConfig`] and call
//!    [`draco_runtime::run_capture_with_resources`], which boots a **fresh** V8
//!    isolate, evaluates the page's scripts, and returns every fetch/XHR the
//!    SPA attempted.
//! 4. Stream one [`JailToSupervisor::Intercept`] per [`CapturedRequest`] (the
//!    request body, if any, rides that frame's body), then a terminal
//!    [`JailToSupervisor::Result`].
//! 5. **Loop back to step 2** for the next job, until a `Shutdown` frame (or a
//!    clean channel EOF) closes the worker.
//!
//! ## Warm worker: many captures per process, fresh isolate each
//!
//! The child is a **reusable warm worker**: it pays the process spawn + sandbox
//! arming cost once, then services many `Hydrate` jobs over its lifetime. Each
//! job gets a pristine isolate — [`draco_runtime::run_capture_with_resources`]
//! builds a fresh current-thread tokio runtime and a fresh snapshot-restored
//! `JsRuntime` per call (V8 flags are latched once behind a `Once`, so repeated
//! calls are safe and verified not to bleed state between isolates). This is the
//! child side of the daemon's warm isolate pool: the supervisor keeps a set of
//! these workers idle and dispatches a job to one, amortizing the ~fork+exec +
//! seccomp/namespace arming + first snapshot restore across every scrape rather
//! than paying it per scrape.
//!
//! The accumulated `Resource` map is **cleared between jobs**, so one page's
//! prefetched subresources can never resolve for the next. A `Shutdown` (or EOF)
//! before any `Hydrate` is an orderly no-op close; the supervisor also sends
//! `Shutdown` to recycle a worker after a bounded number of jobs.
//!
//! ## Threading / async
//!
//! The child entry (`draco __jail`) is plain **sync** — we do NOT wrap this in a
//! tokio runtime. `run_capture` owns its runtime; nesting a second one would
//! panic. Everything here is blocking frame I/O over the inherited fd-3 socket.

use std::collections::HashMap;
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

    // Service jobs until Shutdown / EOF. `Resource` frames arrive before each
    // `Hydrate`, carrying the supervisor-prefetched script subresources; we
    // accumulate them into the `{url -> source}` map the isolate's module loader
    // serves, and clear it after each job so no page's subresources leak into
    // the next.
    let mut resources: HashMap<String, Vec<u8>> = HashMap::new();
    loop {
        let msg = match frame::read_supervisor_frame(&mut stream) {
            Ok(f) => f,
            // Supervisor hung up without a Shutdown frame: orderly close.
            Err(FrameError::Eof) => return Ok(()),
            Err(e) => return Err(e.into()),
        };

        match msg.header {
            SupervisorToJail::Resource { url } => {
                resources.insert(url, msg.body);
            }
            SupervisorToJail::Hydrate {
                url,
                capture_window_ms,
                quiesce_ms,
                max_intercepts,
                stub_response_json,
            } => {
                // The frame body carries the raw page HTML (lossy-decoded: page
                // bytes may not be valid UTF-8, and the capture engine wants a &str).
                let html = String::from_utf8_lossy(&msg.body).into_owned();
                let cfg = CaptureConfig {
                    capture_window_ms,
                    quiesce_ms,
                    max_intercepts,
                    stub_response_json,
                };

                // capture() builds its OWN current-thread tokio runtime and a
                // fresh snapshot-restored isolate per call (V8 flags latched once
                // behind a `Once`). We are plain sync here — do NOT wrap it.
                let report = capture(&url, &html, &cfg, std::mem::take(&mut resources));
                emit_report(&mut stream, report)?;
                // Job done. Loop for the next one; the resource map was consumed
                // by `capture` (via `take`), so the next job starts with a clean
                // slate.
            }
            SupervisorToJail::Shutdown => return Ok(()),
        }
    }
}

/// Indirection over [`draco_runtime::run_capture_with_resources`] so the wiring
/// tests can inject a canned [`CaptureReport`] instead of booting a real V8
/// isolate (which is exercised by draco-runtime's own integration tests and the
/// `#[ignore]`d full-jail smoke test). Production always calls the real engine.
#[cfg(not(test))]
fn capture(
    url: &str,
    html: &str,
    cfg: &CaptureConfig,
    resources: HashMap<String, Vec<u8>>,
) -> CaptureReport {
    draco_runtime::run_capture_with_resources(url, html, cfg, resources)
}

/// Test seam: return the [`CaptureReport`] queued by the current test.
#[cfg(test)]
fn capture(
    _url: &str,
    _html: &str,
    _cfg: &CaptureConfig,
    _resources: HashMap<String, Vec<u8>>,
) -> CaptureReport {
    tests::CANNED
        .lock()
        .unwrap()
        .pop_front()
        .expect("test must queue a canned CaptureReport before each Hydrate")
}

/// Serialize a [`CaptureReport`] onto the wire: one `Intercept` per captured
/// request (its optional body rides the frame body), then a terminal `Result`.
///
/// `seq` is the 0-based capture index; header order within each request is
/// preserved verbatim by `run_capture`, and we do not reorder it.
///
/// The terminal `Result` frame's **body** carries the hydrated DOM serialized by
/// the runtime (`CaptureReport::rendered_html`), when present — this is the raw
/// material for the supervisor's render-then-Markdown escalation. Reusing the
/// frame body keeps the frozen `JailToSupervisor::Result` header unchanged while
/// still returning the rendered markup; an absent DOM is an empty body.
fn emit_report(stream: &mut UnixStream, report: CaptureReport) -> Result<(), PayloadError> {
    let intercept_count = report.requests.len() as u32;
    let rendered_html = report.rendered_html;

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

    let result_body = rendered_html.unwrap_or_default();
    frame::write_jail_frame(
        stream,
        &JailToSupervisor::Result {
            outcome: report.outcome,
            intercept_count,
        },
        result_body.as_bytes(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{read_jail_frame, write_supervisor_frame};
    use draco_types::{InterceptVia, RuntimeOutcome};
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::thread;

    // The queue of canned reports the `#[cfg(test)]` `capture()` above pops, one
    // per serviced `Hydrate` — a queue (not a single slot) so a multi-job test
    // can pre-load one report per job the looping worker will service.
    pub(super) static CANNED: Mutex<VecDeque<CaptureReport>> = Mutex::new(VecDeque::new());
    // Serializes the tests that queue canned reports, so parallel test threads
    // cannot cross-consume each other's reports.
    static CAPTURE_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn set_canned(report: CaptureReport) {
        let mut q = CANNED.lock().unwrap();
        q.clear();
        q.push_back(report);
    }

    fn queue_canned(reports: Vec<CaptureReport>) {
        let mut q = CANNED.lock().unwrap();
        q.clear();
        q.extend(reports);
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
            rendered_html: None,
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
        // No rendered DOM in this report → empty Result body.
        assert!(result.body.is_empty(), "expected empty Result body");

        drop(sup);
        child_handle.join().unwrap().unwrap();
    }

    /// The warm-worker contract: one `Ready`, then the worker services **many**
    /// `Hydrate` jobs over its lifetime (each with its own terminal `Result`),
    /// and the per-job `Resource` map does not leak between jobs. Closing the
    /// channel ends the worker cleanly.
    #[test]
    fn worker_services_multiple_jobs_until_eof() {
        let _guard = CAPTURE_TEST_LOCK.lock().unwrap();
        // One canned report per job the worker will service.
        queue_canned(vec![
            CaptureReport {
                outcome: RuntimeOutcome::Quiesced,
                requests: vec![cap("https://x.com/api/1", InterceptVia::Fetch, None)],
                rendered_html: None,
            },
            CaptureReport {
                outcome: RuntimeOutcome::NoIntercepts,
                requests: vec![],
                rendered_html: Some("<html><body>job 2</body></html>".into()),
            },
            CaptureReport {
                outcome: RuntimeOutcome::Quiesced,
                requests: vec![cap("https://x.com/api/3", InterceptVia::Xhr, Some("{}"))],
                rendered_html: None,
            },
        ]);

        let (sup, child) = UnixStream::pair().unwrap();
        let child_handle = thread::spawn(move || run_capture_loop(child, None));
        let mut sup = sup;

        // One Ready at boot — NOT once per job.
        let ready = read_jail_frame(&mut sup).unwrap();
        assert!(matches!(ready.header, JailToSupervisor::Ready { .. }));

        // Three back-to-back jobs on the SAME worker. Each job: a Resource frame
        // (proving the map is accepted + cleared per job), a Hydrate, then the
        // job's Intercepts + terminal Result.
        let expected_counts = [1u32, 0, 1];
        for (job, &count) in expected_counts.iter().enumerate() {
            write_supervisor_frame(
                &mut sup,
                &SupervisorToJail::Resource {
                    url: format!("https://x.com/job{job}.js"),
                },
                b"// prefetched source",
            )
            .unwrap();
            write_supervisor_frame(
                &mut sup,
                &SupervisorToJail::Hydrate {
                    url: format!("https://x.com/page{job}"),
                    capture_window_ms: 500,
                    quiesce_ms: 50,
                    max_intercepts: 8,
                    stub_response_json: "{}".into(),
                },
                b"<html></html>",
            )
            .unwrap();

            // Drain this job's Intercepts, then its terminal Result.
            let mut seen = 0u32;
            loop {
                let f = read_jail_frame(&mut sup).unwrap();
                match f.header {
                    JailToSupervisor::Intercept { .. } => seen += 1,
                    JailToSupervisor::Result {
                        intercept_count, ..
                    } => {
                        assert_eq!(intercept_count, count, "job {job} intercept count");
                        assert_eq!(seen, count, "job {job} streamed Intercepts");
                        break;
                    }
                    other => panic!("job {job}: unexpected {other:?}"),
                }
            }
        }

        // Closing the channel ends the worker cleanly (no Shutdown needed).
        drop(sup);
        child_handle.join().unwrap().unwrap();
    }

    #[test]
    fn rendered_dom_rides_the_result_frame_body() {
        let _guard = CAPTURE_TEST_LOCK.lock().unwrap();
        let dom = "<html><head></head><body><h1>Hydrated</h1></body></html>";
        set_canned(CaptureReport {
            outcome: RuntimeOutcome::Quiesced,
            requests: vec![],
            rendered_html: Some(dom.to_string()),
        });

        let (sup, child) = UnixStream::pair().unwrap();
        let child_handle = thread::spawn(move || run_capture_loop(child, None));
        let mut sup = sup;

        let _ready = read_jail_frame(&mut sup).unwrap();
        write_supervisor_frame(
            &mut sup,
            &SupervisorToJail::Hydrate {
                url: "https://spa.example/".into(),
                capture_window_ms: 500,
                quiesce_ms: 50,
                max_intercepts: 8,
                stub_response_json: "{}".into(),
            },
            b"<html><body><div id=app></div></body></html>",
        )
        .unwrap();

        // The terminal Result carries the serialized hydrated DOM in its body.
        let result = read_jail_frame(&mut sup).unwrap();
        assert_eq!(
            result.header,
            JailToSupervisor::Result {
                outcome: RuntimeOutcome::Quiesced,
                intercept_count: 0,
            }
        );
        assert_eq!(
            String::from_utf8_lossy(&result.body),
            dom,
            "serialized DOM should ride the Result frame body"
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
            rendered_html: None,
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
            rendered_html: None,
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
