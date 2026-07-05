//! Linux jail implementation (canonical spec §7).
//!
//! The child arms these layers in order after re-exec. On Linux the achieved
//! posture is reported as `hardened` (the seccomp OS-sandbox is engaged):
//!
//! 1. **seccomp-bpf (required, always engaged).** By default a **robust denylist**
//!    (default `Allow`, `KILL_PROCESS` only a curated breakout set — see
//!    [`seccomp`]); with `--strict-sandbox`, the historical two-phase default-deny
//!    allowlist. Because the denylist kills `socket`/`connect`, it is itself the
//!    network air-gap and needs no per-host tuning. If seccomp cannot be installed
//!    the child dies and the supervisor surfaces a jail error.
//! 2. **network namespace (defense-in-depth, best-effort).** A fresh user + net
//!    namespace, when unprivileged userns is available. No longer *required* for
//!    the air-gap (seccomp provides it); a soft failure just drops this layer.
//! 3. **Landlock (defense-in-depth, best-effort).** Deny-all-but-a-read-only
//!    allowlist; silently absent on kernels < 5.13.
//! 4. **rlimits** — bound address space, fds, cpu time, file size; no core dumps.
//!
//! The seccomp layer is the real containment of hostile page JS *in addition to*
//! the V8 isolate having no host-capability ops. netns + Landlock harden against a
//! V8-engine exploit. Their runtime enforcement can only be validated on bare
//! metal — see the `#[ignore]`d red-team tests.

use std::ffi::CString;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

use draco_types::{JailKind, JailToSupervisor};
use nix::sys::resource::{setrlimit, Resource};
use nix::sys::socket::{socketpair, AddressFamily, SockFlag, SockType};
use nix::unistd::{execv, fork, ForkResult};

use crate::level::{self, SandboxLevel};
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

/// Try to enter a fresh **user** namespace and a fresh **network** namespace.
///
/// The user namespace lets an unprivileged process gain capabilities *inside* the
/// namespace, which is what makes the subsequent `CLONE_NEWNET` succeed without
/// root; the network namespace has no configured interfaces, so the child has no
/// route off-box.
///
/// **Best-effort.** With the denylist seccomp filter the network air-gap comes
/// from killing `socket`/`connect`, so netns is now purely defense-in-depth
/// against a V8-engine exploit. If the host forbids unprivileged user namespaces
/// (some hardened distros set `kernel.unprivileged_userns_clone=0` or
/// `user.max_user_namespaces=0`), we simply skip this layer and report it in the
/// achieved level — we do **not** fail the run.
fn try_enter_namespaces() -> level::NetnsStatus {
    use nix::sched::{unshare, CloneFlags};

    // A new user namespace first so the unprivileged process can create the
    // network namespace. `setgroups`/uid_map/gid_map wiring is intentionally
    // left to bare-metal validation; the identity mapping is enough for the
    // reduced-privilege posture we want here.
    if unshare(CloneFlags::CLONE_NEWUSER).is_err() {
        return level::NetnsStatus::UsernsUnavailable;
    }
    if unshare(CloneFlags::CLONE_NEWNET).is_err() {
        return level::NetnsStatus::NetnsFailed;
    }
    level::NetnsStatus::Engaged
}

// ---------------------------------------------------------------------------
// Landlock (best-effort FS lockdown)
// ---------------------------------------------------------------------------

/// Apply a Landlock ruleset denying all filesystem access beyond a minimal
/// read-only allowlist, returning how much of it the kernel actually enforced.
///
/// **Best-effort defense-in-depth.** On kernels without Landlock (< 5.13) this
/// returns [`level::LandlockStatus::Unavailable`] and the run continues; the
/// seccomp layer is the primary containment. A hard error building the ruleset is
/// still surfaced (misconfiguration, not a missing-kernel-feature case).
fn apply_landlock() -> Result<level::LandlockStatus, JailError> {
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

    Ok(match status.ruleset {
        RulesetStatus::FullyEnforced => level::LandlockStatus::Enforced,
        RulesetStatus::PartiallyEnforced => level::LandlockStatus::Partial,
        RulesetStatus::NotEnforced => level::LandlockStatus::Unavailable,
    })
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

/// Supervisor-side spawn (default posture). Creates the socketpair, forks, and in
/// the child dup's its socket end onto fd 3 and re-execs `<self> __jail`.
///
/// The heavy lockdown (seccomp/netns/Landlock/rlimits) is applied by the re-exec'd
/// child in [`run_jail_child`] rather than between fork and exec, so a single code
/// path arms the sandbox regardless of how `draco __jail` is entered.
///
/// Signature frozen by the workspace contract; delegates to [`spawn_jail_with`].
pub fn spawn_jail() -> Result<JailHandle, JailError> {
    spawn_jail_with(false)
}

/// Supervisor-side spawn, selecting the seccomp model. `strict = false` (default)
/// arms the robust denylist; `strict = true` arms the strict default-deny
/// allowlist (`--strict-sandbox`). The choice is communicated to the re-exec'd
/// child via the [`crate::JAIL_STRICT_ENV`] marker, set async-signal-safely in the
/// forked child before `execv`.
pub fn spawn_jail_with(strict: bool) -> Result<JailHandle, JailError> {
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
    // Marker name/value for strict mode, allocated pre-fork so the child arm is
    // allocation-free (setenv in the child only when strict).
    let strict_env = CString::new(crate::JAIL_STRICT_ENV).expect("env var name has no NUL byte");
    let strict_val = CString::new("1").expect("static \"1\" has no NUL byte");

    // SAFETY: between fork and exec we call only async-signal-safe operations
    // (dup2/close/setenv via libc and execv). We do not allocate, take locks, or
    // touch Rust runtime state that could be inconsistent in the forked child.
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
            // Request strict mode via the environment before exec (child only).
            // SAFETY: single-threaded forked child; setenv is async-signal-safe
            // enough here (no allocation beyond libc's own, no Rust locks).
            if strict && unsafe { libc::setenv(strict_env.as_ptr(), strict_val.as_ptr(), 1) } < 0 {
                unsafe { libc::_exit(125) };
            }
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

/// Apply the lockdown layers, compute the achieved sandbox level, then hand off
/// to the payload loop (which reports the level to the supervisor over fd 3).
fn arm_and_run() -> Result<(), JailError> {
    // Escape hatch: `--no-jail` sets `JAIL_NO_SANDBOX_ENV` before re-exec, so the
    // child runs the capture payload WITHOUT the OS sandbox. Tier 2 still hosts V8
    // with no host-capability bindings (the real containment of page JS); the OS
    // sandbox is the defense-in-depth layer being skipped. This keeps the `__jail`
    // hook a single `run_jail_child()` call regardless of jail vs no-jail — the
    // supervisor's env marker is the only difference, and the supervisor prints the
    // single informational line about the skipped hardening. No warning here.
    if std::env::var_os(crate::JAIL_NO_SANDBOX_ENV).is_some() {
        return run_payload(SandboxLevel::isolate_no_jail());
    }

    // Strict mode marker (from `--strict-sandbox`): pick the default-deny allowlist
    // instead of the robust denylist. Set by `spawn_jail_with(true)` before exec.
    let strict = std::env::var_os(crate::JAIL_STRICT_ENV).is_some();

    // 1. netns air-gap layer — best-effort, defense-in-depth (the seccomp denylist
    //    already blocks the network by killing socket/connect).
    let netns = try_enter_namespaces();

    // 2. rlimits (always applied; a failure here is a real setup error).
    apply_rlimits()?;

    // 3. Landlock FS lockdown — best-effort, defense-in-depth.
    let landlock = apply_landlock()?;

    // 4. seccomp — the primary containment. Denylist by default (robust, no
    //    per-host tuning); strict two-phase allowlist under `--strict-sandbox`.
    //    After this the dangerous syscalls are KILL_PROCESS. Install last so the
    //    setup calls above are unconstrained (denylist allows them anyway; the
    //    strict bootstrap phase permits the setup-only syscalls, then phase-2
    //    narrows). A seccomp install failure IS fatal — it is the layer we require.
    if strict {
        seccomp::install_strict_bootstrap_filter().map_err(|e| {
            JailError::new(JailKind::SeccompInstall, format!("strict phase-1: {e}"))
        })?;
        seccomp::install_strict_runtime_filter().map_err(|e| {
            JailError::new(JailKind::SeccompInstall, format!("strict phase-2: {e}"))
        })?;
    } else {
        seccomp::install_denylist_filter()
            .map_err(|e| JailError::new(JailKind::SeccompInstall, format!("denylist: {e}")))?;
    }

    run_payload(SandboxLevel::hardened(strict, netns, landlock))
}

/// Host the Tier 2 V8 capture payload over fd 3, telling it the achieved sandbox
/// level so it can surface it to the supervisor (as a `Log` frame after `Ready`).
///
/// Plain sync: `run_capture` (deep inside) owns its own current-thread tokio
/// runtime, so we must NOT be inside one here (nesting would panic).
fn run_payload(level: SandboxLevel) -> Result<(), JailError> {
    runtime_payload::run_child_over_fd3(Some(&level.describe()))
        .map_err(|e| JailError::new(JailKind::Protocol, format!("payload: {e}")))
}

/// Try to send a structured `Error` frame to the supervisor on fd 3 before we
/// die. Best-effort: if the channel is gone we just log to stderr.
fn report_fatal(err: &JailError) {
    // SAFETY: fd 3 is our IPC endpoint. This runs on the fatal path only; we
    // deliberately leak the stream (via forget) so its Drop does not double-close
    // a descriptor the OS is about to reclaim on exit anyway.
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
// Red-team tests — denylist model (default).
//
// Each test forks the test process, installs a seccomp filter in the child, then
// has the child attempt a syscall. For a killed syscall the parent asserts the
// child was terminated by `SIGSYS` (seccomp `KillProcess`), using a distinctive
// `_exit` code if the syscall unexpectedly *returns* so a regression fails loudly.
// For an *allowed* syscall the parent asserts the child was NOT SIGSYS-killed.
//
// The default policy is the **denylist**: `KILL_PROCESS` only a curated breakout
// set, everything else `Allow`. So `execve`/`connect`/`ptrace`/`mprotect(EXEC)`
// are killed, while `openat` and `clone`/`fork` are ALLOWED — the filesystem is
// denied by **Landlock**, not seccomp, and thread creation must work for V8/tokio.
// The strict allowlist model (opt-in) is exercised by its own tests below.
//
// These are `#[ignore]`d: they require a kernel that honours seccomp
// `KILL_PROCESS` and (for the full-jail smoke test) unprivileged user namespaces
// + Landlock (kernel >= 5.13). Validated on bare-metal Linux, not in the build
// sandbox. Run with:
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
    /// Exit code the child uses when an *allowed* syscall returned as expected.
    const ALLOWED_OK: i32 = 0;

    /// Which seccomp model to arm in the forked probe child.
    #[derive(Clone, Copy)]
    enum Model {
        /// The default robust denylist (default Allow, kill the breakout set).
        Denylist,
        /// The strict default-deny allowlist (phase-2 runtime filter).
        StrictRuntime,
    }

    /// Fork, arm `model`'s seccomp filter in the child, run `child_body`, and
    /// return the parent's observed wait status.
    ///
    /// # Safety
    ///
    /// `child_body` runs post-fork in a process that shares the test harness's
    /// address space; it must call only async-signal-safe operations and must end
    /// in `_exit`. Upheld at each call site below.
    fn run_under(model: Model, child_body: fn() -> !) -> WaitStatus {
        // SAFETY: the child calls only raw libc syscalls + _exit; see per-body
        // notes. The parent only waits.
        match unsafe { fork() }.expect("fork") {
            ForkResult::Child => {
                let armed = match model {
                    Model::Denylist => super::seccomp::install_denylist_filter().is_ok(),
                    Model::StrictRuntime => super::seccomp::install_strict_runtime_filter().is_ok(),
                };
                if !armed {
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

    fn assert_not_killed(status: WaitStatus) {
        match status {
            WaitStatus::Signaled(_, Signal::SIGSYS, _) => {
                panic!("an allowed syscall was SIGSYS-killed — policy is too tight")
            }
            WaitStatus::Exited(_, ALLOWED_OK) => {}
            // Any non-SIGSYS exit is acceptable "not killed" (e.g. the syscall
            // itself errored). We only fail on a SIGSYS.
            _ => {}
        }
    }

    // ---- denylist: killed breakout syscalls --------------------------------

    #[test]
    #[ignore = "needs bare-metal kernel honouring seccomp KILL_PROCESS"]
    fn connect_is_killed_by_denylist() {
        fn body() -> ! {
            // `socket`/`connect` are in the kill-set — this is the network air-gap.
            // SAFETY: raw syscall with a stack-local sockaddr; async-signal-safe.
            unsafe {
                let addr: libc::sockaddr_in = std::mem::zeroed();
                libc::syscall(
                    libc::SYS_connect,
                    3,
                    &addr as *const _ as usize,
                    std::mem::size_of::<libc::sockaddr_in>(),
                );
                libc::_exit(NOT_KILLED);
            }
        }
        assert_killed_by_sigsys(run_under(Model::Denylist, body));
    }

    #[test]
    #[ignore = "needs bare-metal kernel honouring seccomp KILL_PROCESS"]
    fn execve_is_killed_by_denylist() {
        fn body() -> ! {
            // `execve` is killed: a forked child can spawn no new program image.
            // SAFETY: raw syscall with static NUL-terminated args.
            unsafe {
                let path = b"/bin/true\0";
                let argv = [path.as_ptr(), std::ptr::null()];
                libc::syscall(
                    libc::SYS_execve,
                    path.as_ptr(),
                    argv.as_ptr(),
                    std::ptr::null::<*const u8>(),
                );
                libc::_exit(NOT_KILLED);
            }
        }
        assert_killed_by_sigsys(run_under(Model::Denylist, body));
    }

    #[test]
    #[ignore = "needs bare-metal kernel honouring seccomp KILL_PROCESS"]
    fn ptrace_is_killed_by_denylist() {
        fn body() -> ! {
            // `ptrace` (cross-process inspection/control) is killed.
            // SAFETY: raw syscall; PTRACE_TRACEME takes no pointer args.
            unsafe {
                libc::syscall(libc::SYS_ptrace, libc::PTRACE_TRACEME, 0, 0, 0);
                libc::_exit(NOT_KILLED);
            }
        }
        assert_killed_by_sigsys(run_under(Model::Denylist, body));
    }

    #[test]
    #[ignore = "needs bare-metal kernel honouring seccomp KILL_PROCESS"]
    fn mprotect_exec_is_killed_but_rw_is_allowed_under_denylist() {
        // PROT_EXEC set -> killed (W^X / JIT-spray guard).
        fn exec_body() -> ! {
            // SAFETY: mmap a page then flip it to RX; the filter kills on the
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
                libc::syscall(
                    libc::SYS_mprotect,
                    p as usize,
                    len,
                    (libc::PROT_READ | libc::PROT_EXEC) as usize,
                );
                libc::_exit(NOT_KILLED);
            }
        }
        assert_killed_by_sigsys(run_under(Model::Denylist, exec_body));

        // PROT_EXEC clear -> allowed (child exits 0, not SIGSYS).
        fn noexec_body() -> ! {
            // SAFETY: as above but the mprotect keeps PROT_EXEC clear.
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
                libc::_exit(if rc == 0 { ALLOWED_OK } else { 8 });
            }
        }
        match run_under(Model::Denylist, noexec_body) {
            WaitStatus::Exited(_, ALLOWED_OK) => {}
            other => panic!("PROT_EXEC-clear mprotect should be allowed, got {other:?}"),
        }
    }

    // ---- denylist: syscalls that MUST stay allowed -------------------------

    #[test]
    #[ignore = "needs bare-metal kernel honouring seccomp KILL_PROCESS"]
    fn openat_is_allowed_by_denylist_filesystem_is_landlocks_job() {
        fn body() -> ! {
            // openat is NOT killed by the denylist — the filesystem is confined by
            // Landlock, not seccomp. The open itself may fail (EACCES under
            // Landlock) but it must NOT be SIGSYS-killed. Open a path Landlock's
            // read-only allowlist permits so a clean run can succeed.
            // SAFETY: raw syscall with a static NUL-terminated path.
            unsafe {
                let path = b"/etc/ld.so.cache\0";
                let fd = libc::syscall(
                    libc::SYS_openat,
                    libc::AT_FDCWD,
                    path.as_ptr(),
                    libc::O_RDONLY,
                );
                if fd >= 0 {
                    libc::close(fd as libc::c_int);
                }
                libc::_exit(ALLOWED_OK);
            }
        }
        assert_not_killed(run_under(Model::Denylist, body));
    }

    #[test]
    #[ignore = "needs bare-metal kernel honouring seccomp KILL_PROCESS"]
    fn clone_thread_is_allowed_by_denylist() {
        fn body() -> ! {
            // clone/fork are ALLOWED (V8/tokio spawn threads). A forked child can
            // exec nothing (execve is killed) and inherits this same filter, so
            // allowing thread/process creation is safe. Probe the legacy fork path
            // via a raw clone with SIGCHLD; a returned pid (parent) or 0 (child)
            // both mean "not killed". The transient child just exits.
            // SAFETY: raw syscall, no args dereferenced; both sides _exit promptly.
            unsafe {
                #[cfg(target_arch = "x86_64")]
                let rc = libc::syscall(libc::SYS_clone, libc::SIGCHLD as usize, 0, 0, 0, 0);
                #[cfg(not(target_arch = "x86_64"))]
                let rc = libc::syscall(libc::SYS_clone, 0usize, 0, 0, 0, 0);
                if rc == 0 {
                    // Newly-created child: exit immediately.
                    libc::_exit(ALLOWED_OK);
                }
                libc::_exit(ALLOWED_OK);
            }
        }
        assert_not_killed(run_under(Model::Denylist, body));
    }

    // ---- strict allowlist model (opt-in) -----------------------------------

    #[test]
    #[ignore = "needs bare-metal kernel honouring seccomp KILL_PROCESS"]
    fn connect_is_killed_by_strict_allowlist() {
        fn body() -> ! {
            // SAFETY: raw syscall with a stack-local sockaddr; async-signal-safe.
            unsafe {
                let addr: libc::sockaddr_in = std::mem::zeroed();
                libc::syscall(
                    libc::SYS_connect,
                    3,
                    &addr as *const _ as usize,
                    std::mem::size_of::<libc::sockaddr_in>(),
                );
                libc::_exit(NOT_KILLED);
            }
        }
        assert_killed_by_sigsys(run_under(Model::StrictRuntime, body));
    }

    #[test]
    #[ignore = "needs bare-metal kernel honouring seccomp KILL_PROCESS"]
    fn openat_is_killed_by_strict_allowlist() {
        fn body() -> ! {
            // Under the strict default-deny allowlist, openat is NOT permitted at
            // runtime (unlike the denylist), so it hits the default KillProcess.
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
        assert_killed_by_sigsys(run_under(Model::StrictRuntime, body));
    }

    // ---- full-jail smoke ----------------------------------------------------

    /// Full-jail smoke test: spawn the real jailed child via re-exec and drive
    /// one Hydrate + Shutdown over IPC. Requires the binary to honour the
    /// `draco __jail` argv hook (only true from the `draco` binary, not the test
    /// harness), plus a kernel honouring seccomp, so it is doubly gated.
    #[test]
    #[ignore = "needs the `draco __jail` re-exec hook + kernel honouring seccomp KILL_PROCESS"]
    fn full_jail_roundtrip_smoke() {
        use crate::frame::{read_jail_frame, write_supervisor_frame};
        use crate::level::LEVEL_LOG_PREFIX;
        use draco_types::{JailToSupervisor, RuntimeOutcome, SupervisorToJail};

        let mut handle = spawn_jail().expect("spawn_jail");

        // Child announces Ready, then reports its achieved sandbox level as a
        // prefixed Log frame.
        let ready = read_jail_frame(handle.ipc()).expect("read Ready");
        assert!(matches!(ready.header, JailToSupervisor::Ready { .. }));
        let level = read_jail_frame(handle.ipc()).expect("read sandbox-level Log");
        match level.header {
            JailToSupervisor::Log { msg, .. } => {
                assert!(
                    msg.strip_prefix(LEVEL_LOG_PREFIX)
                        .is_some_and(|l| l.starts_with("hardened:")),
                    "expected a hardened sandbox level, got {msg:?}"
                );
            }
            other => panic!("expected a sandbox-level Log, got {other:?}"),
        }

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
