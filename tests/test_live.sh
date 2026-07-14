#!/usr/bin/env bash
# =============================================================================
# Draco Live Test Suite — tests draco against real-world websites
#
# Each test runs `draco scrape <url> [flags]` and checks the output for expected
# content.  Pass/fail is reported per-test with timing.  Exit code is the number
# of failed tests (0 = all passed).
#
# Usage:
#   ./tests/test_live.sh                  # run all tests
#   ./tests/test_live.sh --quick          # run only the 10 core site tests
#   ./tests/test_live.sh --daemon         # run daemon REST API tests
#   ./tests/test_live.sh --plateau        # repeated local Tier-2 ownership/RSS plateau
#   ./tests/test_live.sh --features       # run only feature-flag tests
#   ./tests/test_live.sh --errors         # run only error-handling tests
#   ./tests/test_live.sh --filter md      # run tests whose name contains "md"
#   DRACO_BIN=./target/debug/draco ./tests/test_live.sh   # use debug build
#   DRACO_PORT=3003 ./tests/test_live.sh --daemon          # custom daemon port
# =============================================================================

DRACO="${DRACO_BIN:-./target/release/draco}"
PASS=0
FAIL=0
SKIP=0
FAILED_TESTS=()
TIMEOUT_SEC=30

# Guard: max time per scrape command (via GNU timeout or macOS gtimeout)
TIMEOUT_CMD=""
if command -v gtimeout &>/dev/null; then
  TIMEOUT_CMD="gtimeout $TIMEOUT_SEC"
elif command -v timeout &>/dev/null; then
  TIMEOUT_CMD="timeout $TIMEOUT_SEC"
fi

# Daemon config
DAEMON_PORT="${DRACO_PORT:-3002}"
DAEMON_PID=""
FIXTURE_PID=""

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

FILTER=""

# Parse args
RUN_ALL=true
RUN_SITE=false
RUN_FEATURE=false
RUN_ERROR=false
RUN_HARD=false
RUN_QUICK=false
RUN_DAEMON=false
RUN_PLATEAU=false

i=1
for arg in "$@"; do
  case "$arg" in
    --quick) RUN_QUICK=true; RUN_ALL=false ;;
    --daemon) RUN_DAEMON=true; RUN_ALL=false ;;
    --plateau) RUN_PLATEAU=true; RUN_ALL=false ;;
    --sites) RUN_SITE=true; RUN_ALL=false ;;
    --features) RUN_FEATURE=true; RUN_ALL=false ;;
    --errors) RUN_ERROR=true; RUN_ALL=false ;;
    --hard) RUN_HARD=true; RUN_ALL=false ;;
    --filter)
      next_idx=$((i + 1))
      if [[ $# -ge $next_idx ]]; then
        FILTER="${!next_idx}"
      fi
      ;;
  esac
  i=$((i + 1))
done

banner() {
  echo ""
  printf "${CYAN}══════════════════════════════════════════════════════════════${NC}\n"
  printf "${CYAN}  %s${NC}\n" "$1"
  printf "${CYAN}══════════════════════════════════════════════════════════════${NC}\n"
  echo ""
}

should_run() {
  local name="$1"
  if [[ -n "$FILTER" && "$name" != *"$FILTER"* ]]; then
    return 1
  fi
  return 0
}

scrape() {
  local url="$1"
  shift
  if [[ -n "$TIMEOUT_CMD" ]]; then
    $TIMEOUT_CMD "$DRACO" scrape "$url" "$@" 2>/dev/null
  else
    "$DRACO" scrape "$url" "$@" 2>/dev/null
  fi
}

scrape_exit() {
  local url="$1"
  shift
  if [[ -n "$TIMEOUT_CMD" ]]; then
    $TIMEOUT_CMD "$DRACO" scrape "$url" "$@" 2>&1
  else
    "$DRACO" scrape "$url" "$@" 2>&1
  fi
}

run_test() {
  local name="$1"
  local url="$2"
  local expected="$3"
  shift 3
  local flags=("$@")

  should_run "$name" || { printf "  ${YELLOW}⊟${NC} %s  (filtered out)\n" "$name"; return; }

  printf "  ${BOLD}▶${NC} %s\n" "$name"
  printf "    url: %s\n" "$url"
  [[ ${#flags[@]} -gt 0 ]] && printf "    flags: %s\n" "${flags[*]}"

  local start
  start=$(date +%s%N 2>/dev/null || python3 -c 'import time; print(time.time_ns())')
  local output
  local exit_code=0

  output=$(scrape "$url" "${flags[@]}")
  exit_code=$?

  local end
  end=$(date +%s%N 2>/dev/null || python3 -c 'import time; print(time.time_ns())')

  local elapsed
  if [[ "$start" =~ ^[0-9]+$ && "$end" =~ ^[0-9]+$ && ${#start} -gt 12 ]]; then
    elapsed="$(( (end - start) / 1000000 ))ms"
  else
    elapsed="?"
  fi

  local status
  if [[ "$exit_code" -eq 0 ]]; then
    if echo "$output" | grep -q "$expected"; then
      status="${GREEN}PASS${NC}"
      PASS=$((PASS + 1))
    else
      status="${RED}FAIL${NC}"
      printf "    ${RED}✗${NC} expected '%s' not found in output\n" "$expected"
      printf "    ${YELLOW}got (first 300 chars):${NC}\n"
      printf "      %s\n" "${output:0:300}"
      FAIL=$((FAIL + 1))
      FAILED_TESTS+=("$name")
    fi
  elif [[ "$exit_code" -eq 124 ]]; then
    status="${RED}TIMEOUT${NC}"
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("$name (timeout)")
  elif [[ "$exit_code" -eq 3 ]]; then
    status="${YELLOW}NEEDS_BROWSER${NC}"
    PASS=$((PASS + 1))
  else
    status="${RED}FAIL (exit $exit_code)${NC}"
    printf "    ${RED}✗${NC} exit code: $exit_code\n"
    printf "    ${YELLOW}output (first 300 chars):${NC}\n"
    printf "      %s\n" "${output:0:300}"
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("$name")
  fi

  printf "    ${status}  ${elapsed}\n"
  echo ""
}

run_test_json() {
  local name="$1"
  local url="$2"
  shift 2
  local flags=("$@")

  should_run "$name" || { printf "  ${YELLOW}⊟${NC} %s  (filtered out)\n" "$name"; return; }

  printf "  ${BOLD}▶${NC} %s (JSON validation)\n" "$name"
  printf "    url: %s\n" "$url"
  [[ ${#flags[@]} -gt 0 ]] && printf "    flags: %s\n" "${flags[*]}"

  local start
  start=$(date +%s%N 2>/dev/null || python3 -c 'import time; print(time.time_ns())')

  local raw_output
  local exit_code=0
  raw_output=$(scrape "$url" "${flags[@]}")
  exit_code=$?

  local end
  end=$(date +%s%N 2>/dev/null || python3 -c 'import time; print(time.time_ns())')

  local elapsed
  if [[ "$start" =~ ^[0-9]+$ && "$end" =~ ^[0-9]+$ && ${#start} -gt 12 ]]; then
    elapsed="$(( (end - start) / 1000000 ))ms"
  else
    elapsed="?"
  fi

  local status="${RED}FAIL${NC}"

  # Check if output is valid JSON — draco always returns JSON for --json or --format json
  if has_status=$(echo "$raw_output" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    s = d.get('status', '')
    print(s if s else 'no_status')
except Exception as e:
    print('invalid: ' + str(e))
" 2>/dev/null); then
    if [[ "$has_status" == "success" || "$has_status" == "unsupported" ]]; then
      status="${GREEN}PASS${NC}"
      PASS=$((PASS + 1))
    else
      status="${RED}FAIL${NC} (unexpected status: $has_status)"
      printf "    ${RED}✗${NC} JSON status: $has_status\n"
      echo "$raw_output" | python3 -m json.tool 2>/dev/null | head -6
      FAIL=$((FAIL + 1))
      FAILED_TESTS+=("$name")
    fi
  else
    status="${RED}FAIL${NC} (invalid JSON)"
    printf "    ${RED}✗${NC} output is not valid JSON\n"
    printf "    ${YELLOW}first 300 chars:${NC}\n"
    printf "      %s\n" "${raw_output:0:300}"
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("$name")
  fi

  printf "    ${status}  ${elapsed}\n"
  echo ""
}

run_test_error() {
  local name="$1"
  local url="$2"
  local expected_exit="$3"
  local expected_output="$4"
  shift 4
  local flags=("$@")

  should_run "$name" || { printf "  ${YELLOW}⊟${NC} %s  (filtered out)\n" "$name"; return; }

  printf "  ${BOLD}▶${NC} %s (expect error)\n" "$name"
  printf "    url: %s\n" "$url"

  local output
  local exit_code=0
  output=$(scrape_exit "$url" "${flags[@]}")
  exit_code=$?

  local status
  if [[ "$exit_code" -eq "$expected_exit" ]]; then
    if [[ -n "$expected_output" ]] && echo "$output" | grep -q "$expected_output"; then
      status="${GREEN}PASS${NC}"
      PASS=$((PASS + 1))
    elif [[ -z "$expected_output" ]]; then
      status="${GREEN}PASS${NC}"
      PASS=$((PASS + 1))
    else
      status="${RED}FAIL${NC} (expected message not found)"
      printf "    ${RED}✗${NC} expected message: '$expected_output'\n"
      FAIL=$((FAIL + 1))
      FAILED_TESTS+=("$name")
    fi
  else
    status="${RED}FAIL${NC} (expected exit $expected_exit, got $exit_code)"
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("$name")
  fi
  printf "    ${status}\n"
  echo ""
}

# ===========================================================================
#  DAEMON HELPERS
# ===========================================================================

start_daemon() {
  local port="${1:-$DAEMON_PORT}"
  printf "  ${BOLD}▶${NC} Starting draco serve on port %s...\n" "$port"
  "$DRACO" serve --port "$port" &
  DAEMON_PID=$!
  # Wait for readiness (up to 10s)
  local waited=0
  while [[ "$waited" -lt 10 ]]; do
    if curl -sf "http://127.0.0.1:$port/health" >/dev/null 2>&1; then
      printf "    ${GREEN}ready${NC} (PID %s)\n\n" "$DAEMON_PID"
      return 0
    fi
    sleep 0.5
    waited=$((waited + 1))
  done
  printf "    ${RED}daemon failed to start${NC}\n"
  return 1
}

stop_daemon() {
  if [[ -n "$DAEMON_PID" ]]; then
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
    DAEMON_PID=""
  fi
}

plateau_skip() {
  printf "    ${YELLOW}SKIP${NC}: %s\n" "$1"
  SKIP=$((SKIP + 1))
}

plateau_fail() {
  printf "    ${RED}FAIL${NC}: %s\n" "$1"
  FAIL=$((FAIL + 1))
  FAILED_TESTS+=("$2")
}

start_plateau_fixture() {
  local port="$1"
  python3 -m http.server "$port" --bind 127.0.0.1 --directory tests/fixtures \
    >/dev/null 2>&1 &
  FIXTURE_PID=$!
  local waited=0
  while [[ "$waited" -lt 20 ]]; do
    if curl -sf "http://127.0.0.1:$port/plateau_tier2.html" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  return 1
}

stop_plateau_fixture() {
  if [[ -n "$FIXTURE_PID" ]]; then
    kill "$FIXTURE_PID" 2>/dev/null || true
    wait "$FIXTURE_PID" 2>/dev/null || true
    FIXTURE_PID=""
  fi
}

cleanup_background_processes() {
  stop_daemon
  stop_plateau_fixture
}

trap cleanup_background_processes EXIT
trap 'cleanup_background_processes; exit 130' INT
trap 'cleanup_background_processes; exit 143' TERM

daemon_thread_count() {
  local count
  count=$(ps -o nlwp= -p "$DAEMON_PID" 2>/dev/null | tr -d ' ')
  if [[ "$count" =~ ^[0-9]+$ ]]; then
    echo "$count"
    return
  fi
  count=$(ps -M -p "$DAEMON_PID" 2>/dev/null | awk 'NR > 1 { n++ } END { print n + 0 }')
  echo "$count"
}

daemon_descendants() {
  local frontier="$DAEMON_PID"
  local descendants=""
  local parent children child
  while [[ -n "$frontier" ]]; do
    local next=""
    for parent in $frontier; do
      children=$(pgrep -P "$parent" 2>/dev/null || true)
      for child in $children; do
        descendants="$descendants $child"
        next="$next $child"
      done
    done
    frontier="$next"
  done
  echo "$descendants"
}

health_sample() {
  curl -sf "http://127.0.0.1:$DAEMON_PORT/health" | python3 -c '
import json, sys
h = json.load(sys.stdin)
jobs = h.get("jobs", {}).get("total", {})
cache = h.get("cache", {})
isolates = h.get("isolates", {})
sessions = h.get("sessions", {})
values = (
    h.get("availableSlots"),
    h.get("activeCaptures"),
    jobs.get("jobs"),
    jobs.get("running"),
    jobs.get("retainedBytes"),
    cache.get("entries"),
    cache.get("payloadBytes"),
    cache.get("keyBytes"),
    cache.get("capacity"),
    isolates.get("created"),
    isolates.get("dropped"),
    isolates.get("active"),
    sessions.get("active"),
)
if any(type(value) is not int or value < 0 for value in values):
    raise SystemExit(42)
print(*values)
'
}

plateau_request() {
  local fixture_url="$1"
  curl -sf -X POST "http://127.0.0.1:$DAEMON_PORT/v1/scrape" \
    -H 'content-type: application/json' \
    -d "{\"url\":\"$fixture_url\",\"formats\":[\"markdown\"],\"tierMax\":2,\"captureWindowMs\":250,\"noJail\":true,\"ignoreRobots\":true,\"runtimeLog\":true}" \
    | python3 -c '
import json, sys
body = json.load(sys.stdin)
markdown = body.get("data", {}).get("markdown", "")
source_tier = body.get("draco", {}).get("sourceTier")
if (body.get("success") is not True
        or source_tier != "runtime_interception"
        or "Tier 2 plateau stable content" not in markdown):
    raise SystemExit(1)
'
}

plateau_tests() {
  banner "DAEMON PLATEAU — LOCAL TIER-2 REPEATED REQUESTS"

  local missing=""
  for tool in curl python3 ps pgrep; do
    command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
  done
  if [[ -n "$missing" ]]; then
    plateau_skip "platform tooling unavailable:$missing"
    return 0
  fi
  if ! ps -o rss= -p $$ >/dev/null 2>&1; then
    plateau_skip "this ps implementation cannot report resident memory"
    return 0
  fi

  local requests="${DRACO_PLATEAU_REQUESTS:-200}"
  local warmups="${DRACO_PLATEAU_WARMUPS:-5}"
  local slope_limit="${DRACO_PLATEAU_MAX_RSS_SLOPE_KB:-64}"
  if [[ ! "$requests" =~ ^[0-9]+$ || ! "$warmups" =~ ^[0-9]+$ || "$requests" -lt 2 ]]; then
    plateau_fail "DRACO_PLATEAU_REQUESTS must be an integer >= 2 and WARMUPS must be non-negative" \
      "daemon plateau configuration"
    return 1
  fi

  local fixture_port="${DRACO_FIXTURE_PORT:-3012}"
  local fixture_url="http://127.0.0.1:$fixture_port/plateau_tier2.html"
  local work_dir
  work_dir=$(mktemp -d "${TMPDIR:-/tmp}/draco-plateau.XXXXXX") || {
    plateau_skip "mktemp is unavailable"
    return 0
  }
  local rss_file="$work_dir/rss-kb.txt"
  local samples_file="$work_dir/samples.csv"
  printf 'request,rss_kb,threads,descendants,available,active_captures,jobs,running_jobs,retained_bytes,cache_entries,cache_payload,cache_key_bytes,cache_capacity,isolates_created,isolates_dropped,isolates_active,sessions\n' >"$samples_file"

  if ! start_plateau_fixture "$fixture_port"; then
    plateau_fail "could not start the local Tier-2 fixture server" \
      "daemon plateau fixture startup"
    rm -rf "$work_dir"
    return 1
  fi
  if ! start_daemon "$DAEMON_PORT"; then
    stop_plateau_fixture
    rm -rf "$work_dir"
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("daemon plateau startup")
    return 1
  fi

  local run_kind="definitive"
  (( requests < 100 )) && run_kind="smoke"
  printf "  warmups: %s; measured requests: %s (%s)\n" "$warmups" "$requests" "$run_kind"
  local i
  for ((i = 1; i <= warmups; i++)); do
    if ! plateau_request "$fixture_url"; then
      FAIL=$((FAIL + 1))
      FAILED_TESTS+=("daemon plateau warmup $i")
      stop_daemon; stop_plateau_fixture; rm -rf "$work_dir"
      return 1
    fi
  done

  local baseline
  baseline=$(health_sample) || baseline=""
  if [[ -z "$baseline" ]]; then
    plateau_fail "required daemon ownership telemetry is missing or invalid" \
      "daemon plateau telemetry"
    stop_daemon
    stop_plateau_fixture
    rm -rf "$work_dir"
    return 1
  fi
  local base_available base_active base_jobs base_running base_retained
  local base_cache_entries base_cache_payload base_cache_key_bytes base_cache_capacity
  local base_isolates_created base_isolates_dropped base_isolates_active base_sessions
  read -r base_available base_active base_jobs base_running base_retained \
    base_cache_entries base_cache_payload base_cache_key_bytes base_cache_capacity \
    base_isolates_created base_isolates_dropped base_isolates_active base_sessions <<<"$baseline"
  local base_threads
  base_threads=$(daemon_thread_count)

  local failed=0
  local telemetry_missing=0
  for ((i = 1; i <= requests; i++)); do
    if ! plateau_request "$fixture_url"; then
      printf "    ${RED}request %s failed semantic validation${NC}\n" "$i"
      failed=1
      break
    fi
    local rss threads descendants health
    rss=$(ps -o rss= -p "$DAEMON_PID" 2>/dev/null | tr -d ' ')
    threads=$(daemon_thread_count)
    descendants=$(daemon_descendants)
    local descendant_count=0
    [[ -n "$descendants" ]] && descendant_count=$(wc -w <<<"$descendants" | tr -d ' ')
    health=$(health_sample) || health=""
    if [[ -z "$health" ]]; then
      telemetry_missing=1
      break
    fi
    local available active jobs running retained cache_entries cache_payload
    local cache_key_bytes cache_capacity isolates_created isolates_dropped isolates_active sessions
    read -r available active jobs running retained cache_entries cache_payload \
      cache_key_bytes cache_capacity isolates_created isolates_dropped isolates_active sessions <<<"$health"
    printf '%s\n' "$rss" >>"$rss_file"
    printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
      "$i" "$rss" "$threads" "$descendant_count" "$available" "$active" \
      "$jobs" "$running" "$retained" "$cache_entries" "$cache_payload" \
      "$cache_key_bytes" "$cache_capacity" "$isolates_created" "$isolates_dropped" \
      "$isolates_active" "$sessions" >>"$samples_file"
    if [[ "$available" != "$base_available" || "$active" != "$base_active" || \
          "$jobs" != "$base_jobs" || "$running" != "$base_running" || \
          "$retained" != "$base_retained" || "$descendant_count" != 0 ]]; then
      failed=1
      printf "    ${RED}ownership did not return to baseline at request %s${NC}\n" "$i"
      break
    fi
    if (( jobs > 1024 || retained > 268435456 )); then
      failed=1
      printf "    ${RED}job ownership exceeded process caps at request %s${NC}\n" "$i"
      break
    fi
    if (( cache_entries > 4096 )); then
      failed=1
      printf "    ${RED}cache entry cap exceeded at request %s${NC}\n" "$i"
      break
    fi
    if (( cache_payload > 33554432 )); then
      failed=1
      printf "    ${RED}cache payload cap exceeded at request %s${NC}\n" "$i"
      break
    fi
    if [[ "$cache_entries" != "$base_cache_entries" || \
          "$cache_payload" != "$base_cache_payload" || \
          "$cache_key_bytes" != "$base_cache_key_bytes" || \
          "$cache_capacity" != "$base_cache_capacity" ]]; then
      failed=1
      printf "    ${RED}cache ownership changed after warmup at request %s${NC}\n" "$i"
      break
    fi
    if (( isolates_active != base_isolates_active || \
          isolates_created - isolates_dropped != base_isolates_created - base_isolates_dropped )); then
      failed=1
      printf "    ${RED}isolate ownership did not return to baseline at request %s${NC}\n" "$i"
      break
    fi
    if (( sessions != base_sessions )); then
      failed=1
      printf "    ${RED}session ownership changed from %s to %s at request %s${NC}\n" \
        "$base_sessions" "$sessions" "$i"
      break
    fi
  done

  if [[ "$telemetry_missing" -eq 1 ]]; then
    plateau_fail "required daemon ownership telemetry disappeared during sampling" \
      "daemon plateau telemetry"
    stop_daemon
    stop_plateau_fixture
    rm -rf "$work_dir"
    return 1
  fi

  local final_threads slope
  final_threads=$(daemon_thread_count)
  slope=$(python3 - "$rss_file" <<'PY'
import pathlib, sys
values = [float(line) for line in pathlib.Path(sys.argv[1]).read_text().splitlines() if line]
values = values[-100:]
if len(values) < 2:
    print(0.0)
else:
    xs = list(range(len(values)))
    xbar = sum(xs) / len(xs)
    ybar = sum(values) / len(values)
    denom = sum((x - xbar) ** 2 for x in xs)
    print(sum((x - xbar) * (y - ybar) for x, y in zip(xs, values)) / denom)
PY
  )
  if ! python3 -c 'import sys; raise SystemExit(0 if float(sys.argv[1]) <= float(sys.argv[2]) else 1)' "$slope" "$slope_limit"; then
    failed=1
    printf "    ${RED}continuing RSS slope %.2f KiB/request exceeds %s${NC}\n" "$slope" "$slope_limit"
  fi
  if (( final_threads > base_threads + 2 )); then
    failed=1
    printf "    ${RED}thread count grew from %s to %s${NC}\n" "$base_threads" "$final_threads"
  fi

  if [[ "$failed" -eq 0 ]]; then
    printf "    ${GREEN}PASS${NC}: cache, isolate, session, job, and capture ownership returned to baseline; RSS slope %.2f KiB/request\n" "$slope"
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("daemon local Tier-2 plateau")
    printf "    samples retained for diagnosis: %s\n" "$samples_file"
  fi

  stop_daemon
  stop_plateau_fixture
  [[ "$failed" -eq 0 ]] && rm -rf "$work_dir"
}

daemon_scrape() {
  local url="$1"
  shift
  # Build JSON body from remaining args (formats, etc.)
  local formats='["markdown"]'
  local extras=""
  if [[ "$#" -gt 0 ]]; then
    # Accept --formats "json,links" or -f json etc
    formats="["
    local sep=""
    for f in "$@"; do
      formats="${formats}${sep}\"${f}\""
      sep=","
    done
    formats="${formats}]"
  fi
  local body
  body=$(printf '{"url":"%s","formats":%s}' "$url" "$formats")
  $TIMEOUT_CMD curl -sf -X POST "http://127.0.0.1:$DAEMON_PORT/v1/scrape" \
    -H 'content-type: application/json' \
    -d "$body" 2>/dev/null
}

daemon_scrape_raw() {
  local url="$1"
  shift
  local formats='["markdown"]'
  local extras=""
  if [[ "$#" -gt 0 ]]; then
    formats="["
    local sep=""
    for f in "$@"; do
      formats="${formats}${sep}\"${f}\""
      sep=","
    done
    formats="${formats}]"
  fi
  local body
  body=$(printf '{"url":"%s","formats":%s}' "$url" "$formats")
  curl -s -X POST "http://127.0.0.1:$DAEMON_PORT/v1/scrape" \
    -H 'content-type: application/json' \
    -d "$body" 2>&1
}

run_daemon_test() {
  local name="$1"
  local url="$2"
  local expected="$3"
  shift 3
  local formats=("$@")

  should_run "$name" || { printf "  ${YELLOW}⊟${NC} %s  (filtered out)\n" "$name"; return; }

  printf "  ${BOLD}▶${NC} %s\n" "$name"
  printf "    url: %s (daemon API)\n" "$url"

  local start
  start=$(date +%s%N 2>/dev/null || python3 -c 'import time; print(time.time_ns())')
  local output
  output=$(daemon_scrape "$url" "${formats[@]}")
  local exit_code=$?
  local end
  end=$(date +%s%N 2>/dev/null || python3 -c 'import time; print(time.time_ns())')

  local elapsed
  if [[ "$start" =~ ^[0-9]+$ && "$end" =~ ^[0-9]+$ && ${#start} -gt 12 ]]; then
    elapsed="$(( (end - start) / 1000000 ))ms"
  else
    elapsed="?"
  fi

  local status
  if [[ "$exit_code" -eq 0 ]] && [[ -n "$output" ]]; then
    # Extract markdown from JSON response via python3
    local md
    md=$(echo "$output" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    m = d.get('data', {}).get('markdown', '')
    print(m[:5000])
except:
    print('')
" 2>/dev/null)
    if echo "$md" | grep -q "$expected"; then
      status="${GREEN}PASS${NC}"
      PASS=$((PASS + 1))
    else
      status="${RED}FAIL${NC}"
      printf "    ${RED}✗${NC} expected '%s' not found in markdown\n" "$expected"
      printf "    ${YELLOW}markdown (first 300 chars):${NC}\n"
      printf "      %s\n" "${md:0:300}"
      FAIL=$((FAIL + 1))
      FAILED_TESTS+=("$name")
    fi
  else
    status="${RED}FAIL${NC} (exit $exit_code)"
    printf "    ${RED}✗${NC} curl exit code: $exit_code\n"
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("$name")
  fi

  printf "    ${status}  ${elapsed}\n"
  echo ""
}

run_daemon_test_http() {
  local name="$1"
  local url="$2"
  local expected_status="$3"
  local expected_in_body="$4"
  shift 4
  local formats=("$@")

  should_run "$name" || { printf "  ${YELLOW}⊟${NC} %s  (filtered out)\n" "$name"; return; }

  printf "  ${BOLD}▶${NC} %s (HTTP status check)\n" "$name"
  printf "    url: %s (daemon API)\n" "$url"

  local start
  start=$(date +%s%N 2>/dev/null || python3 -c 'import time; print(time.time_ns())')
  local response
  response=$(daemon_scrape_raw "$url" "${formats[@]}")
  local exit_code=$?
  local end
  end=$(date +%s%N 2>/dev/null || python3 -c 'import time; print(time.time_ns())')

  local elapsed
  if [[ "$start" =~ ^[0-9]+$ && "$end" =~ ^[0-9]+$ && ${#start} -gt 12 ]]; then
    elapsed="$(( (end - start) / 1000000 ))ms"
  else
    elapsed="?"
  fi

  local status="${RED}FAIL${NC}"
  # Check HTTP success field and optional content
  local check
  check=$(echo "$response" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    s = d.get('success', False)
    print('OK' if s == $expected_status else 'FAIL_status')
except:
    print('PARSE_ERR')
" 2>/dev/null)

  if [[ "$check" == "OK" ]]; then
    if [[ -n "$expected_in_body" ]]; then
      if echo "$response" | grep -q "$expected_in_body"; then
        status="${GREEN}PASS${NC}"
        PASS=$((PASS + 1))
      else
        status="${RED}FAIL${NC} (expected content not found)"
        printf "    ${RED}✗${NC} expected '%s' in response body\n" "$expected_in_body"
        FAIL=$((FAIL + 1))
        FAILED_TESTS+=("$name")
      fi
    else
      status="${GREEN}PASS${NC}"
      PASS=$((PASS + 1))
    fi
  else
    status="${RED}FAIL${NC} (unexpected success=$check)"
    printf "    ${RED}✗${NC} expected success=$expected_status\n"
    printf "    ${YELLOW}response (first 300 chars):${NC}\n"
    printf "      %s\n" "${response:0:300}"
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("$name")
  fi

  printf "    ${status}  ${elapsed}\n"
  echo ""
}

# ===========================================================================
#  DAEMON TESTS
# ===========================================================================
daemon_tests() {
  banner "DAEMON TESTS — REST API (POST /v1/scrape)"

  start_daemon "$DAEMON_PORT" || return 1

  run_daemon_test "daemon: example.com" "https://example.com" "Example Domain"
  run_daemon_test "daemon: Hacker News" "https://news.ycombinator.com" "Hacker News"
  run_daemon_test "daemon: Wikipedia" "https://en.wikipedia.org/wiki/Rust_(programming_language)" "Rust"
  run_daemon_test "daemon: Books to Scrape" "https://books.toscrape.com" "All products"
  run_daemon_test "daemon: Quotes to Scrape" "https://quotes.toscrape.com" "Quotes to Scrape"
  run_daemon_test "daemon: BBC News" "https://www.bbc.com/news" "BBC"
  run_daemon_test "daemon: httpbin" "https://httpbin.org/html" "Herman Melville"
  run_daemon_test "daemon: docs.rs" "https://docs.rs/" "docs.rs"

  # Feature tests via daemon
  run_daemon_test "daemon: links format" "https://example.com" "iana.org" "links"
  run_daemon_test "daemon: no main content" "https://example.com" "Example Domain" "markdown"
  run_daemon_test "daemon: include tag" "https://httpbin.org/html" "Herman Melville" "markdown"
  run_daemon_test "daemon: GitHub" "https://github.com" "GitHub" "markdown"

  # Error cases
  run_daemon_test_http "daemon: invalid URL" "https://this-domain-does-not-exist-12345.com" "False" ""

  stop_daemon
}
# ===========================================================================
#  SITE TESTS
# ===========================================================================
site_tests() {
  banner "SITE TESTS — Real-World Websites (basic markdown scrape)"

  run_test "example.com" "https://example.com" "Example Domain"
  run_test "httpbin HTML" "https://httpbin.org/html" "Herman Melville"
  run_test "Hacker News" "https://news.ycombinator.com" "Hacker News"
  run_test_error "Lobsters (robots.txt)" "https://lobste.rs" 1 "robots"
  run_test "Wikipedia (Rust)" "https://en.wikipedia.org/wiki/Rust_(programming_language)" "Rust"

  run_test "Books to Scrape" "https://books.toscrape.com" "All products"
  run_test "Quotes to Scrape" "https://quotes.toscrape.com" "Quotes to Scrape"

  run_test "docs.rs" "https://docs.rs/" "docs.rs"
  run_test "MDN HTML anchor" "https://developer.mozilla.org/en-US/docs/Web/HTML/Element/a" "a>"

  run_test "GitHub (serde)" "https://github.com/serde-rs/serde" "serde"

  run_test "BBC News" "https://www.bbc.com/news" "BBC"
}

# ===========================================================================
#  FEATURE TESTS
# ===========================================================================
feature_tests() {
  banner "FEATURE TESTS — Flags, Options, Format Variants"

  run_test_json "JSON format — example.com" "https://example.com" --format json
  run_test_json "JSON format — HN" "https://news.ycombinator.com" --format json

  run_test_json "Both format — example.com" "https://example.com" --format both

  run_test "Links format — example.com" "https://example.com" "iana.org" --format links

  run_test_json "Raw HTML format — example.com" "https://example.com" --format raw-html

  run_test_json "JSON envelope — httpbin" "https://httpbin.org/html" --json

  run_test "Pretty JSON — httpbin" "https://httpbin.org/html" '"markdown"' --json --pretty

  run_test "No main content — example.com" "https://example.com" "Example Domain" --no-main-content

  run_test "Include tag — httpbin" "https://httpbin.org/html" "Herman Melville" --include-tag "body"

  run_test "Exclude tag — quotes" "https://quotes.toscrape.com" "Top Ten tags" --exclude-tag ".col-md-8"

  run_test_json "Ignore robots — HN" "https://news.ycombinator.com" --ignore-robots --json

  run_test "Timeout flag — example.com" "https://example.com" "Example Domain" --timeout 5000

  run_test "Tier max 0 — example.com" "https://example.com" "Example Domain" --tier-max 0

  run_test "Custom header" "https://httpbin.org/headers" "User-Agent" --header "User-Agent: DracoTest/1.0"

  run_test_json "Extract jsonpath — example.com" "https://example.com" --extract '$.markdown' --json
}

# ===========================================================================
#  ERROR TESTS
# ===========================================================================
error_tests() {
  banner "ERROR & EDGE-CASE TESTS"

  run_test_error "Invalid URL" "https://this-domain-does-not-exist-12345.com" 1 ""
  run_test_error "Help flag" "" 0 "Usage" --help
}

hardmode_tests() {
  banner "HARDMODE TESTS — JS-heavy SPAs, rendered content"

  run_test_json "HN item JSON" "https://hacker-news.firebaseio.com/v0/item/8863.json" --format json

  run_test "GitHub homepage" "https://github.com" "GitHub" --capture-window-ms 3000

  run_test "npmjs express" "https://www.npmjs.com/package/express" "Express" --capture-window-ms 3000

  run_test "Worldometer" "https://www.worldometers.info/world-population/" "World Population" --tier-max 0
}

# ===========================================================================
#  MAIN
# ===========================================================================
echo ""
printf "${BOLD}╔══════════════════════════════════════════════════════════════╗${NC}\n"
printf "${BOLD}║          DRACO LIVE TEST SUITE                              ║${NC}\n"
printf "${BOLD}║  %s                         ║${NC}\n" "$(date)"
printf "${BOLD}║  binary: %s${NC}\n" "$DRACO"
printf "${BOLD}╚══════════════════════════════════════════════════════════════╝${NC}\n"

if [[ ! -x "$DRACO" ]]; then
  printf "${RED}Error: draco binary not found at %s${NC}\n" "$DRACO"
  echo "Set DRACO_BIN or build: cargo build --release"
  exit 1
fi

if [[ "$RUN_ALL" == true ]]; then
  site_tests
  feature_tests
  hardmode_tests
  error_tests
elif [[ "$RUN_SITE" == true ]]; then
  site_tests
elif [[ "$RUN_FEATURE" == true ]]; then
  feature_tests
elif [[ "$RUN_ERROR" == true ]]; then
  error_tests
elif [[ "$RUN_HARD" == true ]]; then
  hardmode_tests
elif [[ "$RUN_DAEMON" == true ]]; then
  daemon_tests
elif [[ "$RUN_PLATEAU" == true ]]; then
  plateau_tests
elif [[ "$RUN_QUICK" == true ]]; then
  banner "QUICK MODE — Representative subset"
  run_test "example.com" "https://example.com" "Example Domain"
  run_test "Hacker News" "https://news.ycombinator.com" "Hacker News"
  run_test "Wikipedia" "https://en.wikipedia.org/wiki/Rust_(programming_language)" "Rust"
  run_test "Books to Scrape" "https://books.toscrape.com" "All products"
  run_test_json "JSON format — httpbin" "https://httpbin.org/html" --format json
  run_test "Links format — example.com" "https://example.com" "iana.org" --format links
  run_test_json "JSON envelope — HN" "https://news.ycombinator.com" --json
  run_test "No main content" "https://example.com" "Example Domain" --no-main-content
  run_test_error "Invalid URL" "https://this-domain-does-not-exist-12345.com" 1 ""
  run_test_error "Help flag" "" 0 "Usage" --help
fi

# ===========================================================================
#  SUMMARY
# ===========================================================================
echo ""
printf "${CYAN}══════════════════════════════════════════════════════════════${NC}\n"
if [[ "$FAIL" -eq 0 ]]; then
  printf "  ${GREEN}✓${NC} All %s tests passed! (%s skipped)\n" "$PASS" "$SKIP"
else
  printf "  ${RED}✗${NC} %s tests failed, %s passed\n" "$FAIL" "$PASS"
  echo ""
  printf "  ${BOLD}Failed tests:${NC}\n"
  for t in "${FAILED_TESTS[@]}"; do
    printf "    ${RED}●${NC} %s\n" "$t"
  done
fi
printf "${CYAN}══════════════════════════════════════════════════════════════${NC}\n"
echo ""

exit "$FAIL"
