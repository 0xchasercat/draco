# Building Draco in a constrained sandbox

This is the runbook for building/testing Draco inside a memory- and
disk-constrained environment (CI containers, the ~4 GiB agent build sandbox). It
exists because that environment has bitten us the same way several times: a long
`cargo test`/`clippy` run fills the disk, and **all uncommitted work in the
session is lost**. Everything here is aimed at making that impossible.

## Why disk-full is catastrophic here (not just annoying)

The sandbox disk is ~32 GiB. A full `cargo test --workspace` **and** a
`cargo clippy --workspace --all-targets` build two overlapping sets of artifacts,
and the workspace links heavyweight native deps (V8/deno_core, oxc, happy-dom,
BoringSSL via `wreq`). With Cargo's default `debug = 2`, the debuginfo alone runs
to several GiB and accumulates across a multi-release session until `target/`
fills the disk.

The failure is not graceful. When the filesystem hits **0 bytes free, the
command harness can no longer stage its own output**, so *every* command —
including the `rm -rf target` that would fix it — fails with `ENOSPC` before it
can do anything. The session deadlocks at exactly the moment you need to recover,
and whatever wasn't committed is gone.

Two independent rules prevent this. Follow both.

## Rule 1 — Commit and push before any long build

A crash costs nothing if the work is already on GitHub. Before kicking off a
`cargo test`/`clippy` run on meaningful changes:

```sh
git add -A && git commit -m "wip: <what>"      # a WIP commit is fine
git push origin HEAD:refs/heads/<your-branch>  # get it off the box
```

This is the single highest-leverage habit: the disk can vanish and you reclone.

## Rule 2 — Gate through `scripts/gate.sh`, not raw cargo

```sh
bash scripts/gate.sh            # fmt + clippy + test, disk-guarded, single-job
bash scripts/gate.sh clippy     # or a subset: any of fmt | clippy | test
```

`gate.sh` does three things raw cargo does not:

1. **Refuses to start a build below a free-space floor** (default 10 GiB),
   auto-running `reclaim.sh` first. You get a *loud early abort with headroom*
   instead of a silent 0-byte deadlock. Tunable via `DRACO_GATE_FLOOR_GIB`.
2. **Pins `CARGO_BUILD_JOBS=1`.** BoringSSL's C++/FIPS compile OOM-kills on a
   ~4 GiB box under parallel codegen; single-job trades wall-time for surviving.
3. **Prints free space before each phase** so you can watch `target/` grow and
   stop early if something is off.

## In a pinch — `scripts/reclaim.sh`

```sh
bash scripts/reclaim.sh    # rm target/ + cargo registry caches, print free space
```

Run it **the moment `df -h` looks tight — do not wait for ENOSPC.** It clears
only regenerable artifacts (the `target/` tree, `~/.cargo/registry/{cache,src}`,
`~/.cargo/git/checkouts`); source, git history, and credentials are untouched.
It prints almost nothing so it can run when disk is nearly full.

## Footprint control (already in `Cargo.toml`)

`[profile.dev]`/`[profile.test]` compile with `debug = "line-tables-only"` and
dependencies with `debug = false`. That keeps readable panic/backtrace `file:line`
for our own crates while shedding the multi-GiB dependency DWARF that was the bulk
of `target/`. It roughly halves the dev/test tree and is the reason a guarded
build now comfortably fits. Reversible per-dev if you need full debugger symbols.

## Build toolchain (Fedora-family sandbox)

BoringSSL builds from source and bindgen needs libclang:

```sh
sudo dnf install -y gcc gcc-c++ make cmake ninja-build golang perl \
                    clang clang-libs llvm-libs
export LIBCLANG_PATH=/usr/lib64        # so bindgen finds libclang
```

`draco-runtime` and `draco-jail` do **not** depend on the BoringSSL-linked
`draco-net`, so they build and clippy without any of the above — useful for fast
iteration on the isolate/jail without paying the native-toolchain cost.

## TL;DR

- Push WIP before long builds. The box is disposable; your commits are not.
- Build with `bash scripts/gate.sh`, never bare `cargo test`/`clippy` here.
- `bash scripts/reclaim.sh` at the first sign of a tight disk, never at 0 bytes.
