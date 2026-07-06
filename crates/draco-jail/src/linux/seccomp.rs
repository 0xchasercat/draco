//! seccomp-bpf filters for the jailed Tier 2 child (canonical spec §7).
//!
//! Two filter *models* are provided; the child picks one at arm time:
//!
//! * **Denylist (default).** The default action is **`Allow`**; only a curated
//!   set of unambiguously-dangerous breakout syscalls is **`KillProcess`**ed
//!   (see [`denylist_runtime_program`]). V8 and tokio never legitimately call
//!   any of them, so this filter is **robust across kernels, libc versions, and
//!   V8 releases** — a new host or a new V8 can add syscalls without breaking
//!   it, and it needs **zero per-host tuning**. This is the real, cross-platform
//!   default containment layer for Tier 2 on Linux.
//! * **Strict allowlist (opt-in).** The historical maximalist filter: default
//!   action `KillProcess`, allowing only the exact syscall set that a jitless V8
//!   heap and a current-thread tokio time driver need (see
//!   [`strict_runtime_program`]). Selected by `Config::strict_sandbox` /
//!   `--strict-sandbox`. It is the tightest possible policy but, being
//!   default-deny, **may need per-host tuning** (a new kernel/libc/V8 can
//!   introduce a syscall it forgets) — the bare-metal iterate loop documented on
//!   [`strict_core_allow`].
//!
//! ## Why the denylist is safe without namespaces
//!
//! The denylist `KILL_PROCESS`es `socket`/`connect`/`bind`/… , so the child
//! **cannot create a socket or reach the network at all** — the network air-gap
//! now rests on the filter itself, not on the network namespace. It also kills
//! `execve`/`execveat` (no new program image), `ptrace`/`process_vm_*` (no
//! cross-process memory access), the module/kexec/`bpf`/keyring/`reboot` families,
//! namespace-escape calls (`setns`/`unshare`/`pivot_root`/`chroot`/`mount`), and
//! `mprotect`/`pkey_mprotect` **when `PROT_EXEC` is set** (W^X / JIT-spray guard,
//! safe under V8 `--jitless`). It deliberately does **not** touch `clone`/`fork`/
//! `clone3` (V8/tokio/libc may spawn threads; a cloned task can exec nothing
//! because `execve` is killed and inherits this same filter) or `open`/`openat`
//! (the filesystem is covered by Landlock, not seccomp — killing file-opens is
//! exactly the per-host-tuning fragility we are removing). The inherited fd-3 IPC
//! uses `read`/`write` on an
//! **already-created** socketpair, so killing `socket`/`connect` does not affect
//! it (the child never calls `socket()` itself).
//!
//! The concrete syscall numbers come from `libc::SYS_*` for the target arch
//! (`x86_64`/`aarch64`). Some syscalls exist only on one ABI (e.g. `open`,
//! `fork`, `mknod` on x86_64) and are added under `cfg` where relevant.
//!
//! Runtime enforcement of `KILL_PROCESS` needs a kernel that honours it and can
//! only be exercised on bare metal — the red-team tests are `#[ignore]`d (the CI
//! kernel is 5.10 with no unprivileged userns; Landlock needs ≥ 5.13). The
//! denylist needs no iteration; the strict allowlist does (see its docs).

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

/// Insert an unconditional rule (match on syscall number regardless of args).
///
/// In an allowlist filter (matched action = `Allow`) this permits the syscall; in
/// a denylist filter (matched action = `KillProcess`) it kills it.
fn rule_any(map: &mut BTreeMap<i64, Rules>, sys: libc::c_long) {
    map.insert(nr(sys), Vec::new());
}

// ===========================================================================
// Denylist model (default) — default Allow, KILL a curated breakout set.
// ===========================================================================

/// Add the conditional `PROT_EXEC` kill rule for a memory-protection syscall:
/// **kill only when** the `prot` argument (arg index 2) has `PROT_EXEC` set. A
/// request with `PROT_EXEC` clear falls through to the filter's default `Allow`,
/// so ordinary RW mappings work while an attempt to make memory executable
/// (JIT-spray / W^X violation) is killed. Safe under V8 `--jitless`, which never
/// needs an executable mapping.
fn kill_prot_exec(map: &mut BTreeMap<i64, Rules>, sys: libc::c_long) -> Result<(), SeccompError> {
    // (arg2 & PROT_EXEC) != 0  <=>  MaskedEq(PROT_EXEC) against value PROT_EXEC.
    let cond = SeccompCondition::new(
        2,
        SeccompCmpArgLen::Qword,
        SeccompCmpOp::MaskedEq(libc::PROT_EXEC as u64),
        libc::PROT_EXEC as u64,
    )?;
    let rule = SeccompRule::new(vec![cond])?;
    map.insert(nr(sys), vec![rule]);
    Ok(())
}

/// Build the denylist kill-map: the curated set of breakout syscalls that are
/// `KILL_PROCESS`ed while everything else is allowed. See the module docs for the
/// rationale of each group and for what is deliberately **not** here
/// (`clone`/`fork`, `open`/`openat`).
///
/// `mprotect`/`pkey_mprotect` are added by [`kill_prot_exec`] (conditional on
/// `PROT_EXEC`), not here, so RW mappings still work.
fn denylist_map() -> Result<BTreeMap<i64, Rules>, SeccompError> {
    let mut map: BTreeMap<i64, Rules> = BTreeMap::new();

    // --- New program image: no exec, ever (a forked child can spawn nothing). ---
    rule_any(&mut map, libc::SYS_execve);
    rule_any(&mut map, libc::SYS_execveat);

    // --- Cross-process inspection / control. ---
    rule_any(&mut map, libc::SYS_ptrace);
    rule_any(&mut map, libc::SYS_process_vm_readv);
    rule_any(&mut map, libc::SYS_process_vm_writev);

    // --- Sockets / network: killing socket+connect is the network air-gap. ---
    rule_any(&mut map, libc::SYS_socket);
    rule_any(&mut map, libc::SYS_connect);
    rule_any(&mut map, libc::SYS_bind);
    rule_any(&mut map, libc::SYS_listen);
    rule_any(&mut map, libc::SYS_accept);
    rule_any(&mut map, libc::SYS_accept4);

    // --- Mount / namespace escape. ---
    rule_any(&mut map, libc::SYS_mount);
    rule_any(&mut map, libc::SYS_umount2);
    rule_any(&mut map, libc::SYS_pivot_root);
    rule_any(&mut map, libc::SYS_chroot);
    rule_any(&mut map, libc::SYS_setns);
    rule_any(&mut map, libc::SYS_unshare);
    // Do not kill `clone3`: modern libc/pthread implementations may try clone3
    // first for ordinary thread creation and fall back only if it returns ENOSYS.
    // The denylist already kills execve/execveat, so a cloned task cannot turn
    // into a new program image; strict mode remains the opt-in profile for a
    // tighter, per-host-tuned process/thread surface.

    // --- Kernel image / module / eBPF / keyring / power. ---
    rule_any(&mut map, libc::SYS_kexec_load);
    rule_any(&mut map, libc::SYS_kexec_file_load);
    rule_any(&mut map, libc::SYS_init_module);
    rule_any(&mut map, libc::SYS_finit_module);
    rule_any(&mut map, libc::SYS_delete_module);
    rule_any(&mut map, libc::SYS_bpf);
    rule_any(&mut map, libc::SYS_add_key);
    rule_any(&mut map, libc::SYS_keyctl);
    rule_any(&mut map, libc::SYS_request_key);
    rule_any(&mut map, libc::SYS_reboot);
    rule_any(&mut map, libc::SYS_swapon);
    rule_any(&mut map, libc::SYS_swapoff);

    // --- Clock / time-of-day tampering. ---
    rule_any(&mut map, libc::SYS_settimeofday);
    rule_any(&mut map, libc::SYS_clock_settime);

    // --- Device-node creation. `mknod` exists only on x86_64; `mknodat` on both. ---
    #[cfg(target_arch = "x86_64")]
    rule_any(&mut map, libc::SYS_mknod);
    rule_any(&mut map, libc::SYS_mknodat);

    // --- Real/effective/saved uid+gid changes (privilege manipulation). ---
    rule_any(&mut map, libc::SYS_setuid);
    rule_any(&mut map, libc::SYS_setgid);
    rule_any(&mut map, libc::SYS_setreuid);
    rule_any(&mut map, libc::SYS_setregid);
    rule_any(&mut map, libc::SYS_setresuid);
    rule_any(&mut map, libc::SYS_setresgid);

    // --- W^X: kill mprotect/pkey_mprotect only when PROT_EXEC is requested. ---
    kill_prot_exec(&mut map, libc::SYS_mprotect)?;
    kill_prot_exec(&mut map, libc::SYS_pkey_mprotect)?;

    Ok(map)
}

/// Compile a denylist rule map into an installable BPF program: default `Allow`,
/// matched rules `KillProcess`.
fn compile_denylist(rules: BTreeMap<i64, Rules>) -> Result<BpfProgram, SeccompError> {
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,       // default (mismatch) action
        SeccompAction::KillProcess, // action for matched (dangerous) rules
        TARGET_ARCH,
    )?;
    Ok(filter.try_into()?)
}

/// Build (but do not install) the **denylist** runtime program.
pub fn denylist_runtime_program() -> Result<BpfProgram, SeccompError> {
    compile_denylist(denylist_map()?)
}

/// Install the denylist filter on the calling thread. This is the default Tier 2
/// containment layer. A single filter suffices (no bootstrap/runtime split is
/// needed: setup syscalls like `openat`/`prctl`/`seccomp` are simply allowed).
pub fn install_denylist_filter() -> Result<(), SeccompError> {
    apply_filter(&denylist_runtime_program()?)?;
    Ok(())
}

// ===========================================================================
// Strict allowlist model (opt-in via --strict-sandbox) — default Kill.
//
// The historical maximalist default-deny filter, preserved verbatim in spirit.
// Tightest possible policy; being default-deny it MUST be validated on bare metal
// and MAY need per-host tuning (the iterate loop below).
// ===========================================================================

/// Insert an unconditional allow (match on syscall number regardless of args).
fn allow_any(map: &mut BTreeMap<i64, Rules>, sys: libc::c_long) {
    rule_any(map, sys);
}

/// Build the shared core of *allowed* syscalls used by both strict phases:
/// everything a jitless, single-threaded V8 isolate + a current-thread tokio time
/// driver need to speak IPC over fd 3, manage the V8 heap, keep time, and exit.
///
/// ## Bare-metal validation (strict mode only)
///
/// Being default-deny, this list is derived from knowledge of V8/tokio behaviour
/// and must be validated on bare-metal Linux: run the child under this filter,
/// observe any `SIGSYS` (`dmesg`/`SECCOMP` audit shows the offending syscall nr),
/// add exactly that syscall, and repeat until a real page hydrates cleanly. The
/// deliberately-omitted breakout syscalls (`socket`/`connect`, `execve`,
/// `ptrace`, `mprotect` with `PROT_EXEC`, …) must stay omitted. The **denylist**
/// default needs none of this — prefer it unless you specifically want the
/// tightest surface.
fn strict_core_allow() -> BTreeMap<i64, Rules> {
    let mut map: BTreeMap<i64, Rules> = BTreeMap::new();

    // --- IPC over the inherited fd 3 (stream socket read/write) ---
    allow_any(&mut map, libc::SYS_read);
    allow_any(&mut map, libc::SYS_write);
    allow_any(&mut map, libc::SYS_recvfrom);
    allow_any(&mut map, libc::SYS_sendto);
    allow_any(&mut map, libc::SYS_recvmsg);
    allow_any(&mut map, libc::SYS_sendmsg);
    // Blocking primitives a read/write loop (and tokio's reactor) may land on.
    allow_any(&mut map, libc::SYS_ppoll);
    #[cfg(target_arch = "x86_64")]
    allow_any(&mut map, libc::SYS_poll);
    // tokio's current-thread runtime still stands up an epoll fd + an unpark
    // eventfd even with only the time driver enabled; permit the epoll surface
    // and eventfd2 so the reactor can be created and parked/unparked.
    allow_any(&mut map, libc::SYS_epoll_ctl);
    allow_any(&mut map, libc::SYS_epoll_pwait);
    #[cfg(target_arch = "x86_64")]
    allow_any(&mut map, libc::SYS_epoll_wait);
    allow_any(&mut map, libc::SYS_epoll_create1);
    allow_any(&mut map, libc::SYS_eventfd2);

    // --- Memory management (jitless V8 heap + GC). NO PROT_EXEC via mprotect
    //     (added separately). `mremap` for heap resize; `madvise` for GC reclaim.
    allow_any(&mut map, libc::SYS_mmap);
    allow_any(&mut map, libc::SYS_munmap);
    allow_any(&mut map, libc::SYS_mremap);
    allow_any(&mut map, libc::SYS_brk);
    allow_any(&mut map, libc::SYS_madvise);

    // --- Signals. V8 installs a SIGSEGV/SIGBUS trap handler on an alternate
    //     signal stack even when jitless; `sigaltstack` is required for that.
    allow_any(&mut map, libc::SYS_rt_sigaction);
    allow_any(&mut map, libc::SYS_rt_sigprocmask);
    allow_any(&mut map, libc::SYS_rt_sigreturn);
    allow_any(&mut map, libc::SYS_sigaltstack);

    // --- Scheduling / synchronization ---
    allow_any(&mut map, libc::SYS_futex);
    allow_any(&mut map, libc::SYS_sched_yield);
    allow_any(&mut map, libc::SYS_sched_getaffinity); // CPU-count probe (V8/tokio)
    allow_any(&mut map, libc::SYS_membarrier);
    // glibc restartable sequences: registered at thread start and may be
    // re-checked; without it modern glibc can SIGSYS early.
    allow_any(&mut map, libc::SYS_rseq);
    allow_any(&mut map, libc::SYS_restart_syscall);

    // --- Time. `clock_gettime` is extremely hot (V8 + tokio); usually served by
    //     the vDSO but the syscall fallback must be allowed. Timers back the
    //     capture-window driver via `op_sleep`.
    allow_any(&mut map, libc::SYS_clock_gettime);
    allow_any(&mut map, libc::SYS_clock_getres);
    allow_any(&mut map, libc::SYS_clock_nanosleep);
    #[cfg(target_arch = "x86_64")]
    allow_any(&mut map, libc::SYS_nanosleep);

    // --- Fd hygiene / small stat + entropy the runtime needs ---
    allow_any(&mut map, libc::SYS_close);
    allow_any(&mut map, libc::SYS_fstat);
    allow_any(&mut map, libc::SYS_lseek);
    allow_any(&mut map, libc::SYS_getrandom); // V8 RNG seed + hash seed

    // --- Termination ---
    allow_any(&mut map, libc::SYS_exit);
    allow_any(&mut map, libc::SYS_exit_group);

    map
}

/// Add the conditional `mprotect` **allow** rule for strict mode: allow only when
/// the `prot` argument (arg index 2) has `PROT_EXEC` clear. A request that sets
/// `PROT_EXEC` falls through to the default `KillProcess`, enforcing W^X.
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

/// Compile an allowlist rule map into an installable BPF program: default
/// `KillProcess`, matched rules `Allow`.
fn compile_allowlist(rules: BTreeMap<i64, Rules>) -> Result<BpfProgram, SeccompError> {
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::KillProcess, // default (mismatch) action
        SeccompAction::Allow,       // action for matched rules
        TARGET_ARCH,
    )?;
    Ok(filter.try_into()?)
}

/// Build (but do not install) the strict phase-1 bootstrap program (superset:
/// core allow + the setup-only syscalls, dropped in phase 2).
fn strict_bootstrap_program() -> Result<BpfProgram, SeccompError> {
    let mut map = strict_core_allow();
    add_mprotect_no_exec(&mut map)?;

    // Setup-only extras, dropped in phase 2 so the armed payload cannot use them.
    allow_any(&mut map, libc::SYS_openat);
    #[cfg(target_arch = "x86_64")]
    allow_any(&mut map, libc::SYS_open);
    allow_any(&mut map, libc::SYS_prctl); // no_new_privs / seccomp machinery
    allow_any(&mut map, libc::SYS_seccomp); // installing the phase-2 filter

    compile_allowlist(map)
}

/// Build (but do not install) the strict phase-2 runtime program — the tight
/// filter the jailed payload runs under. NO open/openat, NO socket/connect, NO
/// clone/fork/execve, NO prctl/seccomp, NO ptrace; `mprotect` only with
/// `PROT_EXEC` clear.
pub fn strict_runtime_program() -> Result<BpfProgram, SeccompError> {
    let mut map = strict_core_allow();
    add_mprotect_no_exec(&mut map)?;
    compile_allowlist(map)
}

/// Install the strict phase-1 bootstrap filter on the calling thread.
pub fn install_strict_bootstrap_filter() -> Result<(), SeccompError> {
    apply_filter(&strict_bootstrap_program()?)?;
    Ok(())
}

/// Install the strict phase-2 runtime filter on the calling thread. Filters
/// stack, so this narrows the effective policy to the intersection of phase-1 and
/// phase-2.
pub fn install_strict_runtime_filter() -> Result<(), SeccompError> {
    apply_filter(&strict_runtime_program()?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // These compile-and-build tests run anywhere (no syscall installed): they
    // exercise the seccompiler frontend so a malformed rule map is caught in CI
    // even where we cannot actually arm the filter.

    // ---- denylist model -----------------------------------------------------

    #[test]
    fn denylist_program_compiles_nonempty() {
        let prog = denylist_runtime_program().expect("denylist filter compiles");
        assert!(
            !prog.is_empty(),
            "compiled BPF program must have instructions"
        );
    }

    #[test]
    fn denylist_kills_the_curated_breakout_set() {
        let map = denylist_map().expect("denylist map builds");
        // A representative slice of the kill-set must be present (→ KillProcess).
        for (name, sys) in [
            ("execve", libc::SYS_execve),
            ("execveat", libc::SYS_execveat),
            ("ptrace", libc::SYS_ptrace),
            ("process_vm_readv", libc::SYS_process_vm_readv),
            ("process_vm_writev", libc::SYS_process_vm_writev),
            ("socket", libc::SYS_socket),
            ("connect", libc::SYS_connect),
            ("bind", libc::SYS_bind),
            ("listen", libc::SYS_listen),
            ("accept", libc::SYS_accept),
            ("accept4", libc::SYS_accept4),
            ("mount", libc::SYS_mount),
            ("umount2", libc::SYS_umount2),
            ("pivot_root", libc::SYS_pivot_root),
            ("chroot", libc::SYS_chroot),
            ("setns", libc::SYS_setns),
            ("unshare", libc::SYS_unshare),
            ("kexec_load", libc::SYS_kexec_load),
            ("init_module", libc::SYS_init_module),
            ("finit_module", libc::SYS_finit_module),
            ("delete_module", libc::SYS_delete_module),
            ("bpf", libc::SYS_bpf),
            ("add_key", libc::SYS_add_key),
            ("keyctl", libc::SYS_keyctl),
            ("request_key", libc::SYS_request_key),
            ("reboot", libc::SYS_reboot),
            ("swapon", libc::SYS_swapon),
            ("swapoff", libc::SYS_swapoff),
            ("settimeofday", libc::SYS_settimeofday),
            ("clock_settime", libc::SYS_clock_settime),
            ("mknodat", libc::SYS_mknodat),
            ("setuid", libc::SYS_setuid),
            ("setgid", libc::SYS_setgid),
            ("setreuid", libc::SYS_setreuid),
            ("setregid", libc::SYS_setregid),
            ("setresuid", libc::SYS_setresuid),
            ("setresgid", libc::SYS_setresgid),
        ] {
            assert!(map.contains_key(&nr(sys)), "denylist must KILL `{name}`");
        }
    }

    #[test]
    fn denylist_does_not_kill_thread_or_file_syscalls() {
        // The syscalls whose killing caused the manual-iteration breakage MUST be
        // absent from the kill-map (→ default Allow). clone/fork spawn V8/tokio
        // threads; open/openat are Landlock's job.
        let map = denylist_map().expect("denylist map builds");
        assert!(
            !map.contains_key(&nr(libc::SYS_clone)),
            "clone must be allowed"
        );
        assert!(
            !map.contains_key(&nr(libc::SYS_openat)),
            "openat must be allowed"
        );
        #[cfg(target_arch = "x86_64")]
        {
            assert!(
                !map.contains_key(&nr(libc::SYS_fork)),
                "fork must be allowed"
            );
            assert!(
                !map.contains_key(&nr(libc::SYS_vfork)),
                "vfork must be allowed"
            );
            assert!(
                !map.contains_key(&nr(libc::SYS_open)),
                "open must be allowed"
            );
        }
        // read/write (the fd-3 IPC) must never be killed.
        assert!(
            !map.contains_key(&nr(libc::SYS_read)),
            "read must be allowed"
        );
        assert!(
            !map.contains_key(&nr(libc::SYS_write)),
            "write must be allowed"
        );
    }

    #[test]
    fn denylist_allows_clone3_for_libc_thread_creation() {
        // Modern libc/pthread may try clone3 for ordinary thread creation. The
        // default denylist must allow it; execve/execveat still prevent a cloned
        // task from becoming a new program image.
        let map = denylist_map().expect("denylist map builds");
        assert!(
            !map.contains_key(&nr(libc::SYS_clone3)),
            "default denylist must not kill clone3"
        );
    }

    #[test]
    fn denylist_mprotect_is_conditional_on_prot_exec() {
        // mprotect/pkey_mprotect appear in the kill-map but only via a rule that
        // matches PROT_EXEC — a plain RW mprotect falls through to Allow.
        let map = denylist_map().expect("denylist map builds");
        let m = map.get(&nr(libc::SYS_mprotect)).expect("mprotect present");
        assert!(
            !m.is_empty(),
            "mprotect kill must be conditional on PROT_EXEC, not unconditional"
        );
        let pk = map
            .get(&nr(libc::SYS_pkey_mprotect))
            .expect("pkey_mprotect present");
        assert!(
            !pk.is_empty(),
            "pkey_mprotect kill must be conditional on PROT_EXEC"
        );
    }

    // ---- strict allowlist model ---------------------------------------------

    #[test]
    fn strict_bootstrap_program_compiles_nonempty() {
        let prog = strict_bootstrap_program().expect("strict bootstrap filter compiles");
        assert!(
            !prog.is_empty(),
            "compiled BPF program must have instructions"
        );
    }

    #[test]
    fn strict_runtime_program_compiles_nonempty() {
        let prog = strict_runtime_program().expect("strict runtime filter compiles");
        assert!(!prog.is_empty());
    }

    #[test]
    fn strict_runtime_is_a_subset_of_bootstrap() {
        // Every syscall number allowed at runtime must also be allowed during
        // bootstrap (bootstrap is a superset), plus the setup-only syscalls.
        let boot = {
            let mut m = strict_core_allow();
            add_mprotect_no_exec(&mut m).unwrap();
            allow_any(&mut m, libc::SYS_openat);
            #[cfg(target_arch = "x86_64")]
            allow_any(&mut m, libc::SYS_open);
            allow_any(&mut m, libc::SYS_prctl);
            allow_any(&mut m, libc::SYS_seccomp);
            m
        };
        let run = {
            let mut m = strict_core_allow();
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
    fn strict_dangerous_syscalls_are_never_allowed_at_runtime() {
        let mut m = strict_core_allow();
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
    fn strict_mprotect_rule_is_conditional_not_unconditional() {
        let mut m = strict_core_allow();
        add_mprotect_no_exec(&mut m).unwrap();
        let rules = m.get(&nr(libc::SYS_mprotect)).expect("mprotect present");
        assert!(
            !rules.is_empty(),
            "mprotect must be conditional (PROT_EXEC clear), not an unconditional allow"
        );
    }

    #[test]
    fn strict_v8_and_tokio_syscalls_are_allowed_at_runtime() {
        // A jitless V8 heap + GC and a tokio time driver need these; if any
        // regress out of the allowlist the jailed isolate would SIGSYS on metal.
        let m = strict_core_allow();
        for (name, nr_val) in [
            ("mmap", libc::SYS_mmap),
            ("munmap", libc::SYS_munmap),
            ("mremap", libc::SYS_mremap),
            ("madvise", libc::SYS_madvise),
            ("brk", libc::SYS_brk),
            ("futex", libc::SYS_futex),
            ("sched_yield", libc::SYS_sched_yield),
            ("sched_getaffinity", libc::SYS_sched_getaffinity),
            ("rseq", libc::SYS_rseq),
            ("membarrier", libc::SYS_membarrier),
            ("sigaltstack", libc::SYS_sigaltstack),
            ("rt_sigaction", libc::SYS_rt_sigaction),
            ("rt_sigprocmask", libc::SYS_rt_sigprocmask),
            ("rt_sigreturn", libc::SYS_rt_sigreturn),
            ("clock_gettime", libc::SYS_clock_gettime),
            ("clock_getres", libc::SYS_clock_getres),
            ("clock_nanosleep", libc::SYS_clock_nanosleep),
            ("getrandom", libc::SYS_getrandom),
            ("read", libc::SYS_read),
            ("write", libc::SYS_write),
            ("close", libc::SYS_close),
            ("fstat", libc::SYS_fstat),
            ("ppoll", libc::SYS_ppoll),
            ("eventfd2", libc::SYS_eventfd2),
            ("exit", libc::SYS_exit),
            ("exit_group", libc::SYS_exit_group),
        ] {
            assert!(
                m.contains_key(&nr(nr_val)),
                "V8/tokio runtime syscall `{name}` missing from the strict allowlist"
            );
        }
    }

    #[test]
    fn strict_mprotect_is_present_but_not_in_the_plain_core_map() {
        // The plain core map must NOT unconditionally allow mprotect; only the
        // PROT_EXEC-clear conditional (added by add_mprotect_no_exec) may.
        let core = strict_core_allow();
        assert!(
            !core.contains_key(&nr(libc::SYS_mprotect)),
            "strict_core_allow must not contain an unconditional mprotect"
        );
    }
}
