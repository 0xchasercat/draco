//! Linux jail implementation (canonical spec §7).
//!
//! Layers, applied by the child in order after re-exec:
//!
//! 1. **User + network namespace** — the child is `unshare`d into a fresh user
//!    namespace (so it can create further namespaces unprivileged) and a fresh
//!    network namespace with no interfaces except a downed loopback, giving it no
//!    routable network. Set up supervisor-side before/around the fork.
//! 2. **rlimits** — bound address space, file descriptors, cpu time, file size,
//!    and forbid core dumps.
//! 3. **Landlock** — deny all filesystem access beyond a tiny read-only allowlist
//!    (best-effort: silently degrades on kernels < 5.13).
//! 4. **Two-phase seccomp-bpf** — a bootstrap filter permitting the syscalls
//!    needed to finish setup, then a tight runtime filter (default
//!    `KILL_PROCESS`) permitting only what the payload loop needs. `mprotect` is
//!    permitted only when `PROT_EXEC` is clear (arg match).
//!
//! Runtime enforcement of layers 1, 3, and 4 requires a suitable kernel
//! (≥ 5.13 for Landlock; unprivileged user namespaces enabled) and can only be
//! validated on bare metal — see the `#[ignore]`d red-team tests.

use std::ffi::CString;
use std::os::fd::{IntoRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

use draco_types::JailKind;
use nix::sys::resource::{setrlimit, Resource};
use nix::sys::socket::{socketpair, AddressFamily, SockFlag, SockType};
use nix::unistd::{execv, fork, ForkResult};

use crate::{runtime_payload, JailError, JailHandle, JAIL_IPC_FD};

mod seccomp;

// ---------------------------------------------------------------------------
// rlimits
// ---------------------------------------------------------------------------

/// Address-space cap for the child. A jitless V8 isolate reserves large *virtual*
/// regions for its managed heap and cage even though resident memory stays small,
/// so `RLIMIT_AS` must be generous or `mmap` reservations fail at boot. 4 GiB
/// comfortably fits a single default-heap isolate; tune on bare metal alongside
/// the seccomp allowlist. (The supervisor's wall-clock `capture_window_ms` and
/// `RLIMIT_CPU` bound runaway compute independently.)
const RLIMIT_AS_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Max open file descriptors. The child needs stdio + fd 3 + a handful for the
/// runtime; 64 is comfortable and still tight.
const RLIMIT_NOFILE_MAX: u64 = 64;
/// Wall-independent CPU-seconds ceiling; a runaway child is SIGKILL'd by the
/// kernel. The supervisor also enforces a wall-clock `capture_window_ms`.
const RLIMIT_CPU_SECS: u64 = 30;
/// Largest file the child may create. It should never write files at all, so 0.
const RLIMIT_FSIZE_BYTES: u64 = 0;

/// Apply resource limits to the current (child) process.
fn apply_rlimits() -> Result<(), JailError> {
    let set = |res: Resource, soft: u64, hard: u64, what: &str| -> Result<(), JailError> {
        setrlimit(res, soft, hard)
            .map_err(|e| JailError::new(JailKind::Spawn, format!("setrlimit {what}: {e}")))
    };
    set(Resource::RLIMIT_AS, RLIMIT_AS_BYTES, RLIMIT_AS_BYTES, "AS")?;
    set(
        Resource::RLIMIT_NOFILE,
        RLIMIT_NOFILE_MAX,
        RLIMIT_NOFILE_MAX,
        "NOFILE",
    )?;
    set(
        Resource::RLIMIT_CPU,
        RLIMIT_CPU_SECS,
        RLIMIT_CPU_SECS,
        "CPU",
    )?;
    set(
        Resource::RLIMIT_FSIZE,
        RLIMIT_FSIZE_BYTES,
        RLIMIT_FSIZE_BYTES,
        "FSIZE",
    )?;
    // Forbid core dumps outright so a crash cannot leak memory to disk.
    set(Resource::RLIMIT_CORE, 0, 0, "CORE")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Namespaces
// ---------------------------------------------------------------------------

/// Enter a fresh **user** namespace and a fresh **network** namespace.
///
/// The user namespace lets an unprivileged process gain a full set of
/// capabilities *inside* the namespace, which is what makes the subsequent
/// `CLONE_NEWNET` succeed without root. The network namespace has no configured
/// interfaces, so the child has no route off-box — the network air-gap.
///
/// # Fallback
///
/// If the host forbids unprivileged user namespaces (some hardened distros set
/// `kernel.unprivileged_userns_clone=0` or `user.max_user_namespaces=0`), the
/// `unshare(CLONE_NEWUSER)` call fails with `EPERM`. We surface that as a
/// [`JailKind::NamespaceSetup`] error; the caller (spec §7 fallback) may then
/// choose to proceed with only seccomp + Landlock (still a strong sandbox) or to
/// abort. We do **not** silently continue un-air-gapped.
fn enter_namespaces() -> Result<(), JailError> {
    use nix::sched::{unshare, CloneFlags};

    // A new user namespace first so the unprivileged process can create the
    // network namespace. `setgroups`/uid_map/gid_map wiring is intentionally
    // left to bare-metal validation; the identity mapping is enough for the
    // air-gap and the reduced-privilege posture we need here.
    unshare(CloneFlags::CLONE_NEWUSER).map_err(|e| {
        JailError::new(
            JailKind::NamespaceSetup,
            format!(
                "unshare(CLONE_NEWUSER) failed: {e}. Host likely forbids unprivileged user \
                 namespaces; enable them or run seccomp+Landlock-only per spec §7 fallback."
            ),
        )
    })?;

    unshare(CloneFlags::CLONE_NEWNET).map_err(|e| {
        JailError::new(
            JailKind::NamespaceSetup,
            format!("unshare(CLONE_NEWNET) failed: {e}"),
        )
    })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Landlock (best-effort FS lockdown)
// ---------------------------------------------------------------------------

/// Apply a Landlock ruleset denying all filesystem access beyond a minimal
/// read-only allowlist. Best-effort: on kernels without Landlock (< 5.13) this
/// logs a warning and returns `Ok`, because the seccomp layer still blocks the
/// file-opening syscalls outright.
fn apply_landlock() -> Result<(), JailError> {
    use landlock::{
        Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus, ABI,
    };

    // Target the broadest ABI we can; BestEffort downgrades on older kernels.
    let abi = ABI::V1;

    // A read-only allowlist just wide enough for a dynamic loader / minimal
    // runtime. Slice 3 (V8 snapshot) will extend this with the snapshot path.
    let ro_paths = ["/usr", "/lib", "/lib64", "/etc/ld.so.cache"];

    let status = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| JailError::new(JailKind::NamespaceSetup, format!("landlock handle: {e}")))?
        .create()
        .map_err(|e| JailError::new(JailKind::NamespaceSetup, format!("landlock create: {e}")))?
        .add_rules(landlock::path_beneath_rules(
            ro_paths,
            AccessFs::from_read(abi),
        ))
        .map_err(|e| JailError::new(JailKind::NamespaceSetup, format!("landlock rules: {e}")))?
        .restrict_self()
        .map_err(|e| JailError::new(JailKind::NamespaceSetup, format!("landlock restrict: {e}")))?;

    match status.ruleset {
        RulesetStatus::FullyEnforced => {}
        RulesetStatus::PartiallyEnforced => {
            eprintln!("draco-jail: Landlock only PARTIALLY enforced on this kernel");
        }
        RulesetStatus::NotEnforced => {
            eprintln!(
                "draco-jail: WARNING — Landlock NOT enforced (kernel < 5.13?); relying on \
                 seccomp to block filesystem syscalls"
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Supervisor side
// ---------------------------------------------------------------------------

/// Locate the current executable so the child can re-exec it as `draco __jail`.
fn self_exe() -> Result<CString, JailError> {
    let path = std::env::current_exe()
        .map_err(|e| JailError::new(JailKind::Spawn, format!("current_exe: {e}")))?;
    let bytes = path.as_os_str().as_encoded_bytes();
    CString::new(bytes)
        .map_err(|e| JailError::new(JailKind::Spawn, format!("exe path has NUL: {e}")))
}

/// Supervisor-side spawn. Creates the socketpair, forks, and in the child
/// dup's its socket end onto fd 3 and re-execs `<self> __jail`.
///
/// The heavy lockdown (namespaces/rlimits/Landlock/seccomp) is applied by the
/// re-exec'd child in [`run_jail_child`] rather than between fork and exec, so
/// that a single code path arms the sandbox regardless of how `draco __jail` is
/// entered.
pub fn spawn_jail() -> Result<JailHandle, JailError> {
    // Bidirectional stream socketpair; CLOEXEC so neither end leaks across the
    // exec except the one we deliberately place on fd 3.
    let (sup_fd, child_fd) = socketpair(
        AddressFamily::Unix,
        SockType::Stream,
        None,
        SockFlag::SOCK_CLOEXEC,
    )
    .map_err(|e| JailError::new(JailKind::Spawn, format!("socketpair: {e}")))?;

    let exe = self_exe()?;
    let jail_arg = CString::new("__jail").expect("static literal \"__jail\" contains no NUL byte");
    let argv = [exe.as_c_str(), jail_arg.as_c_str()];

    // SAFETY: between fork and exec we call only async-signal-safe operations
    // (dup2/close via libc and execv). We do not allocate, take locks, or touch
    // Rust runtime state that could be inconsistent in the forked child.
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => {
            // Parent keeps the supervisor end; drop the child end.
            drop(child_fd);
            let ipc = ownedfd_to_stream(sup_fd);
            Ok(JailHandle {
                pid: child.as_raw(),
                ipc,
            })
        }
        Ok(ForkResult::Child) => {
            // Move the child socket onto fd 3, clearing CLOEXEC on the dup.
            place_on_fd3_and_exec(child_fd, &argv);
            // place_on_fd3_and_exec never returns on success; if it does, exec
            // failed. Abort hard — we must not run un-jailed code in the child.
            unsafe { libc::_exit(127) };
        }
        Err(e) => {
            drop(sup_fd);
            drop(child_fd);
            Err(JailError::new(JailKind::Spawn, format!("fork: {e}")))
        }
    }
}

/// Convert an `OwnedFd` into a `UnixStream` without an extra dup.
fn ownedfd_to_stream(fd: OwnedFd) -> UnixStream {
    UnixStream::from(fd)
}

/// In the freshly-forked child: dup the socket onto fd 3 and exec. Only
/// async-signal-safe libc calls here. Never returns on success.
fn place_on_fd3_and_exec(child_fd: OwnedFd, argv: &[&std::ffi::CStr]) {
    let raw = child_fd.into_raw_fd();
    if raw == JAIL_IPC_FD {
        // The socket already sits on fd 3. `dup2(fd, fd)` is a no-op that does
        // NOT clear the close-on-exec flag, and the socketpair was created with
        // SOCK_CLOEXEC — so we must clear CLOEXEC explicitly or it would vanish
        // across the exec, breaking IPC.
        // SAFETY: raw is a valid fd we own; clearing FD flags is safe.
        if unsafe { libc::fcntl(raw, libc::F_SETFD, 0) } < 0 {
            unsafe { libc::_exit(126) };
        }
    } else {
        // dup2 onto fd 3 clears CLOEXEC on the new descriptor, so fd 3 survives
        // the exec.
        // SAFETY: raw is a valid open fd we own; JAIL_IPC_FD is a valid target.
        if unsafe { libc::dup2(raw, JAIL_IPC_FD) } < 0 {
            // Cannot set up IPC; refuse to exec.
            unsafe { libc::_exit(126) };
        }
        // Close the original to avoid leaking a stray copy into the child.
        // SAFETY: raw is a valid fd we own and no longer need.
        unsafe { libc::close(raw) };
    }
    // Re-exec ourselves as `draco __jail`. On success this never returns.
    let _ = execv(argv[0], argv);
    // Fall through only on exec failure.
}

// ---------------------------------------------------------------------------
// Child side
// ---------------------------------------------------------------------------

/// Child-side entry (`draco __jail`): arm every sandbox layer over the inherited
/// fd-3 socket, then run the payload loop. Never returns.
pub fn run_jail_child() -> ! {
    match arm_and_run() {
        Ok(()) => {
            // Orderly shutdown.
            std::process::exit(0);
        }
        Err(e) => {
            // Best-effort structured error back to the supervisor before exit.
            report_fatal(&e);
            std::process::exit(1);
        }
    }
}

/// Apply the lockdown layers in order, then hand off to the payload loop.
fn arm_and_run() -> Result<(), JailError> {
    // Dev escape hatch: `--no-jail` sets `JAIL_NO_SANDBOX_ENV` before re-exec, so
    // the child runs the capture payload WITHOUT any lockdown. This keeps the
    // `__jail` hook a single `run_jail_child()` call regardless of jail vs no-jail
    // (the supervisor's env marker is the only difference). Never set in prod.
    if std::env::var_os(crate::JAIL_NO_SANDBOX_ENV).is_some() {
        eprintln!(
            "draco-jail: WARNING — jailed child running UN-JAILED via {} (no seccomp/netns/\
             Landlock). Dev use only.",
            crate::JAIL_NO_SANDBOX_ENV
        );
        return runtime_payload::run_child_over_fd3()
            .map_err(|e| JailError::new(JailKind::Protocol, format!("payload: {e}")));
    }

    // 1. Namespaces (air-gap). A fallback-aware caller may tolerate failure here;
    //    for the scaffold we treat it as fatal so an un-air-gapped run is loud.
    enter_namespaces()?;

    // 2. rlimits.
    apply_rlimits()?;

    // 3. Landlock FS lockdown (best-effort).
    apply_landlock()?;

    // 4a. Phase-1 seccomp: permit setup syscalls (superset). Installed now, while
    //     we still need to open the IPC fd view etc.
    seccomp::install_bootstrap_filter()
        .map_err(|e| JailError::new(JailKind::SeccompInstall, format!("phase-1: {e}")))?;

    // 4b. Phase-2 seccomp: the tight runtime filter. After this, file-open,
    //     network, and process-creation syscalls are KILL_PROCESS.
    seccomp::install_runtime_filter()
        .map_err(|e| JailError::new(JailKind::SeccompInstall, format!("phase-2: {e}")))?;

    // 5. Payload: host the Tier 2 V8 capture — read a `Hydrate`, drive
    //    `draco-runtime`, stream `Intercept`s + a terminal `Result`, then exit.
    //    Plain sync: `run_capture` owns its own current-thread tokio runtime, so
    //    we must NOT be inside one here (nesting would panic).
    runtime_payload::run_child_over_fd3()
        .map_err(|e| JailError::new(JailKind::Protocol, format!("payload: {e}")))
}

/// Try to send a structured `Error` frame to the supervisor on fd 3 before we
/// die. Best-effort: if the channel is gone we just log to stderr.
fn report_fatal(err: &JailError) {
    use draco_types::JailToSupervisor;
    use std::os::fd::FromRawFd;

    // SAFETY: fd 3 is our IPC endpoint. This runs on the fatal path only; we
    // deliberately leak the stream (into_raw_fd via forget) so its Drop does not
    // double-close a descriptor the OS is about to reclaim on exit anyway.
    let mut stream = unsafe { UnixStream::from_raw_fd(JAIL_IPC_FD) };
    let frame_hdr = JailToSupervisor::Error {
        reason: err.reason,
        detail: err.detail.clone(),
    };
    if let Err(e) = crate::frame::write_jail_frame(&mut stream, &frame_hdr, &[]) {
        eprintln!("draco-jail: fatal {err}; could not notify supervisor: {e}");
    } else {
        eprintln!("draco-jail: fatal {err}");
    }
    std::mem::forget(stream);
}

// ===========================================================================
// Red-team tests.
//
// Every test here forks the test process, installs the *runtime* seccomp
// filter in the child, then has the child attempt a syscall that the filter
// must forbid. The parent asserts the child was terminated by `SIGSYS`
// (seccomp `KillProcess`). A distinctive `_exit` code is used if the forbidden
// syscall unexpectedly *returns*, so a policy regression fails loudly rather
// than silently passing.
//
// These are `#[ignore]`d: they require a kernel that honours seccomp
// `KILL_PROCESS` and (for the full-jail smoke test) unprivileged user
// namespaces + Landlock (kernel >= 5.13). They are validated on bare-metal
// Linux, not in the build sandbox. Run with:
//     cargo test -p draco-jail -- --ignored --test-threads=1
// ===========================================================================
#[cfg(test)]
mod redteam {
    use super::*;
    use nix::sys::signal::Signal;
    use nix::sys::wait::{waitpid, WaitStatus};
    use nix::unistd::{fork, ForkResult};

    /// Exit code the child uses if a syscall that *should* have been killed
    /// instead returned control. Seeing this in the parent is a policy failure.
    const NOT_KILLED: i32 = 42;

    /// Fork, run `child_body` under the phase-2 runtime seccomp filter, and
    /// return the parent's observed wait status.
    ///
    /// # Safety
    ///
    /// `child_body` runs post-fork in a process that shares the test harness's
    /// address space; it must call only async-signal-safe operations and must
    /// end in `_exit`. We uphold that in each call site below.
    fn run_under_runtime_filter(child_body: fn() -> !) -> WaitStatus {
        // SAFETY: the child calls only raw libc syscalls + _exit; see per-body
        // notes. The parent only waits.
        match unsafe { fork() }.expect("fork") {
            ForkResult::Child => {
                // Arm the tight runtime filter, then run the probe.
                if super::seccomp::install_runtime_filter().is_err() {
                    // Could not install (e.g. seccomp unavailable): exit with a
                    // sentinel distinct from NOT_KILLED so the test can tell.
                    unsafe { libc::_exit(7) };
                }
                child_body();
            }
            ForkResult::Parent { child } => waitpid(child, None).expect("waitpid"),
        }
    }

    fn assert_killed_by_sigsys(status: WaitStatus) {
        match status {
            WaitStatus::Signaled(_, Signal::SIGSYS, _) => {}
            WaitStatus::Exited(_, NOT_KILLED) => {
                panic!("forbidden syscall RETURNED — seccomp policy did not kill the child")
            }
            other => panic!("expected SIGSYS kill, got {other:?}"),
        }
    }

    #[test]
    #[ignore = "needs bare-metal kernel honouring seccomp KILL_PROCESS"]
    fn connect_is_killed() {
        fn body() -> ! {
            // Attempt a raw connect() to a dummy address. `socket`/`connect` are
            // not in the runtime allow-list, so this must SIGSYS.
            // SAFETY: raw syscall with a stack-local sockaddr; async-signal-safe.
            unsafe {
                let addr: libc::sockaddr_in = std::mem::zeroed();
                libc::syscall(
                    libc::SYS_connect,
                    3, // any fd; the filter kills before the arg is honoured
                    &addr as *const _ as usize,
                    std::mem::size_of::<libc::sockaddr_in>(),
                );
                libc::_exit(NOT_KILLED);
            }
        }
        assert_killed_by_sigsys(run_under_runtime_filter(body));
    }

    #[test]
    #[ignore = "needs bare-metal kernel honouring seccomp KILL_PROCESS"]
    fn open_etc_passwd_is_killed() {
        fn body() -> ! {
            // openat("/etc/passwd") — file opens are not allowed at runtime.
            // SAFETY: raw syscall with a static NUL-terminated path.
            unsafe {
                let path = b"/etc/passwd\0";
                libc::syscall(
                    libc::SYS_openat,
                    libc::AT_FDCWD,
                    path.as_ptr(),
                    libc::O_RDONLY,
                );
                libc::_exit(NOT_KILLED);
            }
        }
        assert_killed_by_sigsys(run_under_runtime_filter(body));
    }

    #[test]
    #[ignore = "needs bare-metal kernel honouring seccomp KILL_PROCESS"]
    fn fork_is_killed() {
        fn body() -> ! {
            // clone/fork are absent from the allow-list. On x86_64 glibc's
            // fork() routes through clone(); invoke the raw clone syscall so the
            // probe is arch-honest.
            // SAFETY: raw syscall, no args dereferenced.
            unsafe {
                #[cfg(target_arch = "x86_64")]
                libc::syscall(libc::SYS_clone, libc::SIGCHLD as usize, 0, 0, 0, 0);
                #[cfg(not(target_arch = "x86_64"))]
                libc::syscall(libc::SYS_clone, 0usize, 0, 0, 0, 0);
                libc::_exit(NOT_KILLED);
            }
        }
        assert_killed_by_sigsys(run_under_runtime_filter(body));
    }

    #[test]
    #[ignore = "needs bare-metal kernel honouring seccomp KILL_PROCESS"]
    fn mprotect_with_exec_is_killed_but_without_exec_is_allowed() {
        // PROT_EXEC set -> killed.
        fn exec_body() -> ! {
            // SAFETY: mmap a page then flip it to RWX; the filter kills on the
            // PROT_EXEC arg. All raw, async-signal-safe syscalls.
            unsafe {
                let len = 4096usize;
                let p = libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                );
                if p == libc::MAP_FAILED {
                    libc::_exit(9);
                }
                // This mprotect sets PROT_EXEC and must be killed.
                libc::syscall(
                    libc::SYS_mprotect,
                    p as usize,
                    len,
                    (libc::PROT_READ | libc::PROT_EXEC) as usize,
                );
                libc::_exit(NOT_KILLED);
            }
        }
        assert_killed_by_sigsys(run_under_runtime_filter(exec_body));

        // PROT_EXEC clear -> allowed (child exits 0, not SIGSYS).
        fn noexec_body() -> ! {
            // SAFETY: as above but the mprotect keeps PROT_EXEC clear, so it is
            // permitted and the child exits cleanly.
            unsafe {
                let len = 4096usize;
                let p = libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                );
                if p == libc::MAP_FAILED {
                    libc::_exit(9);
                }
                let rc = libc::syscall(
                    libc::SYS_mprotect,
                    p as usize,
                    len,
                    libc::PROT_READ as usize,
                );
                libc::_exit(if rc == 0 { 0 } else { 8 });
            }
        }
        match run_under_runtime_filter(noexec_body) {
            WaitStatus::Exited(_, 0) => {}
            other => panic!("PROT_EXEC-clear mprotect should be allowed, got {other:?}"),
        }
    }

    /// Full-jail smoke test: spawn the real jailed child via re-exec and drive
    /// one Hydrate + Shutdown over IPC. Requires the binary to honour the
    /// `draco __jail` argv hook (only true from the `draco` binary, not the test
    /// harness), plus unprivileged userns + Landlock, so it is doubly gated.
    #[test]
    #[ignore = "needs the `draco __jail` re-exec hook + kernel >= 5.13 with unprivileged userns"]
    fn full_jail_roundtrip_smoke() {
        use crate::frame::{read_jail_frame, write_supervisor_frame};
        use draco_types::{JailToSupervisor, RuntimeOutcome, SupervisorToJail};

        let mut handle = spawn_jail().expect("spawn_jail");

        // Child announces Ready.
        let ready = read_jail_frame(handle.ipc()).expect("read Ready");
        assert!(matches!(ready.header, JailToSupervisor::Ready { .. }));

        // Drive a Hydrate; expect a terminal Result.
        write_supervisor_frame(
            handle.ipc(),
            &SupervisorToJail::Hydrate {
                url: "https://example.com".into(),
                capture_window_ms: 500,
                quiesce_ms: 50,
                max_intercepts: 4,
                stub_response_json: "{}".into(),
            },
            b"<html></html>",
        )
        .expect("write Hydrate");
        let result = read_jail_frame(handle.ipc()).expect("read Result");
        assert!(matches!(
            result.header,
            JailToSupervisor::Result {
                outcome: RuntimeOutcome::NoIntercepts,
                ..
            }
        ));

        // The Slice 4 child handles one Hydrate and exits (capture is
        // once-per-process), so it may already be gone by now; a Shutdown write
        // is best-effort (EPIPE if the child already closed the socket).
        let _ = write_supervisor_frame(handle.ipc(), &SupervisorToJail::Shutdown, &[]);
    }
}
