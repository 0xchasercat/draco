# Changelog

All notable changes to Draco are documented here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/); this project uses SemVer.

## [0.19.0] — 2026-07-12

### Added
- **`extract` — selector-schema structured extraction, everywhere.** The one
  genuine gap an external agent audit found: no way to say "give me all the
  product prices as a JSON array." Now: pass `extract` (a JSON object mapping
  output fields to CSS-selector specs — string shorthand, or
  `{selector, all, attr, fields}` for arrays / attributes / nested objects)
  and the result rides the response as `extract` (+ `extractWarnings`).
  Deterministic and LLM-free by design: Firecrawl's `extract` nests a
  server-side model; Draco's consumers *are* LLMs, so the engine provides the
  precise, reproducible primitive — an agent inspects a page once, derives
  selectors, and every extraction after that is instant and exact. Invalid
  selectors null their field and warn (via the trace as `extract.warning`),
  never fail the scrape; bounded (nesting ≤ 5, 1000 matches, 100 fields);
  URL attributes absolutized. Surfaces: `/v1/scrape`, `/v1/batch/scrape` and
  `/v1/crawl` page options, interact `scrape` + `act`, MCP (`draco_scrape`,
  `draco_interact_scrape`, `draco_interact_act`), CLI `--extract-schema`.
  Tier-independent — works on static scrapes and rendered pages alike.
- **MCP tool parity: `draco_map`, `draco_crawl`, `draco_batch_scrape`.** The
  daemon has had `/v1/map`, `/v1/crawl`, `/v1/batch/scrape` for releases, but
  MCP never advertised them — an MCP-connected agent auditing Draco concluded
  it "can't crawl." Three new transport-independent tools (stdio + daemon):
  `draco_map` (sitemap + on-page link discovery via the same `map_site` core),
  `draco_crawl` (bounded synchronous crawl — seed + mapped same-site links,
  ≤ 25 pages scraped inline under the shared gate, per-page failures inline),
  and `draco_batch_scrape` (the same loop over caller-given URLs). Honest
  scope in the descriptions: large/async jobs stay on the REST job endpoints.

## [0.18.0] — 2026-07-12

### Added
- **`act` — high-fidelity batch interactions for interact sessions.** The
  interaction-fidelity threshold: a bare `.click()` fires an event, but a
  framework-gated handler (React/SvelteKit) often listens for the *pointer
  sequence*, and a modal that mounts from a reactive store update makes **no
  network request** — so the exec settle (fetch-activity quiesce) never saw it.
  `act` closes both gaps:
  - **Faithful event sequences.** Each action dispatches what a real user
    produces: `click` → `scrollIntoView` + `focus` + `pointerover/enter/down`,
    `mousedown`, `pointerup`, `mouseup`, `click` (real bubbling, cancelable,
    composed `MouseEvent`s); `type` → focus + clear + value set + `input` +
    `change`; `press` → `keydown`/`keyup`; plus `scroll`, `select`, `hover`,
    and `wait` (selector-appears or fixed pause). Selector-not-found is a
    structured per-step error, not a throw.
  - **DOM-content-settled pump.** After each action the event loop is pumped
    until the DOM *stops changing* (element-count stable for a quiesce window,
    no loads in flight) or the loop drains — bounded by the capture window —
    so a fetch-less modal mount or client-side route render is captured before
    the next action or the readback.
  - **Batch surface, Firecrawl-shaped.** `actions: [{ "type": "click",
    "selector": "…" }, …]` with `click` / `type` / `press` / `scroll` /
    `select` / `hover` / `wait`. Steps run in order and stop at the first
    failure; the response carries the per-step trace (`steps[]`: action, ok,
    error) *and* the post-action page, so one call shows what the interaction
    did: `POST /v1/interact/{id}/act` (returns the scrape envelope + `ok` /
    `steps` / `logs`), MCP `draco_interact_act` (markdown + rawHtml snapshot),
    and CLI `draco interact --act '<json>'` one-shot / `:act <json>` in the
    REPL (trace + snapshot).
  - **e2e proof.** `interact_act_e2e` mounts a modal from a click listener with
    zero network and asserts the serialized DOM contains it — the exact case
    that raw `.click()` + fetch-quiesce missed.

## [0.17.0] — 2026-07-11

### Added
- **Interact — LLM-driven stateful sessions over the Draco engine, no browser.**
  A resumable Tier 2 isolate an agent drives turn-by-turn: run JS in page scope,
  read the returned value + console, click/type via selectors, and navigate —
  with the network session (cookies) persisted for the whole job. New
  `POST /v1/interact` (open) / `…/{id}/exec` / `…/{id}/navigate` / `…/{id}/scrape`
  / `DELETE …/{id}`, a `draco interact` CLI (one-shot `--exec` + REPL), and MCP
  `draco_interact_open`/`exec`/`navigate`/`scrape`/`close` tools. It's the
  DOM-only analog of a devtools console: querying content, clicking a link, and
  reading the result needs a DOM + JS runtime + a cookie-carrying network stack,
  all of which the engine already has — not a renderer.
  - **Session actor.** A `JsRuntime` is `!Send`, so each session owns a dedicated
    OS thread running the isolate on a current-thread tokio runtime; the `Send`
    `Session` handle drives it over an `mpsc` command channel with per-command
    `oneshot` replies. Between commands the loop keeps pumping the event loop, so
    timers / in-flight fetches armed in one turn resolve before the next. Made
    the one-shot capture path resumable (open → hold → exec/navigate/scrape →
    close) without forking the shipped `run_capture` machinery.
  - **exec value capture.** A turn runs as an async function body (may `await`;
    `return`s a value) and the completion value is serialized page-side under a
    size budget — primitives/arrays/objects pass through, DOM nodes/functions/
    cycles are *described* rather than dropped, an over-budget value becomes a
    `{ "__truncated": true, … }` descriptor unless `full` (with `maxBytes`).
  - **Navigation with cookie persistence.** `navigate(url)` fetches the next
    document through a cookie-aware page fetcher and re-hydrates in the same
    session, so a `Set-Cookie` (login / CSRF / session id) from one page rides to
    the next — one operation-scoped cookie jar shared by the initial fetch,
    script/module loads, page API requests, and every navigation. This is the
    browser-tab behaviour that makes multi-page / login interact flows work.
  - **Daemon session lifecycle.** An in-memory `SessionStore` (idle-TTL + hard
    lifetime cap reaper, concurrency-capped to the isolate pool) holds live
    sessions across requests and closes them all on graceful shutdown. The whole
    interact surface (REST + CLI + MCP) is tier2-gated: a lean
    `--no-default-features` / serve-only build compiles it out cleanly.
  - **Containment unchanged.** The isolate still has no host-capability bindings;
    `exec` runs arbitrary page JS but its only I/O is the fetches the engine
    brokers — resumability does not widen the boundary. Network guardrails
    (mutation-safety, robots) remain a deployment-tier concern.

## [0.16.0] — 2026-07-10

### Added
- **Search — Firecrawl-compatible metasearch over Draco's own stealth HTTP
  stack, no rendering.** New `POST /v1/search`, `draco search <query>`, and MCP
  `draco_search`. Several engines are queried **in parallel** over plain HTTP
  (never the browser/isolate — SERP results are server-rendered HTML, so
  rendering would be pure waste) and merged by **reciprocal-rank consensus**, so
  an engine that captcha-walls, geo-blocks, or rots (Google/Mojeek-class
  failures) is a normal *partial* failure that the surviving engines absorb —
  the request only fails if **every** engine fails. Engine set: DuckDuckGo (HTML
  endpoint, POST), Bing, Brave, Baidu, ZapMeta, Yandex, behind a swappable
  `SearchEngine` trait; each parser is fixture-tested so parser changes need no
  live search. SearXNG is a selector/behavior reference only — not a dependency,
  not a wholesale parser port. Mojeek's parser ships as the canonical
  failure-path fixture (its live endpoint returns an automated-query 403).
  - **Request** (Firecrawl-shaped, unknown fields accepted-and-ignored):
    `query` (required), `limit` (default 5, 1–100), `tbs`, `location`,
    `timeout` (default 60000), `scrapeOptions`, plus Draco `proxy`/`ignoreRobots`
    extensions. **Response**: `{ success, data: [ { title, description, url,
    …scrape fields } ], draco: { engines: [ per-engine status ] } }` — a flat
    array; total engine failure → `502`, partial failure → consensus of
    survivors.
  - **`scrapeOptions.formats`** runs each result URL through Draco's existing
    scrape ladder (bounded concurrency + the daemon gate) and merges the
    `Document` fields (`markdown`/`html`/`rawHtml`/`links`/`json`/`metadata`)
    onto the hit — one `FormatSet`, no parallel schema.
  - Consensus canonicalizes URLs (host-case, default ports, tracking params,
    fragments) to merge duplicates, scores by Σ(1/rank) across engines, and
    keeps per-engine contributor diagnostics in the `draco` extension.
  - SERP fetches ride a per-operation session (a per-engine HTTP budget
    independent of the overall deadline; robots not respected by default, since
    engines disallow `/search` and a metasearch fetches SERPs like a browser).

## [0.15.0] — 2026-07-10

### Added
- **Tier 2 Render mode — pure-CSR SPAs now render their content.** A pure-CSR
  page (e.g. thrill.com) ships an empty shell and paints only after client-side
  data fetches resolve; Tier 2 stubbed those fetches (right for `discover`,
  fast for SSR/hybrid), so `scrape` extracted an empty DOM. The isolate now has
  two fetch modes: **Observe** (unchanged — record + synthetic stub) and
  **Render** — the page's *safe* data requests are fetched live through a new
  `ApiFetcher` seam over the existing `draco_net::replay` path (pooled client +
  shared cookie jar) and the page sees the REAL status/headers/body, including
  non-2xx, so framework routers run their native success/error paths.
  Mutation-safety is reused verbatim from ranking: safe methods and read-style
  POST/PUT go live; streaming endpoints and analytics beacons stay stubbed;
  `--allow-unsafe-replay` is honored. The existing thin/skeleton-shell
  escalation triggers Render automatically (hidden `--force-render` to force
  it). Measured: thrill.com went from a 478-char footer to the full 42,823-char
  lobby (every category, game tile, and provider).
- **Render observability — the isolate is no longer a black box.** Under
  `--runtime-log`, every brokered request logs `[raze.fetch] METHOD URL →
  STATUS (bytes, live|stub(...))`, failed chunk/module loads log
  `[raze.chunk]/[raze.module] MISS`, and the capture window logs why and when
  it closed (`drained|quiesce|hard-cap`, requests captured, loads in flight) —
  all stamped `[+ms]` relative to capture start, so a trace reads as a
  timeline. Log budget 32 → 96 lines (exact repeats deduped). This is what
  turned the remaining thrill/target diagnosis from guesswork into one run.

### Fixed
- **`document.currentScript` points at the real parsed `<script>` node.** The
  shim grafted a synthetic node onto `<head>`, so SvelteKit's
  `currentScript.parentElement` mount target resolved to `<head>` — a non-null
  but *wrong* parent (silent misrender into the document head). The runtime now
  matches the executing block to its real node in the tree (inline source /
  external src identity, WeakSet-claimed); fallback parents to `<body>`, never
  `<head>`. (#14)
- **Web Animations shim — Svelte 5 transitions no longer abort the mount.**
  happy-dom ships no `Element.animate`/`getAnimations`; the TypeError aborted
  the component tree mid-mount (footer rendered, content didn't). Inert,
  completion-biased shim: animations auto-finish, `onfinish` fires even when
  assigned late, `finished` resolves, `cancel()` settles. Real implementations
  win if present.
- **Dynamically injected `<script type="module">` evaluates via `import()`.**
  It previously fell through to happy-dom's disabled loader (`game-loader.js`
  class of failures); module scripts now resolve through the same
  `MapModuleLoader`/`ScriptFetcher` path as the page's own imports, with
  correct ES semantics and concurrency.
- **Completion-biased IntersectionObserver + ResizeObserver.** The snapshot
  stubs were inert no-ops, so sections that lazy-load on scroll-into-view never
  learned they were visible and stayed skeletons. Observed elements now report
  fully-intersecting/measured once on the next tick — the correct bias for a
  renderless extractor; window + `max_intercepts` still bound infinite scroll.
- **The document lifecycle fires.** happy-dom never runs
  `readystatechange`/`DOMContentLoaded`/`load` for the `document.write` +
  manual-eval path, and SvelteKit's boot gates its data phase on them. The
  capture loop now fires the full sequence at the parsing-finished moment
  (browser-faithful: before dynamic `import()`s settle), with
  `document.readyState` shadowed `"loading"` → `"complete"`.
- **`discover`'s `replayable` flag matches what replay actually does.** The
  catalog screened on safe methods only, while `best_replayable` also admits
  read-style POST/PUT (GraphQL/JSON-RPC) — thrill's `POST /tickets` was listed
  `replayable: false` yet was replayed. One eligibility rule now feeds both.
- **`--json` stdout is clean.** The vendored timer/MessagePort error handlers
  printed page-side errors (e.g. a `performance.timing` probe) to **stdout**,
  corrupting the JSON envelope (`jq` parse errors). They now land in the
  bounded `runtime.log` channel via `op_raze_log` (stderr as fallback where
  ops aren't registered). Errors never touch stdout.

### Changed
- **Mode-aware capture window.** One 2.5 s clamp served both modes — and sat
  *below* the 2.5 s subresource-fetch timeout, so a route chunk requested late
  in a cold-isolate hydration was abandoned before it could finish; thrill's
  game grid (a TanStack Query inside a lazily-imported route component) never
  mounted. Observe keeps the tight cap (discovery's ranked calls fire early);
  Render gets an 8 s floor / 15 s ceiling — a *ceiling*, not a wait: quiesce
  still ends the window as soon as the page goes idle.
- **Content-activity quiesce — trackers can't pin the window open.** Timeline
  traces showed target.com's content done at +1.8 s while FullStory (257 KB),
  DoubleVerify, googlesyndication, Attentive and Medallia kept the window open
  to +4.0 s. A curated `is_tracker()` denylist (analytics / session-replay /
  ads / bot-detection vendors) now classifies requests: trackers are still
  recorded (discover unaffected) and still fetched live (the page behaves
  normally) but no longer count as progress or hold the window. Every render
  now closes deterministically at [last content fetch + quiesce]. Measured:
  target.com render tier 3999 ms → 2348 ms (−41%) with identical content;
  thrill (a real data dependency chain) unaffected.

## [0.14.0] — 2026-07-09

### Changed
- **In-process async engine — the OS process jail is deleted.** Amputate,
  don't band-aid: the fork/exec jail with its per-chunk blocking IPC was the
  root cause of Tier 2's serialized, budget-strangled chunk loading, and no
  amount of prewarming could disguise it. V8 now runs **in-process** on a
  current-thread tokio runtime behind an async `ScriptFetcher` seam:
  `op_raze_load_script` is a real async op and the module loader awaits the
  fetcher, so `import()`/chunk loads fan out **concurrently** on the event
  loop exactly like a browser's network stack. Initial external `<script src>`
  are fetched concurrently and executed in document order; an in-flight-load
  counter keeps the capture window from quiescing mid-load. Containment is the
  isolate itself — page JS has **no host-capability bindings** (no network, no
  filesystem, no processes; the only I/O it can cause is the fetches the
  engine explicitly brokers) — plus the hosted-cloud infrastructure perimeter.
  Deleted: the `draco-jail` crate (9 files), `prewarm.rs`, and the
  prefetch/import-graph machinery; `--no-jail`/`--strict-sandbox` remain as
  inert CLI no-ops. `Tier2Pool` is now a semaphore bounding concurrent
  isolates.

### Added
- **Process-global immutable chunk cache.** 512 MiB RAM LRU + 2 GiB disk
  (`~/.cache/draco/chunks`), collision-safe and never worse than a miss —
  hashed/immutable SPA chunks are fetched once across scrapes; data responses
  are deliberately never cached.

## [0.13.15] — 2026-07-08

### Changed
- **JIT enabled — SPA hydration is no longer throttled by an app-layer safety
  flag.** The Tier 2 V8 isolate ran with `--jitless`. But real SPA hydration is
  hot JS (React/SvelteKit reconcilers), and jitless ran it 3–10× slower — slow
  enough that a page's boot exceeded the 4 s on-demand chunk budget, so the
  chunk it needed was refused, hydration threw, and a scrape burned 4–5 s to
  return *empty* content (measured: bluff.com 10.3 s→208 chars, thrill.com
  5.1 s→0 chars). Draco's isolation is solved at the infrastructure layer
  (stateless, ephemeral, unidirectional workers) and, in-process, by an isolate
  with **no host-capability bindings** — page JS cannot do I/O regardless of
  JIT. So JIT is now ON (`--single-threaded` kept, so V8 spawns no background
  threads; JIT runs synchronously on the main thread).
- **draco-jail: W^X guard lifted so JIT can map executable code.** The seccomp
  denylist no longer kills `mprotect`/`pkey_mprotect` with `PROT_EXEC`, and the
  strict allowlist now permits them. The real containment is unchanged: the
  network air-gap (killed `socket`/`connect`), no `execve`/`ptrace`, and
  Landlock FS lockdown all stay. (Strict mode may need the usual bare-metal
  syscall tuning for JIT; the default denylist needs none.)

## [0.13.14] — 2026-07-08

### Fixed
- **On-demand chunk loading no longer pays N sequential round-trips.** When a
  Tier 2 SPA pulls a code-split chunk the up-front prefetch didn't cover, the
  isolate requests it over a synchronous `LoadScript` IPC frame that the
  supervisor services one at a time (the isolate's `op_raze_load_script` is a
  blocking op on the single V8 thread) — so a page needing *N* on-demand chunks
  paid *N* back-to-back ~250 ms fetches. A browser doesn't: its network stack is
  async, so `import()`s fan out concurrently. A new per-job **prewarmer**
  (`draco-core` `prewarm.rs`) restores that without letting the air-gapped
  isolate touch the network: when a requested chunk lands, its dependency
  closure — static ES-module imports plus webpack/Next chunk-loader candidates
  parsed from its body — is fetched **concurrently in the background** into a
  per-job cache on a small multi-thread runtime, so the child's next
  `LoadScript` resolves from the warm cache in ~0 ms. Fetches carry the job's
  shared cookie jar (v0.13.13), so Cloudflare's `__cf_bm` is reused. Best-effort
  and bounded (file-count + total-byte caps, the cache doubling as the visited
  set); a cold miss falls back to a direct fetch — never worse than before.

### Added
- `scripts/gate.sh` — the sandbox-safe way to run the CI gates. It refuses to
  *start* a build below a free-space floor (auto-reclaiming first), pins
  `CARGO_BUILD_JOBS=1` for the ~4 GiB box, and prints free space between phases —
  turning the recurring silent 0-byte `ENOSPC` deadlock (at which point even
  `rm` can no longer run and the whole session is lost) into a loud early abort
  with room to act.
- `scripts/reclaim.sh` — one-shot, near-full-safe reclaim of the only large
  *regenerable* consumers (the `target/` tree and cargo registry caches);
  source, git history, and credentials are untouched.
- `docs/SANDBOX.md` — the constrained-sandbox runbook: why disk-full loses the
  whole session, the commit-before-build rule, and how to use the scripts.

### Changed
- Slimmed the dev/test build footprint: `[profile.dev]`/`[profile.test]` now
  compile with `debug = "line-tables-only"` and dependencies with
  `debug = false`. Full DWARF for a workspace that links V8/deno_core/oxc/
  BoringSSL across every `--all-targets` test binary was several GiB of
  `target/` — the dominant cause of the sandbox disk-full loss. Panic/backtrace
  `file:line` is preserved for our own crates; fully reversible per-dev.

## [0.13.13] — 2026-07-07

### Fixed
- **Cookies now persist across a whole operation, not per network call** — the
  root cause of stake.com's intermittent missing chunk. `draco-net` created a
  **fresh, empty cookie jar on every `fetch_target`/`replay` call**, so a
  `Set-Cookie` on the initial page response was discarded before the next
  request. On a Cloudflare-fronted site the page fetch is issued a `__cf_bm`
  bot-management cookie; with it thrown away, every prefetch and on-demand chunk
  request went out cookie-less, so Cloudflare throttled/challenged a rotating
  subset of them — a different chunk failed to load each run (confirmed
  fetchable-with-cookies: the chunk 200s under `curl --impersonate` and carries
  `set-cookie: __cf_bm=…`). A browser scopes cookies to the *page session* and
  replays them on every subresource; draco now does the same.
- New `SharedCookieJar` on `SessionOpts::cookie_jar`: a jar scoped to one logical
  operation (a scrape/discover, and — ahead — a batch/crawl or `/interact`
  session) and shared across its page fetch, every prefetched + on-demand
  subresource, and the replay. `run_ladder` creates one per job; a batch caller
  can pre-set it to share one session across many pages. Isolation is now
  **between** operations, not within one (verified: a cookie set on one call is
  replayed on the next call in the same op, and separate jars stay isolated).
  When no jar is supplied — a genuinely one-shot fetch — a throwaway per-call jar
  is used, so nothing regresses. This is also the cookie half of the
  `SessionState` the planned `/interact` endpoint threads across turns.

## [0.13.12] — 2026-07-07

### Fixed
- **Streaming endpoints (SSE / WebSocket) are no longer replayed** — the fix that
  turned stake.com's 43-second `discover` back into a fast one. Once the 0.13.11
  `EventSource` stub started surfacing SSE endpoints, discovery correctly found
  `/_api/feature-flag/v1/flags/stream` (score 18, a safe same-origin GET) and
  picked it as the replay winner — but an SSE stream never closes, so replaying
  it to capture a sample body hung for the full 30 s session timeout (43 s total
  run). Ranking now treats any endpoint with `Accept: text/event-stream` (SSE) or
  a `ws(s)://` URL (WebSocket) as **non-replayable**: it is still reported as a
  discovered endpoint, it just isn't fetched for a body. Both `best_replayable`
  (the replay winner) and the per-endpoint `replayable` output flag honor this.
- **`response.body.getReader()` no longer aborts hydration.** The fetch-response
  stub had no `body`, so streaming-response readers (SvelteKit data loaders,
  fetch-based SSE) threw `Cannot read properties of undefined (reading
  'getReader')`. The response now exposes a minimal already-closed
  `ReadableStream` stand-in whose `getReader().read()` resolves `{done:true}` at
  once — the reader loop ends cleanly with no data (the isolate is air-gapped),
  never throwing. One-shot `text()`/`json()` bodies are unchanged.

## [0.13.11] — 2026-07-07

### Fixed
- **`EventSource` / `WebSocket` no longer abort hydration** — and now surface as
  discovered endpoints. SPAs commonly open a streaming connection during init;
  stake.com's app bootstrap does `new EventSource(...)`, and a bare reference to
  a missing constructor is a `ReferenceError` that kills hydration before any
  data fetch runs (same failure class as the 0.13.8 `SVGAElement` and 0.13.5
  Performance backfills). happy-dom ships no `EventSource` and only a
  non-functional `WebSocket` stub that throws "ws does not work in the browser".
  The glue now installs no-op stubs for both that never throw and **record the
  connection URL as an intercepted request** — an SSE/WebSocket endpoint is
  precisely the API surface `discover` exists to find. Installed unconditionally
  (overriding happy-dom's broken `WebSocket`), same posture as the `fetch`/XHR
  interceptors.

### Changed
- **Prefetch no longer chases dynamic `import()` route bundles.** v0.13.10 fetched
  the static critical graph first but still walked dynamic imports afterward,
  spending the budget on lazy route bundles — on stake.com, **12.9 MB** of them
  versus a **548 KB** critical graph, and under Cloudflare's per-fetch throttling
  that starved the critical graph itself (a 4130ms on-demand-budget miss on a
  *static* entry dep). The prefetch walk now fetches only the **static** module
  graph (the eager `import`/`export … from` closure — precisely what hydration
  needs) plus webpack/Next chunk-loader candidates; dynamic `import("…")` targets
  are left to the on-demand `LoadScript` path, which fetches any lazy chunk
  hydration actually reaches. `discover`'s capture window never navigates, so
  route bundles never come due — skipping them removes megabytes of wasted
  fetching and keeps the critical graph inside budget even under CDN throttling.
  (Oxc's `requested_modules` is static-only and `dynamic_imports` separate, so the
  static/dynamic split from the one parse is exact; the webpack chunk-candidate
  regex does not match Vite's relative `../chunks/` form, so Vite route bundles
  are correctly excluded.)

## [0.13.10] — 2026-07-07

### Changed
- **Prefetch prioritizes the critical hydration path.** The supervisor's
  script-graph prefetch walk (`prefetch_scripts_with_budget`) now fetches a
  page's **static** module graph — the eager `import`/`export … from` closure
  that hydration cannot proceed without — strictly ahead of **lazy** targets
  (dynamic `import("…")` and webpack/Next chunk-loader references, which the app
  pulls on demand, usually for routes). Previously both were enqueued into one
  FIFO frontier, so on a large code-split SPA the file/byte/wall budget could be
  spent fetching lazy route chunks while a critical dependency of the entry went
  unfetched — it then fell to the on-demand `LoadScript` path, whose own budget
  could expire mid-hydration, stalling the page at **0 endpoints**. This was the
  stake.com `discover` failure: v0.13.9's `--runtime-log` pinned it to
  `on-demand load budget exhausted … chunks/BBKH46HI.js`, a *static* dep of the
  SvelteKit entry. The walk now uses a two-tier frontier (`critical` drained
  fully before `lazy`, `visited` shared) so the eager graph is fetched up-front
  and in parallel — on-demand loading becomes the rare fallback it was meant to
  be, not the mechanism the critical path depends on. Helps every heavy SPA, not
  just stake. Import extraction is split accordingly (`extract_imports` returns
  static vs dynamic specifiers from the one Oxc parse). No API or wire-protocol
  change; budgets and caps are unchanged (this spends them better).

## [0.13.9] — 2026-07-07

### Added
- **On-demand chunk-load failures now explain themselves** under `--runtime-log`.
  When the runtime's module loader (v0.13.8) asks the supervisor for a chunk that
  wasn't prefetched and the supervisor can't produce it, the reason was collapsed
  to a bare "missing" — indistinguishable between *budget exhausted*, an HTTP
  status (a `403` bot-wall or `404`), and a network/timeout error. The supervisor
  now records the specific cause as a bounded `runtime.log` line
  (`[loadscript] HTTP 403: <url>`, `[loadscript] on-demand load budget exhausted
  (…ms ≥ 4000ms); not fetched: <url>`, `[loadscript] fetch error: …: <url>`),
  correlated by URL to the runtime's own import rejection. A SvelteKit/Vite SPA
  that hydrates to 0 endpoints on a missing chunk now says *why* — the difference
  between "raise the dynamic-load budget", "the CDN is challenging our fetch"
  (needs the future browser fallback), and "the chunk URL is wrong" — instead of
  leaving it to guesswork. `fetch_dynamic_script` returns the reason rather than
  swallowing it to `None`; successful loads stay silent. Diagnostic only — no
  change to capture control flow or the jail wire protocol.

## [0.13.8] — 2026-07-07

### Fixed
- **ESM module loading is honest instead of silently empty** — the single biggest
  cause of SvelteKit/Vite SPAs discovering **0 endpoints** (observed on stake.com
  and chaser.sh). The in-isolate module loader previously served an **empty
  module** for any `import` / `import()` whose URL was missing from the
  supervisor's prefetch set (`unwrap_or_default()`). An empty module satisfies the
  *load* but then fails V8 *linking* with a phantom
  `SyntaxError: … does not provide an export named 'x'`, aborting hydration far
  from the real cause and blaming the page's own code. Draco's static prefetch
  scanner cannot always discover every chunk URL — *minified* static imports
  (`import{x}from"../chunks/HASH.js"`, zero spaces) and Vite's runtime dep-map are
  common misses — so this fired constantly on real apps. The loader now:
  1. serves prefetched subresources (the common path);
  2. on a miss, fetches the chunk **on demand** through the same supervisor loader
     `op_raze_load_script` already uses (`draco-net`), caching it back; and
  3. only if a module is genuinely unreachable, **rejects that import with a real
     load error** — a browser rejects a 404'd chunk the same way, and sibling
     scripts plus already-scheduled fetches still surface.
  Module loads now resolve via `ModuleLoadResponse::Async`, so the graph is pulled
  and registered on the event loop rather than compiled synchronously inside V8's
  dynamic-import host callback. Verified against the real chaser.sh chunk graph
  (56 chunks): with the `chunks/` directory withheld from the prefetch map — the
  exact field state — hydration now recovers every chunk on demand and captures
  the endpoint, where before it produced the phantom export error and 0 endpoints.
- **Backfill DOM element constructors happy-dom omits.** Frameworks reference DOM
  constructors as **bare globals** in `instanceof` / `typeof` guards; a bare
  reference to an undefined identifier is a `ReferenceError` (not `undefined`), so
  one missing constructor in a hot path aborts the surrounding code. happy-dom
  ships 69 `SVG*Element` classes but not `SVGAElement` (SVG `<a>`); SvelteKit's
  link router runs `el instanceof SVGAElement ? el.href.baseVal : el.href` during
  hydration and threw `SVGAElement is not defined`, killing navigation wiring
  (seen on chaser.sh even once all chunks were available). The glue now backfills
  such missing constructors as subclasses of the nearest base happy-dom *does*
  provide, so the guard evaluates (to `false` for the common HTML element) instead
  of throwing. Same class of fix as the 0.13.5 Performance API and 0.13.7
  window/self shims. Never clobbers a constructor happy-dom already exposes.

## [0.13.7] — 2026-07-06

### Fixed
- **Unify `window === self === globalThis === top === parent === frames`** in the
  Tier 2 isolate. The glue pointed the page-facing `window` at happy-dom's Window
  *instance* while `self`/`globalThis` stayed the V8 global, so `window !== self`.
  Any library that writes one global alias and reads another broke — Next.js is the
  canonical victim: it writes `window.__NEXT_DATA__` (client/index.js) but the
  Router reads `self.__NEXT_DATA__.gssp` (router.js) → `undefined` → throw,
  aborting hydration before any data fetch (bluff.com and any Next.js pages-router
  SPA hydrated to 0 endpoints). All aliases now point at the one V8 global, matching
  the browser top-level contract; the DOM stays coherent because the DOM globals are
  mirrored onto it and happy-dom references its own window via a private Symbol.

## [0.13.6] — 2026-07-06

### Fixed
- **Tier 2 latency budgets + honest trace attribution.** `prefetch_scripts` fetched
  up to 64 chunk candidates **sequentially** (30 s timeout each) inside the
  `runtime.capture` timer, so a slow/hung subresource could blow the 2.5 s discover
  cap to tens of seconds while attributing the time to the wrong stage. Prefetch is
  now parallel with bounded concurrency, a wall-clock budget, and a per-fetch
  timeout, surfaced as a `runtime.prefetch` trace step; a per-job dynamic
  script-load budget bounds on-demand chunk loading too. New tunables:
  `PREFETCH_CONCURRENCY`, `PREFETCH_WALL_BUDGET_MS`, `SUBRESOURCE_FETCH_TIMEOUT_MS`,
  `DYNAMIC_LOAD_BUDGET_MS`.

### Added
- **`--runtime-log` diagnostics.** Glue-swallowed exceptions/rejections,
  `console.error`/`console.warn`, and page-script throws are collected (count- and
  length-bounded) and surfaced as `runtime.log` trace steps on `scrape`/`discover`,
  with daemon `runtimeLog` field and MCP parity — so a page that fails to hydrate
  can be self-diagnosed without a browser devtools. (This is the flag that surfaced
  the 0.13.7 and 0.13.8 root causes.)

### Changed
- A markdown-only scrape that produced empty markdown now falls back to the JSON
  envelope instead of printing a blank line.

## [0.13.5] — 2026-07-06

### Fixed
- **Performance API compatibility** for telemetry/web-vitals chunks. The runtime now
  installs safe no-op implementations for `performance.getEntriesByType`,
  `getEntriesByName`, `getEntries`, `mark`, `measure`, resource-timing buffer
  methods, and `PerformanceObserver`, preventing Sentry/web-vitals setup code from
  aborting Next.js hydration before application fetches run.

## [0.13.4] — 2026-07-06

### Fixed
- **Dynamic script loading is now supervisor-mediated** for chunks that are not in
  the initial prefetch set. When page code appends a `<script src>` chunk, the
  air-gapped isolate asks the supervisor over IPC; the supervisor fetches the raw
  JS with `draco-net`, returns the source bytes, and the runtime evaluates them and
  fires `onload`. This preserves the jail's no-network invariant while covering
  code-split Next/Webpack chunks whose URLs are computed too dynamically for static
  prefetch heuristics.
- **Tier 2 capture windows are capped at 2500 ms** at the supervisor boundary so a
  bad hydration path fails fast instead of spending tens of seconds before falling
  back.

## [0.13.3] — 2026-07-06

### Fixed
- **Next.js / webpack chunk prefetching** now handles the common split-map runtime
  shape where one numeric map gives a chunk basename and another gives the content
  hash. Short hex basenames such as `7722f4ca` are no longer mistaken for hashes,
  so Draco can prefetch and execute chunks like
  `_next/static/chunks/7722f4ca.78bc63657a3a3377.js` before the page's loader
  reports `ChunkLoadError`.

## [0.13.2] — 2026-07-06

### Fixed
- **Next.js / webpack dynamic chunk loading** now works in the air-gapped runtime.
  Draco executes prefetched `<script src>` chunks when page code appends or inserts
  script nodes, instead of letting happy-dom's disabled file loader fail them with
  `ChunkLoadError`. This restores endpoint discovery for pages whose API calls live
  behind lazy chunks.
- **Chunk prefetching** now also follows likely webpack/Next chunk-loader references
  inside fetched bundles (direct `_next/static/chunks/*.js` literals and chunk
  id/name/hash maps), so dynamically appended chunks are available to the runtime
  without giving the isolate network access.

## [0.13.1] — 2026-07-06

### Fixed
- **`draco discover` hardened Linux jail boot** no longer dies before returning a
  result. The default seccomp denylist now allows `clone3` (modern libc may use it
  for ordinary thread creation) while still blocking `execve`/`execveat`, and the
  child address-space limit now permits current deno_core/V8 Oilpan virtual cage
  reservations. The W^X `mprotect(PROT_EXEC)` guard remains in place.
- **Classic inline SvelteKit/Vite boot scripts** now resolve dynamic
  `import("./chunk.js")` against the page URL instead of a synthetic non-base
  `draco:` script name. This fixes the `invalid URL: relative URL with a
  cannot-be-a-base base` rejection observed on macOS and lets the module graph run.
- **Module graph prefetching** now seeds from JS preload hints (`modulepreload` and
  `preload as=script`) and inline-script string-literal imports, not only
  `<script src>`, so SvelteKit-style entry modules are available to the air-gapped
  runtime.

## [0.13.0] — 2026-07-06

### Added
- **Webhooks** for crawl and batch scrape. A request may carry a `webhook` — a
  bare URL string or `{ url, headers, metadata, events }` — and the job fires
  Firecrawl's four lifecycle events: `started`, `page` (with the scraped
  `Document`), `completed`, and `failed`. The `events` filter uses bare names;
  the delivered payload's `type` is prefixed by job kind (`crawl.page`,
  `batch_scrape.completed`, …). Payloads are
  `{ success, type, id, data, metadata }`, `metadata` echoing the config's.
  - Delivery is fire-and-forget (a slow/dead endpoint never stalls the job),
    `POST`ed as JSON via the pooled client with a 10s deadline, retried at
    +1min / +5min / +15min on any non-2xx or transport error, then dropped.
    Custom `headers` are sent on every attempt; the endpoint is never
    robots-gated.

## [0.12.1] — 2026-07-06

### Fixed
- **`robotsBlocked` is now populated** on the crawl/batch `/errors` endpoints,
  matching Firecrawl (which lists URLs skipped by `robots.txt` separately from
  `errors`). v0.12.0 always returned `[]` and folded robots denials into
  `errors` — that was not parity. draco-net now signals a robots denial as a
  distinct `NetKind::Robots` (was an indistinguishable `NetKind::Status`), and
  the crawl/batch workers route those URLs to `robotsBlocked` instead of
  `errors`.

## [0.12.0] — 2026-07-06

### Added
- **Batch scrape** — `POST /v1/batch/scrape` takes a list of URLs and runs each
  through the full extraction ladder as one async job. Scrape options are **flat**
  at the top level (matching Firecrawl's batch request, unlike crawl's nested
  `scrapeOptions`): `formats`, `onlyMainContent`, `includeTags`/`excludeTags`,
  `headers`, `waitFor`, plus the Draco extensions. `ignoreInvalidURLs` drops
  non-http(s) URLs and reports them in `invalidURLs` instead of failing the whole
  request. Parallelism is bounded by the daemon-wide `--max-concurrency` gate, so
  a large batch saturates exactly the configured budget.
  - `GET /v1/batch/scrape/{id}` — status + accumulated `data`, **paginated** via
    `?skip=&limit=` with a `next` URL when the job is still running or the page
    hit the 10 MiB serialized-size cap.
  - `GET /v1/batch/scrape/{id}/errors` — `{ errors, robotsBlocked }`.
  - `DELETE /v1/batch/scrape/{id}` — cancel (keeps data already gathered).
- **Crawl status parity** — `GET /v1/crawl/{id}` now supports the same
  `?skip=&limit=` pagination + `next`, `creditsUsed`, and `expiresAt` fields, and
  gains a **`GET /v1/crawl/{id}/errors`** endpoint. Failed crawl pages now record
  their URL + error for that endpoint.

### Changed
- The async-job registry (`JobStore`) is extracted into a shared `serve::jobs`
  module used by both crawl and batch scrape — one lifecycle, one status shape,
  one pagination implementation instead of two.

_Webhooks (the crawl/batch event lifecycle) are the next release; batch lands
first because webhooks hook into it._

## [0.11.1] — 2026-07-06

### Added
- **`includeTags` / `excludeTags`** are now honored (previously accepted and
  ignored). `excludeTags` drops matching elements before extraction; `includeTags`
  restricts to matching subtrees. Applied to the `markdown` and `html` formats
  (before `onlyMainContent`); `rawHtml` stays the raw fetch and `links` is
  harvested from the full page. Available on `POST /v1/scrape`, `draco scrape`
  (`--include-tag` / `--exclude-tag`, repeatable), the `draco_scrape` MCP tool,
  and per-page in `POST /v1/crawl`'s `scrapeOptions`.
- **`headers`** (custom request headers) are now honored: forwarded to every
  outbound fetch (initial request, retries, robots probe). Available on the same
  four surfaces (`draco scrape --header "Name: Value"`, repeatable).

This completes the request-field parity started in v0.11.0 (`onlyMainContent`,
`waitFor`).

## [0.11.0] — 2026-07-06

### Added
- **Content formats `html`, `rawHtml`, `links`** — the scrape surface is now a
  true multi-select `formats` set, not a three-way choice. `html` is the cleaned,
  absolutized main-content HTML (the same DOM pre-processing that feeds the
  Markdown transform); `rawHtml` is the unmodified fetched body; `links` is every
  absolutized `<a href>` on the page. All compose with `markdown`/`json`/
  `endpoints` in one request. On a client-rendered page, `html`/`links` reflect
  the hydrated DOM when the render escalation wins (`rawHtml` stays the raw fetch).
- **`onlyMainContent` and `waitFor`** are now honored (previously accepted and
  ignored). `onlyMainContent` (default `true`) toggles boilerplate stripping for
  `markdown`/`html`; `waitFor` is a Firecrawl-style alias for the Tier 2 capture
  window (`captureWindowMs` wins if both are given).
- **CLI verbs mirror the REST/MCP surface**: `draco discover <url>` (≙
  `POST /v1/discover`) and `draco map <url>` (≙ `POST /v1/map`) join `draco scrape`.

### Changed
- **`draco extract` is renamed to `draco scrape`** to match `POST /v1/scrape` —
  point-of-parity with Firecrawl clients. The old `extract` verb is **removed**
  (no alias). `--format` is now **repeatable** and accepts `markdown`, `html`,
  `raw-html`, `links`, `json`, `endpoints` (plus `both` as `markdown`+`json`).
- **Crawl `includePaths`/`excludePaths` are now regex** matched against the URL
  pathname (Firecrawl semantics), with `regexOnFullURL` to match the full URL —
  previously they were case-sensitive substring matches (a drop-in-compat bug).
- **Map** now discovers sitemaps from `robots.txt` `Sitemap:` directives, honors
  `ignoreSitemap`/`sitemapOnly`, defaults `includeSubdomains` to `true`, and
  raises the `limit` cap to 100000 — Firecrawl parity.
- Formats the DOM-only engine cannot produce (`screenshot`, `screenshot@fullPage`,
  `actions`, and the not-yet-implemented `extract`/`changeTracking`/`summary`/
  `branding`/`product`/`menu`) now return a clear **HTTP 422** ("needs a browser")
  rather than a generic 400 — an unrecognized token is still a 400. Unknown request
  fields remain accepted-and-ignored for drop-in compatibility.

### Fixed
- The lean (`--no-default-features`) build no longer fails to compile: the Tier 2
  `try_tier2` lacked its `#[cfg(feature = "tier2")]` gate and collided with the
  lean stub. (Pre-existing since ≤v0.10.0; masked because gates only built the
  default feature set.)

## [0.10.0] — 2026-07-06

### Added
- **API discovery/replay** — surface the JSON/XHR API endpoints a
  client-rendered page's own JavaScript calls, ranked, as a first-class result.
  The Tier 2 isolate already intercepts and scores every `fetch`/XHR to pick a
  replay winner; discovery exposes the *full ranked catalog* (`method`, `url`,
  `via`, `score`, `replayable`, `headers`) so a caller can see — and replay —
  any endpoint behind a SPA, not just the auto-picked one.
  - **CLI:** `draco extract <url> --format endpoints` prints the catalog (plus
    the replayed winner as `data`) in the envelope.
  - **Daemon:** `POST /v1/scrape` with `formats: ["endpoints"]` returns
    `data.endpoints` (composes with `markdown`/`json`); a dedicated
    **`POST /v1/discover`** convenience route returns `{ success, endpoints,
    data }` top-level, mirroring `/v1/map`'s shape.
  - **MCP:** a new **`draco_discover`** tool alongside `draco_scrape`, for agent
    clients that want a page's data API rather than its rendered text.
  - Result contract: `ExtractionResult.endpoints` (additive, elided when
    discovery didn't run); `draco-core` gains `Config::discover_endpoints` and
    the `DiscoveredEndpoint` type. Discovery runs its own Tier-2 capture ahead
    of the Tier 0/1 early-returns; capped below Tier 2 it records a skip and
    returns no catalog. Ranked best-guess-data-API first; `replayable` reflects
    the same viability + mutation-safety policy the winner replay uses.

## [0.9.0] — 2026-07-06

### Added
- **Warm Tier 2 isolate pool for the daemon.** `draco serve` now keeps a set of
  jailed capture workers alive and idle between scrapes, so each Tier 2 request
  skips the per-scrape fork+exec + sandbox arming (unprivileged userns / netns /
  seccomp-BPF / Landlock) + first snapshot cost instead of paying it every time.
  - Each job still runs in a **fresh snapshot-restored isolate** inside a reused
    worker process (the jailed child now loops over `Hydrate` jobs rather than
    exiting after one), so there is **no cross-scrape state, cookie, or DOM
    bleed** — only the expensive process + sandbox are reused, never the isolate.
  - Workers are recycled after `--isolate-max-jobs` captures (default 100, leak
    hygiene) and dropped (never reused) on any IPC error; the pool retires idle
    workers on graceful shutdown.
  - New flags: `--isolate-pool-size` (default `0` = auto, ≈ CPU count, which
    also caps concurrent isolates) and `--isolate-max-jobs`. A request whose
    sandbox posture (`noJail` / strict) differs from the pool's transparently
    falls back to a one-shot spawn.
  - `/v1/scrape`, `/v1/crawl`, and `POST /mcp` route through the pool; the CLI
    (`draco extract`) and stdio MCP keep the one-shot path.
  - New `draco-core` API: `extract_with_pool` + the `Tier2Pool` type (a
    finalizes-`Unsupported` stub in the lean, non-`tier2` build).

  Measured note: in the `--no-jail` sandbox the warm win is modest (~15%, only
  fork+exec + V8 platform init amortized); the larger payoff is on bare-metal
  jailed hosts where the pool also amortizes the userns/netns/seccomp/Landlock
  arming that dominates a cold jailed spawn. Each job still pays its own snapshot
  restore + capture window (the price of a guaranteed-fresh isolate).

## [0.8.1] — 2026-07-06

### Changed
- **`draco-net` now shares one pooled HTTP client process-wide instead of
  building a fresh client per fetch.** Previously every `fetch_target` /
  `replay` call constructed a new `wreq::Client` — discarding the keep-alive /
  HTTP-2 connection pool and recompiling the BoringSSL connector + emulation
  profile on every request (measured ~263 µs of pure build cost per fetch, plus
  a fresh TCP+TLS handshake since no connection could be reused). The client is
  now built once per proxy and cached (`Client` is `Arc`-backed, so clones
  share the pool), giving connection reuse across requests in the `draco serve`
  daemon **and** across the several same-host fetches within one extraction
  (page + script subresources + replay). Warm client acquisition drops to
  ~667 ns (~395× cheaper), before counting the saved TLS handshakes.
- **Cookie isolation is preserved exactly.** A shared client must not let one
  call's cookies bleed into another, so the cookie jar moved from the client to
  a fresh per-call `Jar` attached via wreq's per-request `cookie_provider`; the
  total request timeout likewise moved to a per-request setting (so the shared
  client isn't fragmented by timeout). Cookies still flow across a redirect
  chain within a single call but never leak between calls — covered by new
  integration tests (`cookies_do_not_leak_between_calls`,
  `cookie_flows_across_redirect_within_one_call`).

  Note: this is a latency/efficiency fix that helps the daemon most. It does
  not change the daemon-vs-CLI gap under concurrent load, which is CPU
  contention between simultaneous Tier-2 hydrations (a warm isolate pool /
  `--max-concurrency` tuning is the lever there).

## [0.8.0] — 2026-07-06

### Added
- **`POST /v1/map`** — Firecrawl-compatible site URL discovery. Merges the
  site's `/sitemap.xml` (following a sitemap index one level, bounded) with
  the target page's own `href` links, resolved and same-host filtered
  (`includeSubdomains` opt-in), fragment-stripped, order-preserving deduped,
  filtered by case-insensitive `search`, truncated to `limit` (default 5000).
  An HTTP-error page is treated as unfetched rather than harvesting an error
  template's links; `502` only when neither source is reachable.
- **`POST /v1/crawl` + `GET|DELETE /v1/crawl/{id}`** — Firecrawl-compatible
  async crawl jobs. Bounded BFS (default `limit` 10, hard cap 100; `maxDepth`
  default 2) where every page runs the full extraction ladder, so crawled
  SPAs hydrate exactly like single scrapes. Frontier links are harvested from
  each page's *Markdown* (Draco absolutizes links, so no second fetch — and
  JS-injected links come free when the render escalation ran). `includePaths`
  / `excludePaths` are substring matches on the URL path (exclude wins);
  same-host unless `allowExternalLinks`; `scrapeOptions.formats` selects
  markdown/json per page. Pages share the daemon-wide `--max-concurrency`
  budget; jobs are in-memory (no external queue), sequential within a job.
- **MCP server** — Draco's scraping as Model Context Protocol tools:
  - `draco mcp`: stdio transport (newline-delimited JSON-RPC 2.0), for agent
    clients like Claude. stdout carries protocol messages only.
  - `POST /mcp` on the daemon: minimal Streamable-HTTP subset (single-message
    POST → single JSON response; `202` for notifications; no SSE/session).
  - Protocol: initialize with version negotiation (2025-06-18 / 2025-03-26 /
    2024-11-05), `tools/list`, `tools/call`, `ping`; tool-level failures are
    `isError: true` results, protocol misuse is a JSON-RPC error; lifecycle is
    tolerated but not enforced (uniform with the stateless HTTP binding).
  - One tool: `draco_scrape` (`url`, `formats`, `tierMax`, `captureWindowMs`,
    `timeout`, `ignoreRobots`), annotated read-only/open-world.

## [0.7.0] — 2026-07-06

### Added
- **Daemon mode: `draco serve`** — a persistent HTTP daemon exposing a
  **Firecrawl-compatible REST API** (axum), so the process stays warm (no
  per-scrape binary spawn) and existing Firecrawl clients can point at Draco
  unchanged.
  - `POST /v1/scrape`: Firecrawl request shape (`url`, `formats`, `timeout`;
    unknown fields like `onlyMainContent` / `waitFor` accepted and ignored) →
    Firecrawl response shape (`{ success, data: { markdown?, json?, metadata } }`,
    `{ success: false, error }` on failure). `formats` supports `"markdown"`
    (default) and `"json"` (Draco's tiered JSON-API extraction under
    `data.json`); recognized-but-unsupported formats (`html`, `rawHtml`,
    `links`, `screenshot`, …) are rejected with a clear `400` instead of being
    silently dropped.
  - HTTP status mapping: `200` success · `400` bad request · `502`
    upstream/network failure · `422` unsupported target / needs-browser ·
    `503` shutting down.
  - Draco per-request extensions mirroring the CLI flags: `tierMax`,
    `captureWindowMs`, `noJail`, `allowUnsafeReplay`, `ignoreRobots`, `proxy`.
    Server-wide defaults from `draco serve` flags (`--timeout`, `--tier-max`,
    `--no-jail`, …). Every response carries a `draco` extension object
    (`sourceTier`, `timing`, `trace`).
  - `GET /health`; graceful shutdown on ctrl-c/SIGTERM; concurrency bounded by
    `--max-concurrency` (default 8, excess requests queue).
  - New `serve` cargo feature (default on), independent of `tier2`: the lean
    `--no-default-features` build stays axum-free, and
    `--features serve` without `tier2` serves the static tiers only.

## [0.6.1] — 2026-07-06

### Changed
- **ES-module import extraction now uses the Oxc parse-once AST** instead of
  regex. The supervisor's module-graph crawl (`extract_module_imports`) parses
  each fetched module with `oxc_parser` and reads its `ModuleRecord` — static
  `import`/`export … from` / `export * from` (the requested-module specifiers)
  plus dynamic `import("…")` string literals. A real parse means specifiers
  inside string literals or comments are never matched, and computed
  `import(expr)` is correctly ignored — eliminating the regex over-matching. Oxc
  error-recovery keeps extraction working on partial/odd bundles. (`<script src>`
  discovery in HTML stays a regex; the Oxc path is JS-only.) tier2-gated; the lean
  build pulls in no Oxc crates.

## [0.6.0] — 2026-07-06

**ES-module & external-script apps now hydrate.** Previously the Tier 2 isolate
ran only *inline classic* `<script>` — so real SPAs (external bundles,
`<script type="module">`, dynamic `import()`) never executed. Draco now runs
them, with the (air-gapped) isolate fed by a supervisor-side prefetch.

### Added
- **In-isolate ES-module execution** (`draco-runtime`): a `deno_core` module
  loader (`MapModuleLoader`) backed by a `{url → source}` map serves static +
  dynamic imports; a module not in the map resolves to an **empty module**
  (graceful) so a missing lazy chunk can't crash hydration. Scripts run in
  document order — classic (inline/external) via `execute_script`, ES modules via
  `load_side_es_module` + `mod_evaluate`. New entry
  `run_capture_with_resources(url, html, cfg, resources)`.
- **Supervisor script prefetch + module-graph crawl** (`draco-core`):
  `prefetch_scripts` seeds from every `<script src>`, then BFS-crawls the ES
  module graph (static/dynamic/`export … from` specifiers, resolved per importer)
  via `draco-net`, bounded by file-count + total-byte caps. The air-gap holds —
  the **supervisor** fetches; the isolate never does. (Import extraction is regex
  today; an `oxc_parser` pass is the planned upgrade.)
- **IPC**: `SupervisorToJail::Resource { url }` frames (body = source) stream the
  prefetched subresources to the jailed child before `Hydrate`; the child
  accumulates them into the module-loader map.

### Notes
- Verified end-to-end: an empty-shell SPA whose entire article is delivered by an
  external module + static import + dynamic `import()` hydrates and extracts to
  clean Markdown (`source_tier: runtime_interception`).
- `--tier-max 1`/`0` still skips the isolate; the lean `--no-default-features`
  build compiles with the prefetch/regex path gated out.

## [0.5.0] — 2026-07-06

**Tier 2 DOM engine replaced: real happy-dom, baked into a V8 snapshot.** The
hand-written ~2,185-line DOM/scheduler polyfill is gone. The isolate now runs a
real browser-grade DOM — [happy-dom](https://github.com/capricorn86/happy-dom) —
on a base of ecosystem web-primitive polyfills, bundled with **Rolldown** (Oxc)
and evaluated **once** into a V8 startup snapshot at build time. This dramatically
widens the set of SPAs that hydrate (real events, custom elements,
MutationObserver, CSSOM, `MessageChannel`) instead of the previous ~dozen
hand-stubbed DOM primitives.

### Changed
- **`draco-runtime` DOM engine → happy-dom.** `build.rs` bakes `js/base.iife.js`
  (whatwg-url, text-encoding, structured-clone + a Node-compat shim with
  `op_sleep`-backed timers + `MessageChannel`) and `js/happydom.iife.js` into a V8
  startup snapshot (`DRACO_SNAPSHOT.bin`). Each isolate restores it in
  ~single-digit ms rather than re-parsing ~2.6 MB of JS (~95 ms) per spawn — a
  ~3.4× cold-start win (~112 ms → ~33 ms to first hydrated DOM in the bench).
- Per-isolate `js/glue.js` constructs a fresh happy-dom `Window` for the target
  URL, mirrors its DOM globals onto `globalThis`, installs the `op_raze_fetch`
  fetch/XHR interceptor (page JS still does zero real I/O), swallows async
  errors, and loads the fetched HTML.
- **`--jitless` retained.** Benchmarking showed JIT vs. jitless is within noise
  here (the cost is snapshot restore + DOM construction, not hot JIT-tier loops),
  so the W^X / seccomp lockdown stays — no `mprotect(PROT_EXEC)` relaxation.

### Removed
- `js/polyfill.js` and `js/interceptor.js` (the hand-rolled DOM, scheduler, and
  Web-API shims) — fully superseded. One clean code path, no legacy engine.

### Notes
- The DOM bundles are vendored + regenerable (`vendor/happy-dom/`, Rolldown);
  `cargo build` needs no network — it only bakes the committed bundles into the
  snapshot.
- Known follow-on: apps delivered as ES modules (`<script type="module">` /
  dynamic `import()`) still need an isolate module loader — a fast-follow. Classic
  hydration payloads (Webpack/Next/Nuxt/Vite legacy) work today.

## [0.4.1] — 2026-07-06

### Fixed
- **Skeleton / `Loading…` screens are no longer returned as content, and now
  trigger the render pass.** The escalation trigger was purely length-based
  (`is_thin_content`), so a client-rendered page with lots of nav/promo chrome
  but whose actual content rails were still `Loading…` (e.g. a large retail
  homepage) cleared the thin-content bar and was returned verbatim — a wall of
  `Loading…` placeholders — *without* escalating to Tier 2. Now:
  - A new **incomplete-render detector** flags a page as a skeleton when it has
    several repeated `Loading…` / `Please wait` placeholder lines, **independent
    of length**. A skeleton escalates to the render-then-Markdown pass just like a
    thin shell does (when `--tier-max >= 2`).
  - **`Loading…` placeholder lines are stripped from the Markdown** in every case
    (even when the render pass is capped out or can't improve the page), so that
    noise never reaches the user. Real text that merely contains the word
    (`Loading dock tours`), buttons (`Load More`), and spinner images
    (`![loading](…)`) are left untouched.
  - The render upgrade now prefers a hydrated re-scrape that **resolves the
    skeleton** (real content replacing placeholders) even when it isn't longer,
    and refuses a hydration that is *still* a skeleton. `ScrapeResult` gains an
    `incomplete` flag; the `static.markdown` trace step names the reason
    (`incomplete render: skeleton/loading shell`).

## [0.4.0] — 2026-07-06

**Render-then-Markdown escalation** — Draco now scrapes client-rendered SPAs to
clean Markdown, not just static pages. This closes the v0.3.0 roadmap item: a
page whose *content* only exists after JavaScript runs is no longer returned as
an empty shell.

### Added
- **Render-then-Markdown for thin client-rendered shells.** When the initial
  fetch yields a thin shell (almost no static main content) and Tier 2 is
  permitted (`--tier-max >= 2`, the default), Draco hydrates the page in the same
  jitless V8 isolate it uses for JSON interception, serializes the *live* DOM
  (`document.documentElement.outerHTML`), splices the shell's real `<head>`
  (title, Open Graph, canonical, `<base>`) onto the hydrated `<body>`, and re-runs
  the identical Firecrawl-parity content engine over it. This mirrors how a real
  browser render feeds an HTML→Markdown transform — the isolate is the browser
  stand-in. One hydration now serves both `--format markdown` (DOM serialization)
  and `--format json` (endpoint interception).
- The result `trace` gains a **`runtime.render`** step (with the re-scraped
  character count), and a successful escalation is attributed to
  `source_tier: runtime_interception`. A thin shell that can't be improved (no
  DOM, hydration added nothing, or the isolate was unavailable) keeps the static
  shell and says so in the trace — never a regression, never a crash.

### Changed
- The runtime serializes the hydrated DOM after the capture window and returns it
  on the terminal IPC `Result` frame body (the frozen `JailToSupervisor::Result`
  header is unchanged — the DOM rides the frame body). `CaptureReport` /
  `CaptureResult` gain a `rendered_html` field.
- The isolate's DOM serializer now HTML-escapes text and attribute values, and
  the DOM parser decodes HTML entities into the in-memory text model — so the
  serialized markup re-parses losslessly (and `textContent` is finally correct
  for entity-bearing text). Raw-text elements (`<script>`/`<style>`) are left
  verbatim.

### Notes
- `--tier-max 1`/`0` skips the render pass (returns the static shell, noted in the
  trace). The lean `--no-default-features` build has no isolate and reports the
  render as skipped.
- As with `--format json` Tier 2, the OS-level jail requires Linux ≥ 5.13 with
  unprivileged user namespaces (or macOS's isolate mode); on hosts without it the
  render escalation degrades to the static shell. See
  `docs/BARE_METAL_VALIDATION.md`.

## [0.3.0] — 2026-07-05

Draco is now a **Markdown-first web scraper** — a lighter Firecrawl/Browserbase
alternative — with the JSON-API extraction as an opt-in mode.

### Changed
- **Default output is clean Markdown + metadata.** `draco extract <url>` returns
  the page's main content as Markdown (printed to stdout; `--json` for the full
  envelope). For standard HTML that's a single fingerprinted fetch + parse —
  ~300 ms, no browser. The tiered JSON-API extraction is now `--format json`.

### Added
- **Firecrawl-parity Tier 0 content extraction, natively in Rust.** Deterministic
  main-content extraction (42 boilerplate selectors with `:not(:has())`
  force-include protection — matching Firecrawl's *current* pipeline) with a
  Mozilla-Readability fallback (`dom_smoothie`); a Turndown/GFM-equivalent
  converter (`htmd`: ATX headings, fenced code with language, `-` bullets, GFM
  tables + strikethrough); and Firecrawl's pre/post-processing (unwanted-element
  stripping, `srcset` collapse, base64-image elision, skip-to-content and
  multiline-link cleanup, link/image absolutization).
- `metadata` mirrors Firecrawl's keys (`og:*`, `twitter:*`, `article:*`,
  `canonical`, `favicon`, `description`, `language`, `sourceURL`, `statusCode`,
  `contentType`, …).
- `ExtractionResult` gains `markdown` and `metadata` fields (additive).
- `--format <markdown|json|both>` and `--json` CLI flags.

### Notes
- JS-rendered SPAs whose *content* requires the DOM are flagged as a thin shell
  today; render-then-Markdown escalation via the Tier 2 isolate is the next step.

## [0.2.1] — 2026-07-05

### Fixed
- **Tier 2 hydration no longer crashes on standard Web APIs.** Real page scripts
  (and third-party analytics/fingerprinting like Cloudflare Zaraz) were throwing
  `ReferenceError: btoa is not defined`, `document.currentScript` was `undefined`
  (`…reading 'parentElement'`), and an unhandled promise rejection from any script
  aborted the whole capture loop — so the app never reached its data fetch. The
  isolate polyfill now provides `btoa`/`atob`, `crypto` (`getRandomValues`,
  `randomUUID`, `subtle` stub), `TextEncoder`/`TextDecoder`, `structuredClone`,
  `Blob`/`File`, `AbortController`/`AbortSignal`, `DOMException`, a per-script
  non-null `document.currentScript`, and richer `navigator`. A throwing or
  rejecting third-party script is now swallowed and the page keeps running —
  matching browser behavior — so a later script's data fetch is still captured.
  (Also fixed a latent bug: the fetch interceptor used `TextEncoder` without it
  being defined.)

## [0.2.0] — 2026-07-05

Tier 2 sandbox reframed for real-world use — **macOS is now first-class** and
there are no manual setup steps.

### Changed
- **Isolate mode is the supported cross-platform default.** Tier 2's real
  containment is the V8 context itself: page JS gets no host-capability bindings
  (no network/filesystem/process ops), the same isolation class as
  Puppeteer/Playwright/jsdom. It runs identically on macOS and Linux with zero
  configuration — the "dev only / running un-jailed" warnings are gone. macOS is
  a fully supported target, not a second-class one.
- **seccomp is now a robust *denylist*** instead of a default-deny allowlist. It
  kills only the unambiguous breakout syscalls (`execve`, `socket`/`connect`,
  `ptrace`, `mount`, `bpf`, executable `mprotect`, …) and allows the rest, so it
  **never needs per-host tuning** — the manual "SIGSYS iterate loop" is gone.
  Network is now blocked by the denylist itself (no longer dependent on user
  namespaces); netns + Landlock remain best-effort extra layers applied
  automatically when the kernel supports them.
- **The achieved sandbox level is reported in the result `trace`** as a
  `runtime.sandbox` step (e.g. `hardened: seccomp+netns+landlock` or `isolate: v8
  no host bindings (macos)`) instead of being shouted to stderr.

### Added
- `--strict-sandbox` flag / `Config::strict_sandbox` — opt into the maximal
  default-deny seccomp allowlist (may need per-host tuning; see
  `docs/BARE_METAL_VALIDATION.md`).

### Docs
- README security/platform sections rewritten around the two-level model;
  `docs/BARE_METAL_VALIDATION.md` reframed as *optional* hardening verification.

## [0.1.1] — 2026-07-05

### Fixed
- **Challenge detection no longer false-positives on CDN-fronted `200` pages.**
  A response is now classified as a bot-wall challenge only when it carries the
  definitive `cf-mitigated` header, or arrives with a **blocking status**
  (`403`/`429`/`503`) *and* a specific interstitial token (a challenge-script
  `src`, a captcha-delivery host, a verification class). Previously, benign
  Cloudflare signals on an ordinary `200` — the `/cdn-cgi/challenge-platform/`
  JS-detections beacon, `server: cloudflare`, `__cf_bm` cookies — or even a page
  whose *copy* merely mentioned a vendor name were mislabeled `needs_browser`,
  making Draco give up on sites `curl` reads fine. This defeated the tool's core
  purpose; extraction now proceeds on any `2xx`. (Reported against a
  Cloudflare-DNS site with no anti-bot enforcement.)

## [0.1.0] — 2026-07-05

First release. A browserless, tiered data-extraction engine — a statically
buildable Rust workspace (7 crates) with a `draco` CLI.

### The tiered engine
- **Tier 0 — static extraction** (`draco-static`): `__NEXT_DATA__`, JSON-LD, and
  object-literal `window.__NUXT__`, via a quote-aware HTML scan.
- **Tier 1 — heuristic API replay** (`draco-static` + `draco-core`): discover a
  Next.js `buildId` and fetch `/_next/data/<buildId>/…​.json` directly; app-router
  (RSC) pages are detected and escalate.
- **Tier 2 — runtime interception** (`draco-runtime` + `draco-jail`): a jitless
  V8 isolate hydrates the page, its `fetch`/`XHR` calls are intercepted, ranked,
  and the winner is replayed with the stealth client. Proven end-to-end against a
  **real Vue 3.5.39 bundle** that hydrates in-isolate and leaks a response-driven
  dependent fetch.

### Networking (`draco-net`)
- `wreq 6.0.0-rc.29` (BoringSSL) Chrome JA4/TLS + HTTP/2 emulation with preserved
  header order; per-session cookie jar; http/https/socks5 proxy; per-host delay
  with jitter; 429/503 backoff honoring `Retry-After`; robots.txt (respected by
  default); redirect cap; connect + total timeouts.

### Orchestration (`draco-core`)
- Escalation state machine with a challenge short-circuit
  (Cloudflare/DataDome/Akamai/PerimeterX → `needs_browser`), `tier_max` clamp, and
  full `timing` + `trace` assembly.
- Intercept **ranking policy** (canonical §11): same-origin +10, api-path +8,
  json +5, analytics −100, static-asset −50.
- **Mutation-safety**: state-changing requests (unsafe methods that aren't
  GraphQL/JSON-RPC reads) are withheld from replay unless `--allow-unsafe-replay`.

### Security sandbox (`draco-jail`)
- Self-re-exec `draco __jail` child: user + network namespace air-gap, Landlock
  filesystem lockdown, two-phase seccomp-bpf (default `KILL`), fd-3 length-prefixed
  IPC codec. V8 runs `--jitless --single-threaded` (no executable memory).

### CLI (`draco-cli`)
- `draco extract <URL>` → `ExtractionResult` JSON (`status`, `source_tier`,
  `data`, `timing`, `trace`). Flags: `--extract <JSONPATH>`, `--tier-max`,
  `--proxy`, `--delay`, `--timeout`, `--capture-window-ms`, `--ignore-robots`,
  `--no-jail`, `--allow-unsafe-replay`, `--pretty`. Exit codes 0/1/2/3.

### Build & packaging
- **Feature-gated Tier 2**: `--no-default-features` yields a lean Tier 0/1 build
  with no V8 or jail linked. Targets `x86_64-unknown-linux-gnu` (full jail) and
  `aarch64-apple-darwin` (dev, un-jailed Tier 2).

### Known limitations
- **Jailed enforcement is validated on bare metal only.** seccomp kills, the
  V8-under-seccomp allowlist, netns, and Landlock require kernel ≥ 5.13 +
  unprivileged user namespaces — see `docs/BARE_METAL_VALIDATION.md`. The
  allowlist is expected to need per-host iteration.
- **JS challenge walls are not defeated** (Cloudflare/DataDome/…) — they return
  `needs_browser`.
- **Framework hydration** is proven for virtual-DOM frameworks (Vue verified);
  frameworks needing layout measurement, real event dispatch, or ES-module/WASM
  delivery may not hydrate in the hand-written polyfill.
- **V8 snapshot** cold-start optimization is intentionally deferred (runtime
  polyfill execution is used instead).
- **musl fully-static** single-binary build is deferred.

[0.3.0]: https://github.com/0xchasercat/draco/releases/tag/v0.3.0
[0.2.1]: https://github.com/0xchasercat/draco/releases/tag/v0.2.1
[0.2.0]: https://github.com/0xchasercat/draco/releases/tag/v0.2.0
[0.1.1]: https://github.com/0xchasercat/draco/releases/tag/v0.1.1
[0.1.0]: https://github.com/0xchasercat/draco/releases/tag/v0.1.0
