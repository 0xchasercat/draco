# Draco Roadmap — current handoff

Status as of **v0.13.6** (2026-07-06). This is the canonical takeover document for the next agent. It supersedes older v0.10-era roadmap notes and matches the repository state on `main` at tag `v0.13.6`.

Draco is a fast, stealth, native-Rust web scraper positioned as a lighter DOM-only alternative to Firecrawl / Browserbase. It is not a browser and must never fake browser-only outputs such as screenshots or actions.

## Ground rules for the next agent

- Verify claims against the actual source and `docs/firecrawl-api-reference.md` before asserting Firecrawl parity.
- Keep one clean code path. When replacing a design, delete the obsolete path rather than preserving legacy aliases or parallel implementations.
- Unknown/unrecognized request fields stay **accept-and-ignore** for drop-in friendliness; do not switch to Firecrawl's strict-Zod 400 behavior unless explicitly requested.
- CLI verbs mirror REST: `scrape`, `discover`, `map`, then later `crawl`, `batch`, `search`. Do not resurrect the removed `extract` alias.
- Draco is DOM-only. `screenshot`, `screenshot@fullPage`, `actions`, and other browser-only formats return explicit **422 needs-browser**, never fake payloads.
- For substantial work: critique the plan first, record the agreed architecture here or in a companion design doc, implement on a feature branch, run fmt/check/build/test/clippy, commit, merge, tag, push, and scrub any temporary Git credentials.
- The Linux jail cannot be fully red-team validated in the sandbox. Sandbox kernel/userns/Landlock constraints differ from bare metal; local gates still matter, but seccomp/Landlock runtime enforcement should be treated with evidence.

## Shipped state by release

### v0.7.0 — daemon scrape API

- `draco serve` exposes a Firecrawl-shaped `POST /v1/scrape` plus `GET /health`.
- Output included `markdown` and `json` at this point.
- Unknown fields were accepted and ignored.
- Unsupported browser-only formats were rejected rather than faked.

### v0.8.0 — map, crawl, MCP base

- `POST /v1/map` discovers URLs from sitemap.xml plus on-page links.
- `POST /v1/crawl`, `GET /v1/crawl/{id}`, and `DELETE /v1/crawl/{id}` implement async bounded BFS crawl jobs.
- MCP server shipped over stdio plus a minimal `/mcp` HTTP route.
- MCP tool `draco_scrape` shipped.

### v0.8.1 — draco-net connection pooling

- `draco-net` now uses a process-wide pooled `wreq::Client` cache keyed by proxy.
- Cookie isolation is preserved with a fresh per-call cookie jar attached to each request.
- This fixed the prior fresh-client-per-fetch behavior and restored keep-alive / HTTP-2 pooling.

### v0.9.0 — warm Tier 2 isolate pool for daemon

- The jailed child loops over multiple hydrate jobs until shutdown.
- `Tier2Pool` reuses the worker process and sandbox, but creates a fresh snapshot-restored V8 isolate per scrape.
- Daemon routes `/v1/scrape`, `/v1/crawl`, and `/mcp` through the pool.
- CLI and stdio MCP remain one-shot.

### v0.10.0 — API discovery/replay

- `--format endpoints`, `POST /v1/discover`, and MCP `draco_discover` expose ranked API endpoint discovery.
- `DiscoveredEndpoint` includes `method`, `url`, `via`, `score`, `replayable`, and `headers`.
- `ExtractionResult.endpoints` and `Config.discover_endpoints` were added.
- Discovery runs after Markdown/render consideration and before Tier 0 JSON fallback when requested.

### v0.11.0 — API alignment and format surface

- Replaced the old coarse `OutputFormat` with `FormatSet` for `{ markdown, html, rawHtml, links, json, endpoints }`.
- `ExtractionResult` gained `html`, `rawHtml`, and `links`.
- CLI renamed `extract` to `scrape`; the old verb was removed, not aliased.
- CLI added `discover` and `map` verbs.
- `--format` became repeatable and accepts both wire casing and kebab casing, including `rawHtml` / `raw-html`.
- `onlyMainContent` and `waitFor` are honored.
- Crawl `includePaths` / `excludePaths` now use regex semantics against URL pathname, with `regexOnFullURL` for full URL matching.
- Map gained robots.txt `Sitemap:` discovery, `ignoreSitemap`, `sitemapOnly`, default `includeSubdomains: true`, and higher limit cap.
- Browser-only formats return clear 422 needs-browser responses.
- `docs/firecrawl-api-reference.md` was vendored as the parity reference.

### v0.11.1 — field honoring

- `includeTags` and `excludeTags` are honored on scrape surfaces and crawl `scrapeOptions`.
- Custom `headers` are forwarded to outbound fetches, retries, and robots probes.
- This completed the planned v0.11 request-field parity slice.

### v0.12.0 — batch scrape and shared async job store

- `POST /v1/batch/scrape` runs a list of URLs as one async job.
- Batch scrape options are **flat at top level**, not nested under `scrapeOptions`.
- `ignoreInvalidURLs` drops invalid non-http(s) URLs into `invalidURLs`.
- Crawl and batch share `serve::jobs::JobStore`.
- Status endpoints support pagination via `?skip=&limit=`, `next`, 10 MiB page cap, `creditsUsed`, and advisory `expiresAt`.
- Crawl gained `GET /v1/crawl/{id}/errors`; batch has `GET /v1/batch/scrape/{id}/errors`.
- Per-page failures are recorded.

### v0.12.1 — robotsBlocked parity fix

- `robotsBlocked` is now populated for crawl and batch `/errors` endpoints.
- `draco-net` signals robots denial as `NetKind::Robots` instead of folding it into ordinary status failures.
- Crawl and batch route robots-denied URLs to `robotsBlocked`, not `errors`.

### v0.13.0 — webhooks

- Crawl and batch scrape requests accept `webhook` as either a bare URL string or `{ url, headers, metadata, events }`.
- Four lifecycle events are supported: `started`, `page`, `completed`, `failed`.
- The `events` filter uses bare names; emitted payload `type` is prefixed by job kind, e.g. `crawl.page`, `batch_scrape.completed`.
- Payload shape is `{ success, type, id, data, metadata }`.
- Delivery is fire-and-forget via `draco_net::replay`, with 10s deadline and retries at +1, +5, +15 minutes.
- Webhook endpoints are never robots-gated.
- v0.13.0 is **not** search. Search was resequenced to v0.14.0.

### v0.13.1 — discover/runtime patch

- Fixed Linux hardened jail failures where `draco discover` died before returning a result.
- Default seccomp denylist now allows `clone3` because modern libc may use it for ordinary thread creation. `execve` / `execveat` remain killed, so a cloned task cannot become a new program image.
- Jailed child `RLIMIT_AS` was raised to 64 GiB to permit current deno_core/V8 Oilpan virtual cage reservations. RSS remains small; this is a virtual address-space cap, not a resident-memory target.
- The W^X `mprotect(PROT_EXEC)` guard remains in place.
- Fixed classic inline SvelteKit/Vite boot scripts by executing inline classic scripts under an absolute page-derived synthetic script URL instead of non-base `draco:page[i]`. This fixes relative dynamic imports such as `import("./_app/...")`.
- Module graph prefetching now seeds from `<script src>`, `<link rel="modulepreload">`, `<link rel="preload" as="script">`, and inline-script string-literal imports.
- Local Chaser/SvelteKit-shaped fixtures validated hardened discover capture. Full gates were green: 344 tests, 0 failed.

### v0.13.2 – v0.13.5 — Next.js/webpack dynamic-chunk saga (bluff.com)

- **v0.13.2**: the air-gapped isolate now *executes* dynamically appended `<script src>` chunks itself (glue hooks `appendChild`/`insertBefore`/`append`; `op_raze_resource` exposes the prefetched resource map) instead of firing a false `ChunkLoadError`. Prefetch follows webpack/Next chunk-loader references inside fetched bundles.
- **v0.13.3**: broadened chunk prefetch to split id→basename / id→hash webpack maps (hash threshold ≥ 12 hex).
- **v0.13.4**: replaced static-prefetch-only chunks with a **supervisor-mediated dynamic script loader** — `op_raze_load_script` rides new `LoadScript`/`Script` IPC frames; the supervisor fetches via draco-net; the isolate still never touches the network. Capture window clamped to 2 500 ms supervisor-side.
- **v0.13.5**: browser-ish Performance API shim (no-op `getEntriesByType`/`mark`/`measure`/…, `PerformanceObserver` class) so telemetry/web-vitals chunks (e.g. Sentry) stop aborting hydration.

### v0.13.6 — Tier 2 wall-time budgets + runtime diagnostics

- **Why**: on live code-split sites the 2 500 ms capture clamp did not bound wall time — `prefetch_scripts` fetched up to 64 chunk candidates *sequentially* (30 s session timeout each) inside the `runtime.capture` step, and synchronous `LoadScript` services could not be preempted mid-JS-turn (observed: 24.7 s `runtime_ms` on bluff.com from macOS).
- Prefetch is now a **wave-parallel BFS** (8 concurrent over the borrowed fetcher) with a 5 s wall budget and a 2.5 s per-subresource timeout clamp (politeness delay dropped for subresources). Recorded as its own `runtime.prefetch` trace step, so `runtime.capture` times the capture alone.
- `LoadScript` servicing has a 4 s per-job wall budget (past it the supervisor answers `ok: false` immediately) and the same per-fetch clamp.
- A markdown-only scrape that produced **empty** markdown (empty client-rendered shell) now prints the JSON envelope instead of a single blank line.
- **`--runtime-log`** (CLI scrape+discover) / **`runtimeLog`** (daemon, MCP): surfaces the isolate's page-side diagnostics — glue-swallowed exceptions/rejections, `console.error`/`console.warn`, page-script throws — as `runtime.log` trace steps. New `op_raze_log`; lines are count/length-bounded child-side and ride as `Error`-level `Log` frames before the terminal `Result`. This is the tool for diagnosing *why* a page hydrates to 0 endpoints.
- Fixture-verified (42-chunk delayed SPA + 8 s hung chunk, hardened jail): `runtime_ms` 16 680 → 3 878 ms; endpoint discovery intact.
- Known follow-up: `cargo test --no-default-features` has pre-existing failures (tier2-gated helpers referenced by ungated tests) — the lean gate is build-only today.

## Current capability matrix

- Scrape: shipped.
- Map: shipped.
- Crawl: shipped.
- Batch scrape: shipped.
- MCP: scrape and discover shipped; map/batch/search MCP riders remain to add as needed.
- API discovery: shipped and patched.
- Webhooks: shipped.
- Search: **not shipped**. This is the next major feature, now planned as v0.14.0.

## Next major release: v0.14.0 — search / metasearch

Search is the remaining net-new engine. The goal is Firecrawl-compatible search through Draco's own stealth HTTP stack, not an external search API dependency.

### Product contract

Add:

- `POST /v1/search`
- `draco search <query>`
- MCP tool `draco_search` as a rider after the REST/CLI surface is stable

Request shape should be Firecrawl-compatible where practical:

- `query`: required string.
- `limit`: default 5, max 100.
- `tbs`: optional time filter string. Treat as engine-specific best effort.
- `location`: accept; initially best-effort or no-op per engine if exact support is not available.
- `timeout`: default 60000 ms.
- `scrapeOptions`: optional. When present and `formats` is non-empty, each selected SERP URL is passed through Draco's existing scrape ladder with those options.

Response shape:

- `success: true`.
- `data: []` flat array.
- Each result includes at least `title`, `description`, and `url`.
- If `scrapeOptions.formats` is requested, merge the scrape fields from Draco's `Document` shape into the corresponding result, rather than inventing a separate nested schema unless Firecrawl reference requires it.
- On total engine failure, return a clear error. On partial engine failure, return successful consensus results and optionally include Draco extension diagnostics.

### Architecture decision

Do **not** clone or maintain SearXNG's large parser set.

The maintenance-avoidance mechanism is:

- A small set of robust engines behind a swappable trait.
- Parallel fan-out using Tokio `JoinSet`.
- Consensus scoring tolerant of per-engine failure.
- Engines can rot, captcha-wall, or fail individually without taking the feature down.

SearXNG is a selector and behavior reference only, not a dependency and not a parser source to port wholesale.

### Engine set

Start with a small, practical set:

- DuckDuckGo HTML.
- Bing.
- Brave.
- Mojeek.
- Google only as best-effort if it is cheap to include; expect it to fail often and do not make it foundational.

Each engine should be independently testable from saved SERP fixtures so parser changes do not require live search during unit tests.

### Suggested crate/module layout

Prefer a clean module under the daemon / serve layer if the feature is only exposed through CLI/REST, or a reusable core module if CLI and REST both need it without going through HTTP.

Likely shape:

- `crates/draco-core/src/search.rs` or `crates/draco-cli/src/serve/search.rs` depending on how much reuse is needed.
- `SearchEngine` trait:
  - `name() -> &'static str`
  - `build_request(query, params) -> HttpRequestSpec` or equivalent URL + headers.
  - `parse(html, base_url) -> Vec<SearchHit>`.
- `SearchHit` internal struct:
  - `title`
  - `description`
  - `url`
  - `engine`
  - `rank`
- consensus function over `Vec<SearchHit>`.

Use `draco-net` for outbound SERP fetches so search gets the same stealth transport, proxy support, pooled clients, timeout behavior, and header handling as the rest of Draco.

### Consensus scoring

Use a SearXNG-like simple reciprocal-rank consensus:

- Normalize/canonicalize URLs enough to merge obvious duplicates.
- Score each hit by summing `1 / rank` across engines.
- Preserve supporting engine names and ranks internally for trace/debug extension data.
- Sort by score descending, then by best rank, then deterministic URL/title tie-breakers.
- Apply `limit` after consensus.

Do not over-engineer ranking in v0.14.0. The key property is fault tolerance with understandable behavior.

### Scrape integration

If `scrapeOptions.formats` is present and non-empty:

- For each selected result URL, call the existing scrape ladder with a `Config` derived from `scrapeOptions`.
- Reuse v0.11+ `FormatSet`; do not create a parallel output enum.
- Respect `onlyMainContent`, `includeTags`, `excludeTags`, `headers`, `waitFor`, `timeout`, and the DOM-only 422 behavior already implemented for scrape.
- Bound concurrency so search+scrape cannot explode work. Use the daemon gate or a local bounded `JoinSet`.
- Partial scrape failures should not necessarily fail the entire search; include the SERP hit and omit scrape fields or include a Draco extension error, depending on Firecrawl reference and existing response conventions.

### CLI surface

Add:

```sh
draco search "rust web scraper"
draco search "rust web scraper" --limit 10
draco search "rust web scraper" --format markdown --format links
```

Keep CLI names aligned with REST. Do not add an unrelated alias.

### REST surface

Add:

- Route: `POST /v1/search`.
- Accept JSON body matching the product contract above.
- Unknown fields stay accepted-and-ignored.
- Validate required `query` and `limit` bounds.
- Use a clear timeout model: overall search timeout defaults to 60000 ms; per-engine fetches should not each consume the full deadline serially.

### MCP rider

After REST/CLI search is stable, add `draco_search` to MCP with a schema mirroring `POST /v1/search`.

Do this as a rider, not as a standalone architecture fork.

### Tests to add for v0.14.0

Unit tests:

- Engine parser fixtures for each initial engine.
- URL canonicalization and duplicate merge.
- Consensus scoring with partial engine failure.
- Limit default and max behavior.
- Request validation.
- `scrapeOptions` to `Config` mapping.

Integration-ish tests:

- Mock/fake engines returning overlapping results.
- REST `POST /v1/search` happy path.
- CLI `draco search` output shape.
- Search with scrape formats using local fixture pages.
- Partial engine failure still returns consensus results.

Gates before ship:

- `cargo fmt --all -- --check`
- `cargo check --no-default-features`
- `cargo build --workspace`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`

### v0.14.0 implementation slices

1. **Design confirmation**
   - Re-read `docs/firecrawl-api-reference.md` search section.
   - Confirm exact response shape and scrapeOptions behavior.
   - Record any intentional divergence in this roadmap before coding.

2. **Core search types and consensus**
   - Define internal `SearchHit`, normalized URL key, consensus scorer, and tests.
   - No network yet.

3. **Engine trait and first parser fixtures**
   - Implement DuckDuckGo HTML first.
   - Add saved fixture tests.
   - Add Bing/Brave/Mojeek after the trait is stable.

4. **draco-net fan-out**
   - Use `JoinSet` to fetch engines concurrently.
   - Tolerate per-engine network/parser errors.
   - Return diagnostics in Draco extension data if appropriate.

5. **REST endpoint**
   - Add `POST /v1/search` request parsing, validation, timeout handling, and response serialization.

6. **ScrapeOptions integration**
   - Convert search `scrapeOptions` to existing scrape `Config` / `FormatSet`.
   - Bound scrape concurrency.
   - Add tests with local pages.

7. **CLI**
   - Add `draco search` matching REST options.
   - Keep output shape consistent with other CLI JSON output.

8. **MCP rider**
   - Add `draco_search` after REST/CLI are green.

9. **Docs, changelog, version, ship**
   - README usage.
   - CHANGELOG v0.14.0.
   - Bump version.
   - Full gates.
   - Feature branch, merge to main, tag, push.

## Known constraints and non-goals

- No screenshot/actions/browser automation in Draco. Return 422 needs-browser.
- Do not add paid external search APIs such as Exa/Tavily as Draco dependencies.
- Do not port all SearXNG engines.
- Do not overfit to Google; Google failure must be a normal partial failure.
- Do not make live SERP availability required for unit tests. Use fixtures.
- Keep `--no-default-features` build working; serve-specific deps must remain properly gated.
- Keep async job store semantics unchanged unless search explicitly needs async jobs; initial search can be synchronous request/response with timeout.

## Suggested immediate next step

Before coding v0.14.0, do a short search-design spike:

1. Read Firecrawl's search reference in `docs/firecrawl-api-reference.md`.
2. Fetch or save sample SERP HTML for DuckDuckGo, Bing, Brave, and Mojeek.
3. Confirm which engines are reachable through `draco-net` in the dev environment.
4. Decide exact response shape for partial failures and scrapeOptions failures.
5. Update this roadmap with any confirmed divergences, then implement the slices above.
