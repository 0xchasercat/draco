//! Tier 2 wiring (Slice 4): jail-hosted V8 capture → ranked replay.
//!
//! This is the supervisor half of the Tier 2 flow (the jailed-child half lives in
//! `draco-jail`). When the ladder reaches Tier 2 (Tiers 0/1 missed, it is not a
//! challenge, and `tier_max >= 2`), it drives a [`Tier2Capture`] seam to obtain a
//! [`CaptureResult`], then [`rank_and_replay`]s it:
//!
//! 1. **Capture.** The production seam ([`ProdTier2Capture`], `tier2` feature on)
//!    spawns the jail child via [`draco_jail::spawn_jail`] (or, when
//!    `config.no_jail`, an un-jailed dev fork of `draco __jail`), drives the IPC
//!    exchange (read `Ready`, write `Hydrate` with the Tier-0 HTML as the frame
//!    body, collect `Intercept`s until the terminal `Result`), and returns the
//!    captured requests + outcome. With the feature OFF, the seam is
//!    [`DisabledCapture`], which reports "built without tier2".
//! 2. **Rank + replay.** [`best_candidate`](crate::ranking::best_candidate) picks
//!    the most data-endpoint-like intercept; if it clears
//!    [`MIN_VIABLE_SCORE`](crate::ranking::MIN_VIABLE_SCORE) it is replayed
//!    through the [`PageFetcher`] seam. A JSON body finalizes `Success` /
//!    `SourceTier::RuntimeInterception`; otherwise the run is `Unsupported`.
//!
//! ## Why a capture *seam*
//!
//! Splitting capture behind [`Tier2Capture`] keeps the ladder offline-testable:
//! unit tests drive `run_ladder` (and [`rank_and_replay`]) with a **mock** capture
//! that returns a canned `Vec` of intercepts, so no real child is forked. The
//! real-child path is exercised only by an `#[ignore]`d e2e test. The rank/replay
//! logic and the `CaptureResult` shape are V8-free and always compiled; only the
//! jail-spawning production seam is behind `#[cfg(feature = "tier2")]`.
//!
//! ## Blocking IPC inside an async ladder
//!
//! `spawn_jail` forks and the IPC uses blocking `UnixStream` reads/writes, so the
//! spawn+capture phase is synchronous. The ladder runs under a multi-thread tokio
//! runtime, so [`ProdTier2Capture`] pushes that phase onto
//! [`tokio::task::spawn_blocking`] rather than stalling a worker. The V8 isolate
//! (and its own current-thread tokio runtime) lives entirely in the **child**
//! process, so there is no nested-runtime hazard on the supervisor side. Only the
//! *replay* is async — it goes back through the normal `PageFetcher`.

use async_trait::async_trait;
use draco_types::{DracoError, JailKind, RuntimeOutcome};

use crate::fetcher::PageFetcher;
use crate::ranking::{best_candidate, Candidate};
use crate::Config;

// ===========================================================================
// Always-on: capture result shape, the seam, and rank+replay.
// (No dependency on draco-jail / V8; compiled in the lean build too.)
// ===========================================================================

/// The outcome of the capture phase: what the child intercepted, plus the
/// terminal runtime outcome it reported. Produced by a [`Tier2Capture`] seam and
/// consumed by [`rank_and_replay`].
#[derive(Debug, Clone)]
pub(crate) struct CaptureResult {
    /// Ranking views of every intercepted request, in capture order.
    pub candidates: Vec<Candidate>,
    /// The raw request body captured for each intercept (parallel to
    /// `candidates`; `None` when the request had no body). Kept out of
    /// [`Candidate`] so the ranking policy stays body-agnostic, but carried here
    /// so a POST winner (e.g. GraphQL) can be replayed faithfully.
    pub bodies: Vec<Option<Vec<u8>>>,
    /// The child's terminal runtime outcome.
    pub outcome: RuntimeOutcome,
}

/// The Tier 2 capture seam: given the page URL + Tier-0 HTML, produce a
/// [`CaptureResult`] (or a [`DracoError::Jail`]). Behind a trait so the ladder is
/// drivable offline with a mock that fabricates intercepts, no child forked.
#[async_trait]
pub(crate) trait Tier2Capture: Send + Sync {
    async fn capture(
        &self,
        url: &str,
        html: &[u8],
        config: &Config,
    ) -> Result<CaptureResult, DracoError>;
}

/// A structured Tier 2 error, mapped to [`DracoError::Jail`] for the trace/result.
pub(crate) fn jail_error(reason: JailKind, detail: impl Into<String>) -> DracoError {
    DracoError::Jail {
        reason,
        detail: detail.into(),
    }
}

/// Rank a capture result and replay the winner, producing the finalized
/// `(data, detail)` on success or `None` when nothing viable was found.
///
/// Async because replay goes through the [`PageFetcher`] seam. Returns
/// `Ok(Some((json, detail)))` on a JSON-bodied winner, `Ok(None)` when no
/// candidate clears the viability bar, the replay was non-2xx, or the body is not
/// JSON, and `Err(..)` only on a replay transport failure. This is the
/// offline-unit-tested core (mock `PageFetcher` + a hand-built `CaptureResult`).
pub(crate) async fn rank_and_replay<F>(
    capture: &CaptureResult,
    target_url: &str,
    opts: &draco_net::SessionOpts,
    fetcher: &F,
) -> Result<Option<(serde_json::Value, String)>, DracoError>
where
    F: PageFetcher + ?Sized,
{
    let Some((idx, score)) = best_candidate(&capture.candidates, Some(target_url)) else {
        return Ok(None);
    };
    let winner = &capture.candidates[idx];

    // Build the replay spec, attaching the captured request body (base64) so a
    // POST winner replays faithfully. Header order is preserved verbatim.
    let mut spec = winner.to_request_spec();
    if let Some(body) = capture.bodies.get(idx).and_then(|b| b.as_ref()) {
        spec.body_b64 = Some(base64_encode(body));
    }

    let resp = fetcher.replay(&spec, opts).await?;

    if !is_2xx(resp.meta.status) {
        return Ok(None);
    }

    // Accept the body only if it actually parses as JSON — Tier 2's contract is a
    // JSON data endpoint.
    match serde_json::from_slice::<serde_json::Value>(&resp.body) {
        Ok(value) => Ok(Some((
            value,
            format!("{} {} (score {})", winner.method, winner.url, score),
        ))),
        Err(_) => Ok(None),
    }
}

/// Capture seam used when the crate is built **without** the `tier2` feature:
/// there is no jail/runtime linked, so Tier 2 cannot run. The ladder records a
/// "built without tier2" note and finalizes `Unsupported`.
#[cfg(not(feature = "tier2"))]
pub(crate) struct DisabledCapture;

#[cfg(not(feature = "tier2"))]
#[async_trait]
impl Tier2Capture for DisabledCapture {
    async fn capture(
        &self,
        _url: &str,
        _html: &[u8],
        _config: &Config,
    ) -> Result<CaptureResult, DracoError> {
        Err(jail_error(
            JailKind::Spawn,
            "built without tier2: Tier 2 (jail-hosted V8 capture) is not compiled in",
        ))
    }
}

// ===========================================================================
// Tunables + small helpers (always on — used by rank+replay and the prod seam).
// ===========================================================================

/// Safety cap on captured requests per Hydrate (mirrors the runtime default).
#[cfg(feature = "tier2")]
const MAX_INTERCEPTS: u32 = 64;

/// Derive the quiesce window from the capture window: ~1/6th, clamped to a
/// sensible `[150, 500]` ms band so a short capture window still gets a chance to
/// idle-detect while a long one does not wait excessively.
#[cfg(feature = "tier2")]
fn default_quiesce_ms(capture_window_ms: u64) -> u64 {
    (capture_window_ms / 6).clamp(150, 500)
}

/// Is this an HTTP 2xx status?
fn is_2xx(status: u16) -> bool {
    (200..300).contains(&status)
}

/// Standard-alphabet base64 encoder (RFC 4648, with `=` padding) for request
/// bodies handed to replay. Kept local so draco-core does not take a base64 dep;
/// draco-net decodes with the `base64` crate's `STANDARD` engine, which matches.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((triple >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

// ===========================================================================
// tier2-gated: the production capture seam (spawns the jail, drives IPC).
// ===========================================================================

#[cfg(feature = "tier2")]
mod prod {
    use std::os::unix::net::UnixStream;

    use async_trait::async_trait;
    use draco_jail::frame::{self, FrameError};
    use draco_types::{DracoError, JailKind, JailToSupervisor, RuntimeOutcome, SupervisorToJail};

    use super::{
        default_quiesce_ms, jail_error, CaptureResult, Config, Tier2Capture, MAX_INTERCEPTS,
    };
    use crate::ranking::Candidate;

    /// Production [`Tier2Capture`]: spawns the jail child and drives one Hydrate.
    pub(crate) struct ProdTier2Capture;

    #[async_trait]
    impl Tier2Capture for ProdTier2Capture {
        async fn capture(
            &self,
            url: &str,
            html: &[u8],
            config: &Config,
        ) -> Result<CaptureResult, DracoError> {
            // The spawn + blocking IPC exchange runs off the async worker pool.
            let url = url.to_string();
            let html = html.to_vec();
            let config = config.clone();
            tokio::task::spawn_blocking(move || capture_blocking(&url, &html, &config))
                .await
                .map_err(|e| {
                    jail_error(
                        JailKind::Spawn,
                        format!("capture task panicked/cancelled: {e}"),
                    )
                })?
        }
    }

    /// Spawn the jail child, drive one `Hydrate`, and collect the capture result.
    /// **Blocking** — always called from `spawn_blocking`.
    fn capture_blocking(
        url: &str,
        html: &[u8],
        config: &Config,
    ) -> Result<CaptureResult, DracoError> {
        let mut handle = spawn(config)?;
        let ipc = handle.ipc_stream();

        // 1. Read the child's Ready handshake. A hard EOF / protocol error here
        //    usually means the child died during sandbox setup (seccomp kill, or
        //    namespaces refused) — surface it as a jail error.
        match frame::read_jail_frame(ipc) {
            Ok(f) => match f.header {
                JailToSupervisor::Ready { .. } => {}
                JailToSupervisor::Error { reason, detail } => {
                    return Err(jail_error(
                        reason,
                        format!("child error before ready: {detail}"),
                    ));
                }
                other => {
                    return Err(jail_error(
                        JailKind::Protocol,
                        format!("expected Ready, got {other:?}"),
                    ));
                }
            },
            Err(e) => return Err(map_frame_err(e, "reading Ready")),
        }

        // 2. Send Hydrate with the page HTML as the frame body.
        let hydrate = SupervisorToJail::Hydrate {
            url: url.to_string(),
            capture_window_ms: config.capture_window_ms,
            quiesce_ms: default_quiesce_ms(config.capture_window_ms),
            max_intercepts: MAX_INTERCEPTS,
            stub_response_json: "{}".to_string(),
        };
        frame::write_supervisor_frame(ipc, &hydrate, html)
            .map_err(|e| map_frame_err(e, "sending Hydrate"))?;

        // 3. Collect Intercept frames until the terminal Result.
        let mut candidates = Vec::new();
        let mut bodies = Vec::new();
        let outcome: RuntimeOutcome = loop {
            let f = match frame::read_jail_frame(ipc) {
                Ok(f) => f,
                Err(FrameError::Eof) => {
                    return Err(jail_error(
                        JailKind::Protocol,
                        "child closed IPC before sending a Result",
                    ));
                }
                Err(e) => return Err(map_frame_err(e, "collecting intercepts")),
            };
            match f.header {
                JailToSupervisor::Intercept {
                    method,
                    url,
                    headers,
                    has_body,
                    via,
                    ..
                } => {
                    candidates.push(Candidate {
                        method,
                        url,
                        headers,
                        via,
                    });
                    bodies.push(if has_body { Some(f.body) } else { None });
                }
                JailToSupervisor::Result { outcome, .. } => break outcome,
                // Diagnostic only; ignore for control flow.
                JailToSupervisor::Log { .. } => {}
                JailToSupervisor::Error { reason, detail } => {
                    return Err(jail_error(reason, detail));
                }
                JailToSupervisor::Ready { .. } => {
                    return Err(jail_error(
                        JailKind::Protocol,
                        "unexpected second Ready frame",
                    ));
                }
            }
        };

        // 4. Tell the child to shut down (best-effort; it may have already exited
        //    after emitting its Result) and reap it so we leave no zombie.
        let _ = frame::write_supervisor_frame(ipc, &SupervisorToJail::Shutdown, &[]);
        handle.finish();

        Ok(CaptureResult {
            candidates,
            bodies,
            outcome,
        })
    }

    /// Owns the supervisor side of the child so `capture_blocking` is agnostic to
    /// whether it was jailed (`spawn_jail`) or un-jailed (dev `no_jail`).
    enum Handle {
        Jailed(draco_jail::JailHandle),
        #[cfg(target_os = "linux")]
        Unjailed(unjailed::UnjailedChild),
    }

    impl Handle {
        fn ipc_stream(&mut self) -> &mut UnixStream {
            match self {
                Handle::Jailed(h) => h.ipc(),
                #[cfg(target_os = "linux")]
                Handle::Unjailed(h) => h.ipc(),
            }
        }

        fn finish(self) {
            match self {
                Handle::Jailed(h) => {
                    reap_pid(h.pid());
                    drop(h);
                }
                #[cfg(target_os = "linux")]
                Handle::Unjailed(h) => h.finish(),
            }
        }
    }

    /// Spawn the child: jailed by default, or un-jailed when `config.no_jail`.
    fn spawn(config: &Config) -> Result<Handle, DracoError> {
        if config.no_jail {
            #[cfg(target_os = "linux")]
            {
                return unjailed::spawn().map(Handle::Unjailed);
            }
            #[cfg(not(target_os = "linux"))]
            {
                // On non-Linux, spawn_jail() is already the un-jailed path (with a
                // warning), so there is no separate no_jail branch to take.
                return draco_jail::spawn_jail().map(Handle::Jailed).map_err(|e| {
                    jail_error(e.reason, format!("{} (no_jail on non-linux)", e.detail))
                });
            }
        }
        draco_jail::spawn_jail()
            .map(Handle::Jailed)
            .map_err(|e| jail_error(e.reason, e.detail))
    }

    /// Best-effort reap of a child pid so a completed jail run leaves no zombie.
    #[cfg(target_os = "linux")]
    fn reap_pid(pid: i32) {
        // SAFETY: waitpid on our own child pid; the child exits promptly after
        // emitting its Result and seeing Shutdown/EOF.
        unsafe {
            let mut status: libc::c_int = 0;
            libc::waitpid(pid, &mut status, 0);
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn reap_pid(_pid: i32) {
        // Non-Linux degraded spawn manages child lifetime itself.
    }

    /// Map a frame-codec error to the most specific [`DracoError::Jail`].
    fn map_frame_err(e: FrameError, ctx: &str) -> DracoError {
        match e {
            FrameError::Eof => jail_error(JailKind::Protocol, format!("{ctx}: unexpected EOF")),
            FrameError::Io(io) => jail_error(
                JailKind::Killed,
                format!("{ctx}: IPC I/O error (child likely killed): {io}"),
            ),
            other => jail_error(JailKind::Protocol, format!("{ctx}: {other}")),
        }
    }

    // -----------------------------------------------------------------------
    // Un-jailed dev path (`--no-jail`): fork + re-exec `draco __jail` WITHOUT
    // the namespace/seccomp/Landlock lockdown. Linux-only; local debugging.
    // -----------------------------------------------------------------------
    #[cfg(target_os = "linux")]
    mod unjailed {
        use std::ffi::CString;
        use std::os::fd::{IntoRawFd, OwnedFd};
        use std::os::unix::net::UnixStream;

        use draco_types::JailKind;

        use super::super::jail_error;
        use draco_types::DracoError;

        /// Fd the child inherits its IPC socket on — matches draco-jail's contract.
        const JAIL_IPC_FD: i32 = 3;

        /// A forked-but-un-jailed `draco __jail` child + the supervisor IPC end.
        pub(super) struct UnjailedChild {
            pid: i32,
            ipc: UnixStream,
        }

        impl UnjailedChild {
            pub(super) fn ipc(&mut self) -> &mut UnixStream {
                &mut self.ipc
            }

            pub(super) fn finish(self) {
                // SAFETY: reap our own child; it exits after Shutdown/EOF.
                unsafe {
                    let mut status: libc::c_int = 0;
                    libc::waitpid(self.pid, &mut status, 0);
                }
                drop(self.ipc);
            }
        }

        /// Spawn `draco __jail` un-jailed: socketpair → fork → (child) dup socket
        /// onto fd 3 and exec self; (parent) keep the supervisor end.
        ///
        /// Intentionally skips namespaces/seccomp/Landlock — for the `--no-jail`
        /// dev flag only. The re-exec target is the running executable, whose
        /// `__jail` hook routes into `draco_jail::run_jail_child`. We set
        /// [`draco_jail::JAIL_NO_SANDBOX_ENV`] in the child before exec so that
        /// entry skips arming the sandbox and runs the capture payload directly.
        pub(super) fn spawn() -> Result<UnjailedChild, DracoError> {
            eprintln!(
                "draco-core: WARNING — Tier 2 running with --no-jail: the V8 child has NO \
                 seccomp/netns/Landlock sandbox. Dev use only."
            );

            let (sup, child) = UnixStream::pair()
                .map_err(|e| jail_error(JailKind::Spawn, format!("socketpair: {e}")))?;

            let exe = std::env::current_exe()
                .map_err(|e| jail_error(JailKind::Spawn, format!("current_exe: {e}")))?;
            let exe_c = CString::new(exe.as_os_str().as_encoded_bytes())
                .map_err(|e| jail_error(JailKind::Spawn, format!("exe path has NUL: {e}")))?;
            let jail_arg =
                CString::new("__jail").expect("static literal \"__jail\" contains no NUL byte");
            // Marker name/value for the un-jailed child (set via setenv before
            // exec). Allocated pre-fork so the child arm is allocation-free.
            let env_name = CString::new(draco_jail::JAIL_NO_SANDBOX_ENV)
                .expect("env var name contains no NUL byte");
            let env_val = CString::new("1").expect("static \"1\" contains no NUL byte");

            let child_fd: OwnedFd = child.into();
            let child_raw = child_fd.into_raw_fd();

            // SAFETY: between fork and exec we call only async-signal-safe libc
            // functions (dup2/close/setenv/execv/_exit) and touch no Rust runtime
            // state. `setenv` is safe post-fork in the single-threaded child.
            match unsafe { libc::fork() } {
                -1 => {
                    // SAFETY: child_raw is a valid fd we still own here.
                    unsafe { libc::close(child_raw) };
                    drop(sup);
                    Err(jail_error(JailKind::Spawn, "fork failed"))
                }
                0 => {
                    // SAFETY: async-signal-safe calls only; abort hard on any
                    // failure so we never run supervisor code in the child.
                    unsafe {
                        if child_raw != JAIL_IPC_FD {
                            if libc::dup2(child_raw, JAIL_IPC_FD) < 0 {
                                libc::_exit(126);
                            }
                            libc::close(child_raw);
                        } else if libc::fcntl(child_raw, libc::F_SETFD, 0) < 0 {
                            libc::_exit(126);
                        }
                        // Tell the re-exec'd child to skip the sandbox.
                        if libc::setenv(env_name.as_ptr(), env_val.as_ptr(), 1) < 0 {
                            libc::_exit(125);
                        }
                        let argv = [exe_c.as_ptr(), jail_arg.as_ptr(), std::ptr::null()];
                        libc::execv(exe_c.as_ptr(), argv.as_ptr());
                        // exec failed; diverge so this arm has type `!` (coerces
                        // to the `Result` the other arms produce).
                        libc::_exit(127)
                    }
                }
                pid => {
                    // Parent: close the child end, keep the supervisor end.
                    // SAFETY: child_raw belongs to the child now; close our copy.
                    unsafe { libc::close(child_raw) };
                    Ok(UnjailedChild { pid, ipc: sup })
                }
            }
        }
    }
}

#[cfg(feature = "tier2")]
pub(crate) use prod::ProdTier2Capture;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::MockFetcher;
    use draco_types::InterceptVia;
    use serde_json::json;

    fn cand(url: &str, accept_json: bool, via: InterceptVia) -> Candidate {
        Candidate {
            method: "GET".into(),
            url: url.into(),
            headers: if accept_json {
                vec![("accept".into(), "application/json".into())]
            } else {
                Vec::new()
            },
            via,
        }
    }

    fn capture_of(candidates: Vec<Candidate>) -> CaptureResult {
        let bodies = vec![None; candidates.len()];
        CaptureResult {
            candidates,
            bodies,
            outcome: RuntimeOutcome::Quiesced,
        }
    }

    #[test]
    fn quiesce_is_clamped() {
        // Only meaningful with the prod seam compiled, but the helper is
        // tier2-gated, so guard the assertion behind the same cfg.
        #[cfg(feature = "tier2")]
        {
            assert_eq!(default_quiesce_ms(0), 150);
            assert_eq!(default_quiesce_ms(600), 150); // 100 → floored at 150
            assert_eq!(default_quiesce_ms(1_800), 300); // 300
            assert_eq!(default_quiesce_ms(9_000), 500); // 1500 → ceiled at 500
        }
    }

    #[test]
    fn base64_matches_standard_alphabet() {
        // Vectors from RFC 4648 §10.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(br#"{"q":1}"#), "eyJxIjoxfQ==");
    }

    // ---- rank_and_replay: the offline-unit-tested core (mock PageFetcher) ----

    #[tokio::test]
    async fn rank_and_replay_picks_winner_and_returns_json() {
        let capture = capture_of(vec![
            cand("https://cdn.example.com/app.js", false, InterceptVia::Fetch), // asset
            cand(
                "https://api.example.com/v1/items?q=1",
                true,
                InterceptVia::Fetch,
            ), // strong
        ]);
        let fetcher =
            MockFetcher::ok_html(200, "<ignored>").with_replay_json(200, json!({ "price": 42 }));
        let opts = draco_net::SessionOpts::default();

        let out = rank_and_replay(&capture, "https://api.example.com/", &opts, &fetcher)
            .await
            .unwrap();
        let (data, detail) = out.expect("a viable winner");
        assert_eq!(data, json!({ "price": 42 }));
        assert!(detail.contains("/v1/items"), "detail: {detail}");
        assert_eq!(fetcher.replay_calls(), 1);
    }

    #[tokio::test]
    async fn rank_and_replay_none_when_all_junk() {
        let capture = capture_of(vec![
            cand("https://cdn.example.com/app.js", false, InterceptVia::Fetch),
            cand(
                "https://www.google-analytics.com/collect",
                false,
                InterceptVia::Xhr,
            ),
        ]);
        let fetcher = MockFetcher::ok_html(200, "<ignored>");
        let opts = draco_net::SessionOpts::default();

        let out = rank_and_replay(&capture, "https://api.example.com/", &opts, &fetcher)
            .await
            .unwrap();
        assert!(out.is_none(), "no viable candidate should yield None");
        // Replay must not even be attempted when nothing is viable.
        assert_eq!(fetcher.replay_calls(), 0);
    }

    #[tokio::test]
    async fn rank_and_replay_none_when_replay_body_not_json() {
        let capture = capture_of(vec![cand(
            "https://api.example.com/v1/items",
            true,
            InterceptVia::Fetch,
        )]);
        // Replay returns 200 but an HTML (non-JSON) body.
        let fetcher = MockFetcher::ok_html(200, "<ignored>").with_replay_status(200);
        let opts = draco_net::SessionOpts::default();

        let out = rank_and_replay(&capture, "https://api.example.com/", &opts, &fetcher)
            .await
            .unwrap();
        assert!(
            out.is_none(),
            "non-JSON replay body must not finalize success"
        );
    }

    #[tokio::test]
    async fn rank_and_replay_none_on_non_2xx() {
        let capture = capture_of(vec![cand(
            "https://api.example.com/v1/items",
            true,
            InterceptVia::Fetch,
        )]);
        let fetcher = MockFetcher::ok_html(200, "<ignored>").with_replay_status(500);
        let opts = draco_net::SessionOpts::default();
        let out = rank_and_replay(&capture, "https://api.example.com/", &opts, &fetcher)
            .await
            .unwrap();
        assert!(out.is_none(), "5xx replay must not finalize success");
    }

    #[tokio::test]
    async fn rank_and_replay_propagates_replay_error() {
        use draco_types::NetKind;
        let capture = capture_of(vec![cand(
            "https://api.example.com/v1/items",
            true,
            InterceptVia::Fetch,
        )]);
        let fetcher = crate::testutil::err_replay_fetcher(DracoError::Network {
            reason: NetKind::Timeout,
            detail: "replay timed out".into(),
        });
        let opts = draco_net::SessionOpts::default();
        let err = rank_and_replay(&capture, "https://api.example.com/", &opts, &fetcher)
            .await
            .unwrap_err();
        assert!(matches!(err, DracoError::Network { .. }));
    }
}
