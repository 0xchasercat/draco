//! Two-phase seccomp-bpf filters (canonical spec §7).
//!
//! Both phases use a **default action of `KillProcess`**: any syscall not
//! explicitly allowed terminates the whole child with `SIGSYS`, which the
//! supervisor observes as [`draco_types::JailKind::Killed`].
//!
//! * **Phase 1 — bootstrap.** A superset installed while the child is still
//!   finishing setup (arming Landlock left residual needs, flushing the runtime).
//!   It allows everything the runtime filter allows *plus* the handful of
//!   syscalls used only during bring-up.
//! * **Phase 2 — runtime.** The tight filter the payload runs under. File-open
//!   (`open`/`openat`), socket-creation/`connect`, and process-creation
//!   (`clone`/`fork`/`vfork`/`execve`) syscalls are **absent**, so they hit the
//!   default `KillProcess`. `mprotect` is allowed **only when `PROT_EXEC` is
//!   clear**, blocking W^X violations / JIT-spray from the (future) V8 payload.
//!
//! The concrete syscall numbers come from `libc::SYS_*` for the target arch.
//! This scaffold targets the primary platform, `x86_64-unknown-linux-gnu`; the
//! allow-list must be reconciled against spec §7's canonical table and widened
//! for the deno_core/V8 payload in Slice 3 (e.g. `clone` for V8 background
//! threads, `epoll_*`, `eventfd2`, `futex` are already present).

use std::collections::BTreeMap;

use seccompiler::{
    apply_filter, BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition,
    SeccompFilter, SeccompRule, TargetArch,
};

/// Error type for filter construction/installation.
pub type SeccompError = Box<dyn std::error::Error + Send + Sync>;

/// The seccompiler target arch for the arch we are compiled for.
#[cfg(target_arch = "x86_64")]
const TARGET_ARCH: TargetArch = TargetArch::x86_64;
#[cfg(target_arch = "aarch64")]
const TARGET_ARCH: TargetArch = TargetArch::aarch64;

/// A syscall entry: number plus its matching rules (empty = match any args).
type Rules = Vec<SeccompRule>;

/// Widen a libc syscall number to the `i64` key type seccompiler expects.
///
/// `libc::SYS_*` are `c_long`, which *is* `i64` on our supported LP64 Linux
/// targets (x86_64, aarch64) — hence clippy sees the conversion as an identity —
/// but the explicit widening keeps the map keys correctly typed and would remain
/// correct on an ILP32 target where `c_long` is 32-bit.
#[allow(clippy::useless_conversion)]
fn nr(sys: libc::c_long) -> i64 {
    i64::from(sys)
}

/// Insert an unconditional allow (match on syscall number regardless of args).
fn allow_any(map: &mut BTreeMap<i64, Rules>, sys: libc::c_long) {
    map.insert(nr(sys), Vec::new());
}

/// Build the shared core of allowed syscalls used by *both* phases: the minimal
/// set the payload loop needs to speak IPC over fd 3, manage its own memory, and
/// exit cleanly.
fn core_allow() -> BTreeMap<i64, Rules> {
    let mut map: BTreeMap<i64, Rules> = BTreeMap::new();

    // --- IPC over fd 3 (stream socket read/write) ---
    allow_any(&mut map, libc::SYS_read);
    allow_any(&mut map, libc::SYS_write);
    allow_any(&mut map, libc::SYS_recvfrom);
    allow_any(&mut map, libc::SYS_sendto);
    allow_any(&mut map, libc::SYS_recvmsg);
    allow_any(&mut map, libc::SYS_sendmsg);
    // Blocking primitives a read/write loop may land on.
    allow_any(&mut map, libc::SYS_ppoll);
    #[cfg(target_arch = "x86_64")]
    allow_any(&mut map, libc::SYS_poll);
    allow_any(&mut map, libc::SYS_epoll_ctl);
    allow_any(&mut map, libc::SYS_epoll_pwait);
    #[cfg(target_arch = "x86_64")]
    allow_any(&mut map, libc::SYS_epoll_wait);
    allow_any(&mut map, libc::SYS_epoll_create1);

    // --- Memory management (no PROT_EXEC via mprotect — see below) ---
    allow_any(&mut map, libc::SYS_mmap);
    allow_any(&mut map, libc::SYS_munmap);
    allow_any(&mut map, libc::SYS_brk);
    allow_any(&mut map, libc::SYS_madvise);

    // --- Signals (needed for the SIGSYS handler and normal signal plumbing) ---
    allow_any(&mut map, libc::SYS_rt_sigaction);
    allow_any(&mut map, libc::SYS_rt_sigprocmask);
    allow_any(&mut map, libc::SYS_rt_sigreturn);

    // --- Scheduling / synchronization ---
    allow_any(&mut map, libc::SYS_futex);
    allow_any(&mut map, libc::SYS_sched_yield);
    allow_any(&mut map, libc::SYS_restart_syscall);
    allow_any(&mut map, libc::SYS_clock_nanosleep);
    #[cfg(target_arch = "x86_64")]
    allow_any(&mut map, libc::SYS_nanosleep);

    // --- Fd hygiene / small stat + entropy the runtime needs ---
    allow_any(&mut map, libc::SYS_close);
    allow_any(&mut map, libc::SYS_fstat);
    allow_any(&mut map, libc::SYS_lseek);
    allow_any(&mut map, libc::SYS_getrandom);

    // --- Termination ---
    allow_any(&mut map, libc::SYS_exit);
    allow_any(&mut map, libc::SYS_exit_group);

    map
}

/// Add the conditional `mprotect` rule: allow **only** when the `prot` argument
/// (arg index 2) has `PROT_EXEC` clear. A request that sets `PROT_EXEC` falls
/// through to the default `KillProcess` action, enforcing W^X.
fn add_mprotect_no_exec(map: &mut BTreeMap<i64, Rules>) -> Result<(), SeccompError> {
    // (arg2 & PROT_EXEC) == 0  <=>  MaskedEq(PROT_EXEC) against value 0.
    let cond = SeccompCondition::new(
        2,
        SeccompCmpArgLen::Qword,
        SeccompCmpOp::MaskedEq(libc::PROT_EXEC as u64),
        0,
    )?;
    let rule = SeccompRule::new(vec![cond])?;
    map.insert(nr(libc::SYS_mprotect), vec![rule]);
    Ok(())
}

/// Compile a rule map into an installable BPF program with default `KillProcess`.
fn compile(rules: BTreeMap<i64, Rules>) -> Result<BpfProgram, SeccompError> {
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::KillProcess, // default (mismatch) action
        SeccompAction::Allow,       // action for matched rules
        TARGET_ARCH,
    )?;
    Ok(filter.try_into()?)
}

/// Build (but do not install) the phase-1 bootstrap program.
fn bootstrap_program() -> Result<BpfProgram, SeccompError> {
    let mut map = core_allow();
    add_mprotect_no_exec(&mut map)?;

    // Setup-only extras. During bring-up the runtime may still touch these; they
    // are deliberately *excluded* from the phase-2 runtime filter.
    //
    // `openat` here is for reading a (future) V8 snapshot / config during boot;
    // it is removed in phase 2 so the armed payload cannot open files. On
    // x86_64 the legacy `open` also exists; allow it during bootstrap only.
    allow_any(&mut map, libc::SYS_openat);
    #[cfg(target_arch = "x86_64")]
    allow_any(&mut map, libc::SYS_open);
    // prctl is needed by the seccomp/no_new_privs machinery itself.
    allow_any(&mut map, libc::SYS_prctl);
    // seccomp(2) — installing the phase-2 filter requires calling it while
    // phase-1 is active.
    allow_any(&mut map, libc::SYS_seccomp);

    compile(map)
}

/// Build (but do not install) the phase-2 runtime program.
fn runtime_program() -> Result<BpfProgram, SeccompError> {
    let mut map = core_allow();
    add_mprotect_no_exec(&mut map)?;
    // Note: intentionally NO open/openat, NO socket/connect, NO clone/fork/
    // vfork/execve, NO prctl/seccomp. Those hit the default KillProcess.
    compile(map)
}

/// Install the phase-1 bootstrap filter on the calling thread.
pub fn install_bootstrap_filter() -> Result<(), SeccompError> {
    let prog = bootstrap_program()?;
    apply_filter(&prog)?;
    Ok(())
}

/// Install the phase-2 runtime filter on the calling thread.
///
/// seccomp filters stack: this narrows the effective policy to the intersection
/// of phase-1 and phase-2, so syscalls dropped here become `KillProcess` even
/// though phase-1 allowed them.
pub fn install_runtime_filter() -> Result<(), SeccompError> {
    let prog = runtime_program()?;
    apply_filter(&prog)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // These compile-and-build tests run anywhere (no syscall installed): they
    // exercise the seccompiler frontend so a malformed rule map is caught in CI
    // even where we cannot actually arm the filter.

    #[test]
    fn bootstrap_program_compiles_nonempty() {
        let prog = bootstrap_program().expect("bootstrap filter compiles");
        assert!(
            !prog.is_empty(),
            "compiled BPF program must have instructions"
        );
    }

    #[test]
    fn runtime_program_compiles_nonempty() {
        let prog = runtime_program().expect("runtime filter compiles");
        assert!(!prog.is_empty());
    }

    #[test]
    fn runtime_is_a_subset_of_bootstrap() {
        // Every syscall number allowed at runtime must also be allowed during
        // bootstrap (bootstrap is a superset), and bootstrap must additionally
        // allow the setup-only syscalls.
        let boot = {
            let mut m = core_allow();
            add_mprotect_no_exec(&mut m).unwrap();
            allow_any(&mut m, libc::SYS_openat);
            #[cfg(target_arch = "x86_64")]
            allow_any(&mut m, libc::SYS_open);
            allow_any(&mut m, libc::SYS_prctl);
            allow_any(&mut m, libc::SYS_seccomp);
            m
        };
        let run = {
            let mut m = core_allow();
            add_mprotect_no_exec(&mut m).unwrap();
            m
        };
        for key in run.keys() {
            assert!(
                boot.contains_key(key),
                "runtime syscall {key} missing from bootstrap"
            );
        }
        assert!(boot.contains_key(&nr(libc::SYS_openat)));
        assert!(!run.contains_key(&nr(libc::SYS_openat)));
    }

    #[test]
    fn dangerous_syscalls_are_never_allowed_at_runtime() {
        let mut m = core_allow();
        add_mprotect_no_exec(&mut m).unwrap();
        // These must fall through to the default KillProcess.
        assert!(!m.contains_key(&nr(libc::SYS_connect)));
        assert!(!m.contains_key(&nr(libc::SYS_socket)));
        #[cfg(target_arch = "x86_64")]
        {
            assert!(!m.contains_key(&nr(libc::SYS_open)));
            assert!(!m.contains_key(&nr(libc::SYS_fork)));
            assert!(!m.contains_key(&nr(libc::SYS_vfork)));
        }
        assert!(!m.contains_key(&nr(libc::SYS_openat)));
        assert!(!m.contains_key(&nr(libc::SYS_clone)));
        assert!(!m.contains_key(&nr(libc::SYS_execve)));
    }

    #[test]
    fn mprotect_rule_is_conditional_not_unconditional() {
        let mut m = core_allow();
        add_mprotect_no_exec(&mut m).unwrap();
        let rules = m.get(&nr(libc::SYS_mprotect)).expect("mprotect present");
        assert!(
            !rules.is_empty(),
            "mprotect must be conditional (PROT_EXEC clear), not an unconditional allow"
        );
    }
}
