# Render Memory and Reliability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Preserve Draco's proven SPA output and anti-bot success while removing false browser escalation, attributing the 240-300 MiB Tier-2 live set, reducing safe copies/retention, and proving whether API-first extraction can replace any full hydrations.

**Architecture:** Keep the existing ladder and make every optimization earn promotion through semantic equivalence gates. Separate single-request peak work from daemon-retention work: phase telemetry and fresh-process SPA probes drive peak-RSS changes, while logical ownership gauges and repeated-request tests drive leak fixes. API-first remains an experimental branch unless it reproduces the rendered content contract, not merely a JSON response.

**Tech Stack:** Rust 2021, Tokio, deno_core/V8, happy-dom, draco-net, draco-static, jq, macOS `/usr/bin/time -l`, existing Cargo tests.

---

## Guardrails and measured baseline

- Preserve the current dirty worktree. Before execution, inspect `git diff`; never reset, stash, or overwrite the user's changes.
- Current measured fresh-process peaks:
  - Thrill: 299.7 MiB Tier 2 versus 22.0 MiB Tier 0; successful Markdown >44,000 chars.
  - Bluff: 240.1 MiB Tier 2 versus 28.2 MiB Tier 0; successful Markdown >20,000 chars.
- Target's browser escalation is a correctness bug, not a memory optimization: rich static content was overridden first by benign PerimeterX telemetry and then by a degraded `6047 -> 445` happy-dom result.
- No optimization ships if either SPA loses its catalog/content assertions, returns a shell, adds an application-error marker, or escalates to the real browser unexpectedly.
- Do not add BrowserOxide as a speculative tier in this plan. Its useful intersection has not been demonstrated on actual Draco Tier-2 traffic.

## File map

- `crates/draco-core/src/challenge.rs`: challenge dominance policy.
- `crates/draco-core/src/machine.rs`: render acceptance, hydration-collapse policy, Target-shaped ladder tests.
- `crates/draco-core/src/tier2.rs`: capture ownership, semaphore lifetime, API response bridge, cache-backed fetchers.
- `crates/draco-core/src/chunk_cache.rs`: bounded shared source storage and cache observability.
- `crates/draco-runtime/src/lib.rs`: isolate creation, phase telemetry, script/module ownership, API bridge, capture watchdog.
- `crates/draco-runtime/js/glue.js`: fetch/XHR response implementation and raw-input release.
- `crates/draco-runtime/src/session.rs`: interact watchdog and serialized-DOM retention.
- `crates/draco-cli/src/serve/jobs.rs`: async-job retention limits and expiry.
- `crates/draco-heavy/src/browser.rs`: deterministic browser-timeout cleanup.
- `tests/profile_spa_memory.sh`: fresh-process memory and semantic probe.
- `docs/API_FIRST_FEASIBILITY.md`: measured go/no-go result for API-first Markdown.

---

### Task 1: Lock in the Target no-browser regression

**Files:**
- Modify: `crates/draco-core/src/challenge.rs:197-291`
- Modify: `crates/draco-core/src/machine.rs:671-691,1076-1250`
- Modify: `crates/draco-core/src/testutil.rs:260-335`
- Test: `crates/draco-core/src/machine.rs` test module

- [ ] **Step 1: Preserve and review the current guards**

Keep these two existing policies:

```rust
let kind = capture
    .candidates
    .iter()
    .find_map(|candidate| detect_network_challenge(&candidate.url))?;
runtime_challenge_dominates(capture.rendered_html.as_deref())
    .then(|| format!("network:{}", kind.as_str()))
```

```rust
if let Some(detail) = hydration_collapse_detail(prev_len, new_len) {
    if prev_len < CHALLENGE_FALLBACK_CONTENT_CHARS {
        return Some(finish_runtime_challenge(run, detail));
    }
    run.record(
        SourceTier::RuntimeInterception,
        "runtime.render",
        StepOutcome::Missed,
        cap_ms,
        Bucket::Runtime,
        Some(format!("{detail}; kept content-rich static shell")),
    );
    return None;
}
```

- [ ] **Step 2: Write the combined Target-shaped failing regression test**

Add a test whose rendered document is globally rich enough to suppress a benign PX telemetry signal, while `only_main_content` deliberately collapses to 445 characters. This exercises both guards in one ladder run:

```rust
#[cfg(feature = "tier2")]
#[tokio::test]
async fn target_like_px_telemetry_and_main_collapse_keep_rich_static_result() {
    let shell_markdown = "Target category product promotion and store content. ".repeat(140);
    let rendered = format!(
        "<html><body><nav>{}</nav><main>{}</main></body></html>",
        "Real navigation and store content. ".repeat(80),
        "h".repeat(445),
    );
    let fetcher = MockFetcher::ok_html(200, "<html><body>shell</body></html>");
    let statics = MockStatic::miss_no_build_id()
        .with_markdown(&shell_markdown)
        .with_incomplete(true);
    let capture = MockCapture::rendered_with_candidates(
        rendered,
        vec![Candidate::get(
            "https://ift.px-cloud.net/ns?appId=PXtest",
            InterceptVia::Fetch,
        )],
    );
    let config = Config {
        formats: FormatSet::markdown_only(),
        only_main_content: true,
        force_render: true,
        ..cfg(2)
    };

    let result = run_ladder(
        "https://www.target.com/",
        &config,
        &fetcher,
        &statics,
        &capture,
    )
    .await;

    assert_eq!(result.status, Status::Success);
    assert_eq!(result.source_tier, Some(SourceTier::Static));
    assert_eq!(result.markdown.as_deref(), Some(shell_markdown.as_str()));
    assert!(result.trace.iter().all(|step| step.action != "core.challenge"));
    assert!(result.trace.iter().any(|step| {
        step.action == "runtime.render"
            && step.detail.as_deref().is_some_and(|detail| {
                detail.contains("kept content-rich static shell")
            })
    }));
}
```

Add this focused constructor to `MockCapture` rather than exposing its private
`result` field:

```rust
pub fn rendered_with_candidates(
    dom: impl Into<String>,
    candidates: Vec<Candidate>,
) -> Self {
    let bodies = vec![None; candidates.len()];
    Self {
        result: Ok(CaptureResult {
            candidates,
            bodies,
            outcome: RuntimeOutcome::Quiesced,
            sandbox_level: Some(Self::MOCK_LEVEL.to_string()),
            rendered_html: Some(dom.into()),
            logs: Vec::new(),
        }),
        calls: AtomicUsize::new(0),
    }
}
```

- [ ] **Step 3: Add policy-boundary tests**

Use table cases for static content lengths 999 and 1000. Assert 999 plus a severe collapse returns `NeedsBrowser`; assert 1000 retains the static result. Add a genuine thin PerimeterX wall case and assert it still returns `NeedsBrowser`.

- [ ] **Step 4: Run focused verification**

Run:

```bash
cargo test -p draco-core --features tier2 challenge::tests -- --nocapture
cargo test -p draco-core --features tier2 target_like_px_telemetry_and_main_collapse_keep_rich_static_result -- --nocapture
cargo test -p draco-core --features tier2 hydration_collapse -- --nocapture
```

Expected: all pass; the combined test has no `core.challenge` trace.

- [ ] **Step 5: Verify Target live without compiling heavy fallback**

```bash
cargo run -q -p draco-cli --no-default-features --features tier2 -- \
  scrape https://www.target.com --json --runtime-log |
jq -e '
  .status == "success" and
  .source_tier == "static" and
  (all(.trace[]?; .action != "core.challenge")) and
  any(.trace[]?; (.detail // "") | contains("kept content-rich static shell"))
'
```

Expected: exit 0 and no browser process.

- [ ] **Step 6: Stage only reviewed Target-policy hunks and commit**

```bash
git add -p crates/draco-core/src/challenge.rs crates/draco-core/src/machine.rs crates/draco-core/src/testutil.rs
git diff --cached --check
git commit -m "fix: retain rich content across challenge telemetry"
```

---

### Task 2: Add phase-level V8 and source-byte attribution

**Files:**
- Modify: `crates/draco-runtime/src/lib.rs:877-1127`
- Modify: `crates/draco-core/src/chunk_cache.rs:98-149`
- Test: `crates/draco-runtime/src/lib.rs` test module
- Create: `tests/profile_spa_memory.sh`

- [ ] **Step 1: Write a test for opt-in phase logs**

Create a fixture capture and assert logs contain these ordered phase names:

```rust
let phases: Vec<&str> = report
    .logs
    .iter()
    .filter_map(|line| line.strip_prefix("[raze.memory] phase="))
    .map(|rest| rest.split_whitespace().next().unwrap())
    .collect();
assert_eq!(
    phases,
    ["snapshot", "dom", "scripts-fetched", "scripts-run", "settled", "serialized"]
);
```

- [ ] **Step 2: Add a heap snapshot helper**

Add a private helper that records V8 heap and Rust-side source counts through the already-bounded runtime log channel:

```rust
fn log_memory_phase(
    runtime: &mut JsRuntime,
    cap: &Rc<RefCell<CaptureState>>,
    phase: &str,
    module_bytes: usize,
    external_script_bytes: usize,
) {
    let mut heap = deno_core::v8::HeapStatistics::default();
    runtime.v8_isolate().get_heap_statistics(&mut heap);
    cap.borrow_mut().push_log(&format!(
        "[raze.memory] phase={phase} used_heap={} total_heap={} physical={} external={} heap_limit={} module_bytes={module_bytes} external_script_bytes={external_script_bytes}",
        heap.used_heap_size(),
        heap.total_heap_size(),
        heap.total_physical_size(),
        heap.external_memory(),
        heap.heap_size_limit(),
    ));
}
```

Call it after snapshot restore, DOM creation, external-script fetch, script evaluation, settle, and serialization. Compute byte totals from the existing `modules` and `ext_bytes` maps without cloning values.

- [ ] **Step 3: Expose cache counters without exposing cache contents**

Add a test-only/runtime-log-friendly value object:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RamCacheStats {
    pub entries: usize,
    pub payload_bytes: usize,
    pub key_bytes: usize,
    pub capacity: usize,
}
```

`ChunkCache::ram_stats()` must calculate these values under one lock. Do not change eviction behavior in this task.

- [ ] **Step 4: Create a fresh-process semantic/RSS probe**

`tests/profile_spa_memory.sh` must:

1. Require an already-built `target/release/draco`.
2. Run each URL in a fresh process with `/usr/bin/time -l`.
3. Save JSON and timing text in a temporary directory.
4. Assert Thrill Markdown length >40,000, the games snapshot and providers endpoint are present, and quiescence closed the window.
5. Assert Bluff Markdown length >20,000, the promotions endpoint is present, and quiescence closed the window.
6. Print maximum RSS bytes and the `[raze.memory]` lines for each site.

The script must exit non-zero on semantic failure even if RSS improves.

- [ ] **Step 5: Verify instrumentation**

```bash
cargo test -p draco-runtime phase_memory_logs_are_ordered --release -- --nocapture
cargo build -p draco-cli --release
bash tests/profile_spa_memory.sh
```

Expected: both semantic gates pass and every phase includes V8/source-byte data.

- [ ] **Step 6: Commit instrumentation separately**

```bash
git add crates/draco-runtime/src/lib.rs crates/draco-core/src/chunk_cache.rs tests/profile_spa_memory.sh
git diff --cached --check
git commit -m "perf: attribute tier2 memory by phase"
```

---

### Task 3: Remove safe capture and session DOM copies

**Files:**
- Modify: `crates/draco-runtime/src/lib.rs:1098-1105,1317-1336`
- Modify: `crates/draco-runtime/src/session.rs:463-481`
- Test: `crates/draco-runtime/tests/interact_session.rs`

- [ ] **Step 1: Write failing ownership tests**

Add one test that serializes a multi-megabyte session DOM twice and asserts both replies are identical. Add a capture-unit assertion that `CaptureState.requests`, `rendered_html`, and `logs` are empty immediately after report extraction.

- [ ] **Step 2: Move capture fields instead of cloning**

Replace final report cloning with:

```rust
let mut state = cap.borrow_mut();
CaptureReport {
    outcome,
    requests: std::mem::take(&mut state.requests),
    rendered_html: state.rendered_html.take(),
    logs: std::mem::take(&mut state.logs),
}
```

Use the same `mem::take` pattern in boot-failure `finish`. Ensure no `RefCell` borrow remains live while `JsRuntime` drops.

- [ ] **Step 3: Stop retaining a serialized DOM in idle interact sessions**

Change the actor branch to:

```rust
Some(Command::Serialize { reply }) => {
    serialize_dom(runtime.as_mut().unwrap());
    let html = cap.as_ref().unwrap().borrow_mut().rendered_html.take();
    let _ = reply.send(html);
}
```

The next `Serialize` call repopulates the buffer from the still-live DOM.

- [ ] **Step 4: Clear raw bootstrap inputs after DOM construction**

After `draco:glue` succeeds, execute:

```rust
let _ = runtime.execute_script(
    "draco:release-inputs",
    "globalThis.__DRACO_HTML__=''; globalThis.__DRACO_STUB__='';",
);
```

Keep `__DRACO_URL__` because later dynamic imports and navigation-relative code may need it.

- [ ] **Step 5: Verify behavior and measure**

```bash
cargo test -p draco-runtime --test interact_session --release -- --nocapture
cargo test -p draco-runtime --test capture --release -- --nocapture
bash tests/profile_spa_memory.sh
```

Expected: semantic probes unchanged. Treat an RSS change under 2 MiB as hygiene, not a failed task.

- [ ] **Step 6: Commit**

```bash
git add crates/draco-runtime/src/lib.rs crates/draco-runtime/src/session.rs crates/draco-runtime/tests/interact_session.rs
git diff --cached --check
git commit -m "perf: move tier2 output buffers out of live state"
```

---

### Task 4: Share cached/module source bytes and bound preload retention

**Files:**
- Modify: `crates/draco-runtime/src/lib.rs:172-174,760-864,950-1035`
- Modify: `crates/draco-core/src/chunk_cache.rs:70-149,210-261`
- Modify: `crates/draco-core/src/tier2.rs:404-548`
- Test: `crates/draco-core/src/chunk_cache.rs` tests
- Test: `crates/draco-runtime/tests/esm_dyn.rs`

- [ ] **Step 1: Introduce one shared source-byte type**

Define in `draco-runtime`:

```rust
pub type SharedSource = std::sync::Arc<[u8]>;

pub trait ScriptFetcher {
    fn fetch<'a>(&'a self, url: &'a str) -> LocalBoxFuture<'a, Option<SharedSource>>;
}
```

Update map/null fetchers, the chunk-cache adapter, `MapModuleLoader`, and tests to clone the `Arc`, not the payload.

- [ ] **Step 2: Make cache get/put share allocations**

Change the RAM entry to `Arc<[u8]>`; make `ChunkCache::get` return `Option<Arc<[u8]>>`. A disk hit performs exactly one `Vec<u8> -> Arc<[u8]>` conversion, inserts a clone of the `Arc`, and returns the original `Arc`.

Add tests using `Arc::ptr_eq` to prove a RAM hit shares its payload.

- [ ] **Step 3: Bound empty/tiny-entry metadata**

Add:

```rust
const MAX_RAM_ENTRIES: usize = 4_096;
```

Reject empty payloads from RAM admission. Evict while either payload bytes exceed the 32 MiB budget or entry count exceeds 4,096. Shrink map capacity only after a large eviction when `capacity > len.saturating_mul(4).max(64)`.

- [ ] **Step 4: Replace all-at-once script prefetch with a byte-bounded ordered queue**

Keep document-order execution, but retain at most 16 MiB of not-yet-executed external script bytes. Fetch ahead concurrently until the byte budget is reached; execute the next script; then refill the queue. Do not change module resolution or script ordering.

Add a fixture with four 6 MiB external scripts and assert execution order remains `[0,1,2,3]` while the recorded `external_script_bytes` peak stays at or below 18 MiB (one completed fetch may cross the 16 MiB soft boundary).

- [ ] **Step 5: Evict raw module source after V8 accepts it**

Make `MapModuleLoader::load` remove the resolved source from the per-capture registry when constructing `ModuleSource`. Keep a separate set of loaded specifiers so duplicate imports continue to resolve through V8's module map rather than refetching.

- [ ] **Step 6: Verify and measure**

```bash
cargo test -p draco-core chunk_cache --release -- --nocapture
cargo test -p draco-runtime --test esm_dyn --release -- --nocapture
cargo test -p draco-runtime --test capture --release -- --nocapture
bash tests/profile_spa_memory.sh
```

Expected: no semantic regression; phase logs show lower module/external-script retained bytes. Revert the preload portion if either SPA exceeds its prior runtime by more than 20% across five runs.

- [ ] **Step 7: Commit shared ownership before preload policy**

Create two commits so preload changes can be reverted independently:

```bash
git add crates/draco-core/src/chunk_cache.rs crates/draco-core/src/tier2.rs crates/draco-runtime/src/lib.rs
git commit -m "perf: share tier2 source buffers"
git add crates/draco-runtime/src/lib.rs crates/draco-runtime/tests/esm_dyn.rs
git commit -m "perf: bound parser script prefetch memory"
```

---

### Task 5: Remove JSON-inside-JSON response duplication

**Files:**
- Modify: `crates/draco-runtime/src/lib.rs:192-210,560-610`
- Modify: `crates/draco-runtime/js/glue.js:350-430`
- Test: `crates/draco-runtime/tests/capture.rs`
- Test: `crates/draco-runtime/tests/dom_globals.rs`

- [ ] **Step 1: Add response-contract tests first**

Cover JSON, text, empty, non-2xx, duplicate headers, `Response.clone()`, XHR, and body-used errors. In particular:

```javascript
const response = await fetch('/api/data');
const clone = response.clone();
const first = await response.json();
const second = await clone.json();
let threw = false;
try { await response.text(); } catch (error) { threw = error instanceof TypeError; }
globalThis.__result = JSON.stringify({ first, second, threw, bodyUsed: response.bodyUsed });
```

Assert both parses match and the second consume on the original throws.

- [ ] **Step 2: Return a structured op value**

Replace the escaped JSON envelope string with:

```rust
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiResponseWire {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}
```

Have `op_raze_fetch` return `ApiResponseWire` directly through serde_v8. Remove `resp.to_string()` and the outer `JSON.parse(respJson)`; only the page's requested `.json()` parses the actual response body.

- [ ] **Step 3: Implement one-shot body storage in `glue.js`**

Store body text in a shared internal body-state object. `clone()` duplicates an unconsumed state. `json()`, `text()`, and `arrayBuffer()` mark that response consumed, clear its body string after conversion, update `bodyUsed`, and throw `TypeError` on a second consume.

- [ ] **Step 4: Verify and measure Thrill's API phase**

```bash
cargo test -p draco-runtime --test capture --release -- --nocapture
cargo test -p draco-runtime --test dom_globals --release -- --nocapture
bash tests/profile_spa_memory.sh
```

Expected: Thrill retains its games catalog. Compare the before/after V8 heap at the largest API-response phase; keep the change only if it reduces peak or phase heap without a material runtime regression.

- [ ] **Step 5: Commit**

```bash
git add crates/draco-runtime/src/lib.rs crates/draco-runtime/js/glue.js crates/draco-runtime/tests/capture.rs crates/draco-runtime/tests/dom_globals.rs
git diff --cached --check
git commit -m "perf: avoid nested JSON copies in live fetch responses"
```

---

### Task 6: Find the lowest safe V8 heap cap empirically

**Files:**
- Modify: `crates/draco-runtime/src/lib.rs:80-93`
- Modify: `tests/profile_spa_memory.sh`
- Test: `crates/draco-runtime/src/lib.rs` test module

- [ ] **Step 1: Add an experimental, clamped heap-cap override**

Keep 192 MiB as the default. Parse `DRACO_CAPTURE_MAX_HEAP_MB`, allow only `128..=192`, and fall back to 192 on missing/invalid input:

```rust
fn capture_max_heap_bytes() -> usize {
    std::env::var("DRACO_CAPTURE_MAX_HEAP_MB")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| (128..=192).contains(value))
        .unwrap_or(192)
        * 1024 * 1024
}
```

Use this value in `capture_create_params`. Add tests serializing access to the environment variable.

- [ ] **Step 2: Run a five-run matrix**

For 192, 176, 160, 144, and 128 MiB, run Thrill and Bluff five times in fresh processes. Record median/max RSS, median/max runtime, output length, endpoint assertions, V8 used/total heap, and failures.

- [ ] **Step 3: Apply the promotion rule**

Promote a lower default only when all ten runs at that cap pass semantic assertions, no OOM/termination occurs, median runtime regression is <=15%, max runtime regression is <=25%, and fresh-process median RSS improves by at least 10 MiB on one SPA without worsening the other.

- [ ] **Step 4: Remove the environment override if it has no operational value**

If 192 MiB remains the safe default and the override is only a benchmarking aid, remove it after recording the matrix in the commit message/plan results. Do not leave an undocumented production tuning surface.

- [ ] **Step 5: Commit only a proven default change**

```bash
git add crates/draco-runtime/src/lib.rs tests/profile_spa_memory.sh
git diff --cached --check
git commit -m "perf: set measured safe tier2 heap ceiling"
```

---

### Task 7: Make Tier-2 cancellation and synchronous JavaScript bounded

**Files:**
- Modify: `crates/draco-core/src/tier2.rs:594-709`
- Modify: `crates/draco-runtime/src/lib.rs:249-281,877-1105`
- Modify: `crates/draco-runtime/src/session.rs:428-565,724-754`
- Test: `crates/draco-core/src/tier2.rs` tests
- Test: `crates/draco-runtime/tests/interact_session.rs`

- [ ] **Step 1: Write a cancellation/concurrency regression test**

Use a pool of size one and an injected blocking capture seam. Abort the awaiting task after the blocking closure starts. Assert a second capture cannot acquire capacity until the first closure exits.

- [ ] **Step 2: Move the permit into blocking ownership**

In `Tier2Pool::capture`, acquire the `OwnedSemaphorePermit`, clone request values, and move the permit into the `spawn_blocking` closure that calls `capture_blocking`. Do not call `ProdTier2Capture::capture`, because its closure currently cannot own the pool permit:

```rust
let permit = self.permits.clone().acquire_owned().await.map_err(...)?;
let url = url.to_owned();
let html = html.to_vec();
let config = config.clone();
let opts = opts.clone();
tokio::task::spawn_blocking(move || {
    let _permit = permit;
    capture_blocking(&url, &html, &config, &opts, mode)
})
.await
.map_err(|error| jail_error(JailKind::Spawn, format!("capture task panicked/cancelled: {error}")))?
```

- [ ] **Step 3: Add a V8 termination watchdog**

After creating `JsRuntime`, obtain:

```rust
let isolate_handle = runtime.v8_isolate().thread_safe_handle();
```

Run a watchdog thread scoped to the capture deadline. If the deadline expires before a completion signal, call `isolate_handle.terminate_execution()`. On normal completion, signal and join the watchdog. Map termination to the existing `RuntimeOutcome::Terminated`, then call `runtime.v8_isolate().cancel_terminate_execution()` before any intentional reuse.

- [ ] **Step 4: Apply the same watchdog to interact commands**

Wrap `do_exec` script execution with a per-command deadline and termination handle. After termination, either restore the actor to a known usable state and cancel termination, or close the actor and release its permit; choose the close-on-termination policy for the first implementation because it is easier to prove safe.

- [ ] **Step 5: Verify busy-loop behavior**

```bash
cargo test -p draco-core tier2_pool_abort_keeps_capacity_until_blocking_exit --release -- --nocapture
cargo test -p draco-runtime synchronous_page_loop_is_terminated --release -- --nocapture
cargo test -p draco-runtime --test interact_session exec_busy_loop_is_terminated --release -- --nocapture
```

Expected: no test hangs; actual active isolates never exceed the configured pool size.

- [ ] **Step 6: Commit**

```bash
git add crates/draco-core/src/tier2.rs crates/draco-runtime/src/lib.rs crates/draco-runtime/src/session.rs crates/draco-runtime/tests/interact_session.rs
git diff --cached --check
git commit -m "fix: bound tier2 cancellation and script execution"
```

---

### Task 8: Bound long-lived daemon retention

**Files:**
- Modify: `crates/draco-cli/src/serve/jobs.rs:1-290`
- Modify: `crates/draco-cli/src/serve/mod.rs:85-145`
- Modify: `crates/draco-core/src/chunk_cache.rs:70-113,222-261`
- Modify: `crates/draco-heavy/src/browser.rs:159-202`
- Test: corresponding module tests

- [ ] **Step 1: Make job expiry real**

Add `JobStore::reap_expired(now: SystemTime) -> usize`, removing jobs whose `created + JOB_TTL <= now`. Spawn a 60-second daemon maintenance interval that calls it and stops during graceful shutdown. Add a deterministic unit test using explicit `SystemTime` values; do not sleep for 24 hours.

- [ ] **Step 2: Add job count and retained-byte caps**

Track serialized bytes when appending `data`, `errors`, and `robots_blocked`. Set process-wide caps of 1,024 terminal jobs and 256 MiB retained terminal payload. Evict oldest terminal jobs first when either cap is exceeded; never evict a running job. Change `create_seeded` and `create_with_total` to return `Result<String, JobCapacityError>`; map `JobCapacityError` to the daemon's existing service-unavailable response when all capacity is occupied by running jobs.

Define the capacity error locally without adding a dependency:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JobCapacityError;

impl std::fmt::Display for JobCapacityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("async job capacity exhausted")
    }
}

impl std::error::Error for JobCapacityError {}
```

Expose `JobStoreStats { jobs, running, retained_bytes }` for health/debug output.

- [ ] **Step 3: Complete cache metadata bounds**

Verify Task 4's entry-count, empty-entry, key-byte, and capacity controls through a 10,000-unique-empty/tiny-URL test. Assert both payload bytes and entries plateau.

- [ ] **Step 4: Make browser timeout cleanup explicit**

Keep `Browser` ownership outside the timed drive future. On timeout, call kill/close and wait, abort and await the CDP handler, and verify descendants disappear. Retain dependency `kill_on_drop(true)` as a final safety net rather than the normal cleanup path.

- [ ] **Step 5: Add a repeated-request plateau test design to `tests/test_live.sh`**

Run five warmups followed by 200 sequential local-fixture Tier-2 requests. Sample daemon RSS, thread count, cache stats, active captures, isolate create/drop counts, sessions, jobs, and browser descendants. Pass when logical ownership returns to baseline, cache/jobs plateau under caps, no browser descendants remain, and the last 100 RSS samples have no continuing upward slope.

- [ ] **Step 6: Verify**

```bash
cargo test -p draco-cli --features tier2,serve jobs --release -- --nocapture
cargo test -p draco-core chunk_cache --release -- --nocapture
cargo test -p draco-heavy browser --release -- --nocapture
bash tests/test_live.sh
```

- [ ] **Step 7: Commit retention fixes by subsystem**

```bash
git add crates/draco-cli/src/serve/jobs.rs crates/draco-cli/src/serve/mod.rs
git commit -m "fix: expire and bound daemon job results"
git add crates/draco-core/src/chunk_cache.rs
git commit -m "fix: bound chunk cache metadata"
git add crates/draco-heavy/src/browser.rs
git commit -m "fix: reap browser fallback after timeout"
```

---

### Task 9: Measure API-first Markdown feasibility without shipping it

**Files:**
- Create: `docs/API_FIRST_FEASIBILITY.md`
- Create: `tests/api_first_probe.sh`
- Read/measure: `crates/draco-core/src/tier2.rs:193-284`
- Read/measure: `crates/draco-runtime/src/lib.rs:224-247,877-1105`

- [ ] **Step 1: Measure Observe mode on actual Tier-2 sites**

Run `discover` for Thrill and Bluff in fresh processes with runtime logs. Record peak RSS, endpoint catalog, best replayable response size/type, capture time, and whether the replayed JSON contains the catalog entities visible in rendered Markdown.

- [ ] **Step 2: Define a semantic recall probe**

`tests/api_first_probe.sh` must extract at least 100 stable visible tokens/entity names from the successful rendered Markdown, then measure what fraction appears in the replayed JSON. It must separately detect headings/order/links that cannot be reconstructed from JSON alone.

- [ ] **Step 3: Apply a strict decision gate**

Recommend an API-first production path only when all of the following hold on at least 50 representative Draco Tier-2 URLs:

- replay succeeds safely without mutation;
- >=95% of sampled visible entities are present;
- a site-agnostic transform reproduces required headings, links, and ordering;
- output contains no bootstrap/error shell markers;
- median peak RSS improves by >=25%;
- ambiguous cases automatically continue to full hydration.

If these conditions fail, document the result as `no-go for default Markdown`; keep the existing API-first behavior for JSON/endpoints only.

- [ ] **Step 4: Evaluate framework-specific cheap replays separately**

Record counts for recognizable Next data, Next RSC, SvelteKit `__data.json`, Nuxt payload, Shopify, and embedded JSON patterns. Propose a separate implementation only for formats with a stable, validated content mapping. Do not bundle generic JSON flattening into the default scraper.

- [ ] **Step 5: Commit only the probe and decision record**

```bash
git add docs/API_FIRST_FEASIBILITY.md tests/api_first_probe.sh
git diff --cached --check
git commit -m "docs: measure api-first markdown feasibility"
```

---

### Task 10: Run the release gate and keep only proven wins

**Files:**
- Modify: `docs/ROADMAP.md`
- Modify: `SHOWCASE.md` only if benchmark claims change

- [ ] **Step 1: Run full automated verification**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features --release
```

- [ ] **Step 2: Run five fresh-process semantic/RSS trials**

Run `tests/profile_spa_memory.sh` five times. Report median and maximum RSS/runtime rather than the best run.

- [ ] **Step 3: Re-run the eight-site shootout with semantic classifications**

For each site report static, Tier 2, or browser source; fully rendered, useful static, thin shell, runtime crash, or blocked; peak process-tree RSS; and runtime. Target must not launch a browser. Thrill and Bluff must satisfy their catalog assertions.

- [ ] **Step 4: Run daemon plateau and cancellation tests**

Verify isolate create/drop counts balance, active blocking captures return to zero, job/cache counters remain bounded, and Chrome descendants return to baseline.

- [ ] **Step 5: Apply the final promotion rule**

Keep a memory optimization only when it passes all correctness tests and either:

- reduces fresh-process median SPA peak by >=5%; or
- fixes a proven unbounded retention/cancellation path; or
- removes a measured copy with no performance/correctness regression.

Revert isolated experiments that fail this rule; do not retain complexity on the promise of future savings.

- [ ] **Step 6: Document final measured outcomes and commit**

```bash
git add docs/ROADMAP.md
git add -p SHOWCASE.md  # only after the user approves changes to this existing untracked file
git diff --cached --check
git commit -m "docs: record tier2 memory and reliability results"
```

---

## Expected outcome ranges

- Target: return to the static/Tier-2 path and eliminate the ~2.2 GiB browser fallback.
- Long-lived daemon: up to ~480 MiB less cache retention from the existing 512 -> 32 MiB cap, plus bounded jobs/cache metadata and deterministic child cleanup.
- Thrill/Bluff single-request peak: likely incremental rather than BrowserOxide-level. Shared sources, response bridge, bounded prefetch, and a proven lower heap cap plausibly recover tens of MiB; happy-dom/V8/framework state remains the dominant compatibility cost.
- API-first: likely valuable for JSON/endpoints and selected framework-specific formats; it must prove semantic Markdown equivalence before replacing hydration.
