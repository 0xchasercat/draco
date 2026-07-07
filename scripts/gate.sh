#!/usr/bin/env bash
# gate.sh — run Draco's CI gates inside the memory-/disk-constrained sandbox
# WITHOUT the recurring disk-full session loss. See docs/SANDBOX.md.
#
#   bash scripts/gate.sh                 # fmt + clippy + test (the full gate)
#   bash scripts/gate.sh fmt             # a subset: any of fmt|clippy|test
#   bash scripts/gate.sh clippy test
#
# What it does that raw cargo does not:
#   * Refuses to START a build unless there is real headroom, auto-reclaiming
#     first — so you get a LOUD early abort with room to act, never the silent
#     0-byte ENOSPC deadlock where even `rm` can no longer run and the session
#     is lost.
#   * Pins CARGO_BUILD_JOBS=1 — BoringSSL's C++/FIPS compile OOM-kills on this
#     ~4 GiB box under parallel codegen.
#   * Prints free space before every phase so target/ growth is visible.
#
# Tunables (env): DRACO_GATE_FLOOR_GIB (default 10), DRACO_GATE_HARD_MIN_GIB (4).
set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

# Free space (GiB) we want before daring to start a cargo build. A from-clean
# `cargo test --workspace` grows target/ by several GiB even with slim debug;
# this floor leaves enough headroom that a build which starts can finish.
FLOOR_GIB="${DRACO_GATE_FLOOR_GIB:-10}"
# Below this, after a reclaim, we refuse to build at all rather than gamble.
HARD_MIN_GIB="${DRACO_GATE_HARD_MIN_GIB:-4}"

# Always emits a number; falls back to 9999 if df is unavailable (fail-open on
# the reading, never crash the gate on a df quirk).
free_gib() {
  df -Pk "$root" 2>/dev/null | awk 'NR==2 {printf "%.1f", $4/1048576; f=1} END{if(!f) print "9999.0"}'
}
lt() { awk "BEGIN{exit !($1 < $2)}"; }  # lt A B  → true when A < B
say() { printf '\n\033[1m== %s\033[0m  (%s GiB free)\n' "$1" "$(free_gib)"; }

guard() {
  local f; f="$(free_gib)"
  if lt "$f" "$FLOOR_GIB"; then
    printf 'gate: only %s GiB free (< %s floor) — reclaiming first…\n' "$f" "$FLOOR_GIB"
    bash "$root/scripts/reclaim.sh" || true
    f="$(free_gib)"
  fi
  if lt "$f" "$HARD_MIN_GIB"; then
    printf 'gate: STILL only %s GiB free after reclaim (< %s hard min).\n' "$f" "$HARD_MIN_GIB" >&2
    printf 'gate: refusing to build. Free space by hand (stray checkouts? other caches?) and retry.\n' >&2
    exit 1
  fi
}

phases=("$@")
[ "${#phases[@]}" -eq 0 ] && phases=(fmt clippy test)

export CARGO_BUILD_JOBS=1

guard
for p in "${phases[@]}"; do
  case "$p" in
    fmt)    say "cargo fmt --check";      cargo fmt --all -- --check ;;
    clippy) guard; say "cargo clippy -D warnings"; cargo clippy --workspace --all-targets -- -D warnings ;;
    test)   guard; say "cargo test --workspace";   cargo test --workspace ;;
    *) printf 'gate: unknown phase "%s" (want fmt|clippy|test)\n' "$p" >&2; exit 2 ;;
  esac
done
say "gate complete — all phases green"
