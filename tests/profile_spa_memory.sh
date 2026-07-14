#!/usr/bin/env bash

set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
DRACO="$ROOT/target/release/draco"
TIME_BIN=/usr/bin/time
JQ_BIN=$(command -v jq || true)

if [[ ! -x "$DRACO" ]]; then
  echo "error: release binary not found: $DRACO" >&2
  echo "build it first with: cargo build -p draco-cli --release" >&2
  exit 1
fi
if [[ ! -x "$TIME_BIN" ]]; then
  echo "error: macOS /usr/bin/time is required" >&2
  exit 1
fi
if [[ -z "$JQ_BIN" ]]; then
  echo "error: jq is required to validate the JSON envelope" >&2
  exit 1
fi

TMP_ROOT=${TMPDIR:-/tmp}
WORK_DIR=$(mktemp -d "$TMP_ROOT/draco-spa-memory.XXXXXX")
trap 'rm -rf "$WORK_DIR"' EXIT

require_runtime_log() {
  local json_file=$1
  local needle=$2
  local label=$3

  if ! "$JQ_BIN" -e --arg needle "$needle" '
      any(.trace[]?;
        .action == "runtime.log" and
        ((.detail // "") | contains($needle)))
    ' "$json_file" >/dev/null; then
    echo "error: missing $label runtime log ($needle)" >&2
    return 1
  fi
}

run_site() {
  local name=$1
  local url=$2
  local min_markdown=$3
  shift 3

  local json_file="$WORK_DIR/$name.json"
  local time_file="$WORK_DIR/$name.time"
  local memory_file="$WORK_DIR/$name.memory"

  echo "== $name ($url) =="
  # Each invocation is a new draco process. stdout and time/diagnostics stay in
  # separate temporary files so neither a pipeline nor command substitution can
  # mask the scraper's exit status.
  if ! "$TIME_BIN" -l "$DRACO" scrape "$url" --json --runtime-log \
      >"$json_file" 2>"$time_file"; then
    echo "error: $name scrape failed" >&2
    sed 's/^/  /' "$time_file" >&2
    return 1
  fi

  if ! "$JQ_BIN" -e '.status == "success"' "$json_file" >/dev/null; then
    echo "error: $name did not return status=success" >&2
    return 1
  fi

  local markdown_len
  markdown_len=$("$JQ_BIN" -r '(.markdown // "") | length' "$json_file")
  if (( markdown_len <= min_markdown )); then
    echo "error: $name markdown length $markdown_len <= $min_markdown" >&2
    return 1
  fi

  local semantic_gate
  for semantic_gate in "$@"; do
    require_runtime_log "$json_file" "$semantic_gate" "$name semantic gate"
  done
  require_runtime_log "$json_file" "[raze.window] closed via quiesce" "$name quiesce"

  "$JQ_BIN" -r '
      .trace[]?
      | select(.action == "runtime.log")
      | (.detail // empty)
      | select(startswith("[raze.memory] "))
    ' "$json_file" >"$memory_file"

  local memory_count
  memory_count=$(awk 'END { print NR }' "$memory_file")
  if (( memory_count != 6 )); then
    echo "error: $name emitted $memory_count [raze.memory] lines, expected 6" >&2
    return 1
  fi

  local max_rss
  if ! max_rss=$(awk '
      /maximum resident set size/ { rss = $1; found = 1 }
      END { if (!found) exit 1; print rss }
    ' "$time_file"); then
    echo "error: $name timing output had no maximum resident set size" >&2
    return 1
  fi

  echo "markdown chars: $markdown_len"
  echo "maximum RSS bytes: $max_rss"
  echo "[raze.memory] phases:"
  sed 's/^/  /' "$memory_file"
  echo
}

run_site \
  thrill \
  https://thrill.com \
  40000 \
  "games-state.thrill.com/snapshots/" \
  "/api/v2/games/providers"

run_site \
  bluff \
  https://bluff.com \
  20000 \
  "/promotions"
