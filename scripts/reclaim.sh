#!/usr/bin/env bash
# reclaim.sh — free regenerable disk space, fast, with minimal output.
#
# Draco's dev/test/clippy `target/` tree and the cargo registry caches are the
# only large *regenerable* consumers in a dev checkout. This removes them and
# nothing else — source, git history, and credentials are untouched.
#
# It is written to survive a near-full disk: it prints almost nothing (so the
# command harness can still stage its own output at a few KB free) and clears
# the biggest hog first. Run it the moment `df` looks tight — do NOT wait for
# ENOSPC, because at 0 bytes free the shell itself can no longer run commands,
# and the whole session is then unrecoverable.
#
#   bash scripts/reclaim.sh
set -u

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"

# 1. The build tree — biggest, always regenerable. `rm -rf` (not `cargo clean`)
#    because it needs no working cargo and succeeds even when disk is critical.
rm -rf "$root/target" 2>/dev/null || true

# 2. Cargo download/build caches — re-fetched automatically on the next build.
rm -rf "$cargo_home/registry/cache" \
       "$cargo_home/registry/src" \
       "$cargo_home/git/checkouts" 2>/dev/null || true

df -Pk "$root" 2>/dev/null | awk 'NR==2 {printf "reclaim: %.1f GiB free on %s\n", $4/1048576, $6}'
