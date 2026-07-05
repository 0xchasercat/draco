# Verifying the Linux OS-hardening layer (optional)

**You do not need this to use Draco.** Tier 2 runs in **isolate mode** everywhere
— the V8 context has no host-capability bindings, so page JS can't touch the
network, filesystem, or processes (the same isolation Puppeteer/Playwright rely
on). That works out of the box on macOS and Linux with zero configuration.

On **Linux**, Draco *additionally* applies an OS-hardening layer automatically as
defense-in-depth against a hypothetical V8-engine exploit: a **seccomp-bpf
denylist** (kills breakout syscalls like `execve`, `socket`/`connect`, `ptrace`,
`mount`, `bpf`, executable `mprotect`), a **network-namespace** air-gap, and a
**Landlock** filesystem lockdown. The denylist is designed to need **no per-host
tuning** — it only kills syscalls V8 never legitimately uses, so it can't break
on a new kernel/libc. netns and Landlock are best-effort and silently skipped
when unavailable. The achieved level is reported in every result's `trace` as a
`runtime.sandbox` step (`hardened: …` vs `isolate: …`).

This guide is for the security-conscious operator who wants to **confirm** that
hardening actually engages and enforces on their kernel. A CI/container sandbox
often lacks the kernel features (≥ 5.13 + unprivileged user namespaces) to
exercise it, so the enforcement tests ship `#[ignore]`d — you run them on real
hardware.

---

## 1. Prerequisites

```sh
uname -r                                   # ≥ 5.13 for Landlock (≥ 5.19 = Landlock v2+)
sysctl user.max_user_namespaces            # netns air-gap: want a non-zero number
sysctl kernel.unprivileged_userns_clone 2>/dev/null             # Debian/Ubuntu: want 1
sysctl kernel.apparmor_restrict_unprivileged_userns 2>/dev/null # Ubuntu 24.04+: want 0
grep -o landlock /sys/kernel/security/lsm  # Landlock present?
```

None of these are required to *run* Draco — their absence just lowers the
reported `runtime.sandbox` level (e.g. `hardened: seccomp (no netns…; no
landlock…)` or, with `--no-jail`, `isolate`). The seccomp denylist itself only
needs a Linux kernel with seccomp (essentially all of them).

Build toolchain (wreq/BoringSSL + bindgen, deno_core/V8): `cmake`, a C/C++
compiler, `clang`/`libclang`, `perl`, `pkg-config`.

## 2. Build

```sh
git clone https://github.com/0xchasercat/draco && cd draco
cargo build --release
```

## 3. Run the enforcement tests

`#[ignore]`d so they only run where the kernel supports them. Single-threaded
(each forks a jailed child):

```sh
cargo test -p draco-jail -- --ignored --list          # what's in your checkout
cargo test -p draco-jail -- --ignored --test-threads=1 --nocapture
```

What they prove (names may evolve — trust `--list`):

| Test | Asserts |
|------|---------|
| `connect_is_killed_by_denylist` | `connect()` in the child is killed (SIGSYS) — network blocked by seccomp |
| `execve_is_killed_by_denylist` | `execve()` is killed — no new programs can be launched |
| `ptrace_is_killed_by_denylist` | `ptrace()` is killed |
| `mprotect_exec_is_killed_but_rw_is_allowed_under_denylist` | executable `mprotect` killed; RW mappings allowed (W^X, pairs with `--jitless`) |
| `openat_is_allowed_by_denylist_filesystem_is_landlocks_job` | `openat` is *not* seccomp-killed — the filesystem boundary is Landlock's job, not seccomp's |
| `clone_thread_is_allowed_by_denylist` | thread creation is allowed (a fork can't exec anything anyway) |
| `full_jail_roundtrip_smoke` | supervisor spawns the jailed child, completes fd-3 IPC, reads the `sandbox:` level, reaps cleanly |

**A pass** = all report `ok`. For the `*_is_killed` tests, the child being
`WIFSIGNALED` with `SIGSYS` (signal 31) *is* the passing condition, not a runner
crash.

## 4. Run a jailed Tier 2 extraction

```sh
./target/release/draco extract "https://<a-CSR-SPA>" --tier-max 2 --pretty
```

Check the `runtime.sandbox` trace step reports `hardened: …` and the run
succeeds. Because the default seccomp is a denylist, a jailed run should **just
work** — no syscall tuning. Compare with `--no-jail` (isolate mode) to separate a
hardening problem from a capture/ranking problem:

```sh
./target/release/draco extract "https://<same-site>" --tier-max 2 --no-jail --pretty
```

If the jailed run ever fails with `DracoError::Jail { kind: "namespace_setup" }`,
your kernel lacks unprivileged userns — netns is skipped and the level drops to
`hardened: seccomp+landlock (no netns…)`, which is still safe (seccomp already
blocks network syscalls). That is expected, not an error to fix.

## 5. Strict mode (`--strict-sandbox`) — only if you opt in

The **default** denylist needs no tuning. If you want maximum hardening you can
opt into the strict **default-deny allowlist**:

```sh
./target/release/draco extract "https://<site>" --tier-max 2 --strict-sandbox
```

A default-deny allowlist *can* be killed on a syscall your specific kernel + libc
+ V8 build uses that the allowlist didn't anticipate. If that happens, the
iterate loop:

1. Reproduce; the supervisor reports `DracoError::Jail { kind: "killed", .. }`.
2. Find the syscall: `sudo dmesg | tail -20 | grep -i seccomp` (look for
   `syscall=NNN`), or `strace -f … 2>&1 | tail`. Resolve `NNN` with
   `ausyscall <NNN>` or `scmp_sys_resolver -a x86_64 <NNN>`.
3. Add it to the **strict allowlist** in `crates/draco-jail/src/linux/seccomp.rs`
   (the strict path; the default denylist is separate). Never add `execve`,
   `socket`/`connect`, `open`/`openat`, or unconditional `mprotect`.
4. Rebuild, re-run, then re-run §3 to confirm you didn't widen past a forbidden
   syscall.

Please open a PR/issue with the syscalls your platform needed — that data helps
harden the strict allowlist across distros. (Most users never touch this; the
default denylist is the recommended path.)

## 6. Done

- §3 enforcement tests all `ok`.
- §4 jailed extraction returns `status: success`, `source_tier:
  runtime_interception`, and a `hardened: …` `runtime.sandbox` step.
- `dmesg` shows no unexpected `SECCOMP` kills during a successful default run.
