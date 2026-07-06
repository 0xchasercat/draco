# Changelog

All notable changes to Draco are documented here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/); this project uses SemVer.

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
