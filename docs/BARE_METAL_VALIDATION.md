# Bare-metal validation — jailed Tier 2

Everything in Draco is validated in CI **except the jail's runtime enforcement**:
seccomp actually killing forbidden syscalls, the V8-under-seccomp allowlist, the
network-namespace air-gap, and the Landlock filesystem lockdown. Those need a
real Linux kernel with features that a container/CI sandbox usually lacks
(kernel ≥ 5.13 + unprivileged user namespaces), so they ship as `#[ignore]`d
tests plus the live jailed path. This guide is how you validate them on your own
hardware.

The un-jailed Tier 2 pipeline (fetch → V8 capture → rank → replay) is already
proven end-to-end in CI; here we prove the *cage* around it.

---

## 1. Prerequisites

```sh
# Kernel ≥ 5.13 (Landlock ABI v1 landed in 5.13). ≥ 5.19 is better (Landlock v2+).
uname -r

# Unprivileged user namespaces must be enabled (Draco creates a userns to get a
# netns without root). Any ONE of these indicates they're available:
sysctl user.max_user_namespaces          # want: a large non-zero number
# Debian/Ubuntu also gate them behind one of:
sysctl kernel.unprivileged_userns_clone 2>/dev/null            # want: 1
sysctl kernel.apparmor_restrict_unprivileged_userns 2>/dev/null # want: 0

# Landlock present (optional — the jail degrades with a warning if absent):
grep -o landlock /sys/kernel/security/lsm

# Build toolchain (wreq/BoringSSL + bindgen, deno_core/V8):
#   cmake, a C/C++ compiler, clang + libclang, perl, pkg-config
#   Debian/Ubuntu: apt install build-essential cmake clang libclang-dev perl pkg-config
#   Fedora:        dnf install gcc gcc-c++ cmake clang clang-devel llvm-devel perl pkgconf
```

If `user.max_user_namespaces` is `0` or the Debian/Ubuntu knobs block it, enable
them (as root):

```sh
# Debian/Ubuntu (older knob):
sudo sysctl -w kernel.unprivileged_userns_clone=1
# Ubuntu 24.04+ (AppArmor knob):
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
```

## 2. Build

```sh
git clone https://github.com/0xchasercat/draco && cd draco
cargo build --release            # full build (Tier 0/1/2 + jail); compiles V8 + BoringSSL
```

## 3. Run the red-team tests (seccomp / namespace enforcement)

These are `#[ignore]`d so they never run in a sandbox that can't support them.
Run them single-threaded (each forks a jailed child):

```sh
# See exactly which jailed tests exist in your checkout:
cargo test -p draco-jail -- --ignored --list

# Run them:
cargo test -p draco-jail -- --ignored --test-threads=1 --nocapture
```

What each proves (names may evolve — trust `--list`):

| Test | Asserts |
|------|---------|
| `connect_is_killed` | a jailed child calling `connect()` is killed (SIGSYS) — network syscalls denied |
| `open_etc_passwd_is_killed` | opening `/etc/passwd` is killed — filesystem denied |
| `fork_is_killed` | `fork`/`clone` is killed — no new processes/threads |
| `mprotect_with_exec_is_killed_but_without_exec_is_allowed` | `mprotect(PROT_EXEC)` is killed, but a non-exec `mprotect` is allowed — the W^X guard that pairs with `--jitless` |
| `full_jail_roundtrip_smoke` | supervisor spawns the jailed child, completes the fd-3 IPC handshake, and reaps it cleanly |

**A pass** = all report `ok`. A child killed by the filter shows up as the test
observing `WIFSIGNALED` with `SIGSYS` (signal 31), which is the *expected,
passing* condition for the "is_killed" tests — not a crash of the test runner.

## 4. Run a jailed Tier 2 extraction (V8 under seccomp)

This is the real thing: the isolate runs **inside** the sandbox.

```sh
# A client-rendered site whose data comes from a JSON API (Tiers 0/1 will miss,
# so it escalates to the jailed Tier 2 isolate):
./target/release/draco extract "https://<a-CSR-SPA>" --tier-max 2 --pretty
```

Expected on success:

```json
{
  "status": "success",
  "source_tier": "runtime_interception",
  "data": { "...": "the replayed JSON endpoint" },
  "trace": [ /* ... runtime.spawn / runtime.capture / runtime.rank / runtime.replay */ ]
}
```

Compare against the **un-jailed** path to isolate a jail problem from a
capture/ranking problem:

```sh
./target/release/draco extract "https://<same-site>" --tier-max 2 --no-jail --pretty
```

If `--no-jail` succeeds but the jailed run fails with a `DracoError::Jail`
(`kind: "killed"` / `"seccomp_install"` / `"namespace_setup"`), the problem is
the sandbox policy — almost always the **seccomp allowlist** (next section), not
the extraction logic.

## 5. The seccomp allowlist iterate loop (expected)

The V8 syscall allowlist in `crates/draco-jail/src/linux/seccomp.rs` was built
from knowledge, not measured against *your* kernel + libc + V8 build. It is
normal for the first jailed run on a new host to be killed on a syscall the
allowlist didn't anticipate. The loop:

1. Run a jailed extraction (§4). If the child is killed, the supervisor reports a
   `DracoError::Jail { kind: "killed", .. }`.
2. Find the offending syscall. Easiest is `dmesg` (the kernel logs the audit
   record for a `SECCOMP` kill), or `strace`:
   ```sh
   sudo dmesg | tail -20 | grep -i seccomp     # look for "syscall=NNN"
   # or, run the child path under strace to see the last syscall before the kill:
   strace -f -e trace=all ./target/release/draco extract "https://<site>" --tier-max 2 2>strace.log
   tail -30 strace.log
   ```
   Translate the syscall number with `ausyscall <NNN>` (from `audit`) or
   `scmp_sys_resolver -a x86_64 <NNN>`.
3. Add the syscall to the allowlist in `seccomp.rs` (the module docs there
   describe the groups). Only add what V8/tokio-time legitimately needs; keep the
   default action `KILL_PROCESS` and never add `execve`, `socket`/`connect`,
   `open`/`openat`, `fork`/`clone`, or an unconditional `mprotect` (the exec-bit
   match must stay).
4. Rebuild and re-run. Repeat until a clean jailed extraction succeeds, then
   re-run the red-team tests (§3) to confirm you didn't widen the filter past a
   forbidden syscall.

Please open a PR (or an issue) with the syscalls your platform needed — that
list is exactly the data required to harden the default allowlist across distros.

## 6. What "done" looks like

- §3 red-team tests all `ok` (forbidden syscalls killed, IPC round-trips).
- §4 jailed extraction returns `status: success`, `source_tier:
  runtime_interception` on a CSR site — with **no** `--no-jail`.
- `dmesg` shows no unexpected `SECCOMP` kills during a successful run.

At that point the jail's enforcement is validated on your kernel and Tier 2 is
fully trustworthy against hostile pages.
