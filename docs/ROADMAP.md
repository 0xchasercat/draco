# Draco Roadmap — current handoff

Status as of **v0.15.0** (2026-07-10). This is the canonical takeover document for the next agent. It supersedes older roadmap notes and matches the repository state on `main` at tag `v0.15.0`.

Draco is a fast, stealth, native-Rust web scraper positioned as a lighter DOM-only alternative to Firecrawl / Browserbase. It is not a browser and must never fake browser-only outputs such as screenshots or actions.

## Ground rules for the next agent

- Verify claims against the actual source and `docs/firecrawl-api-reference.md` before asserting Firecrawl parity.
- Keep one clean code path. When replacing a design, delete the obsolete path rather than preserving legacy aliases or parallel implementations. (This is how the OS jail went in v0.14 — amputated, not band-aided.)
- Unknown/unrecognized request fields stay **accept-and-ignore** for drop-in friendliness; do not switch to Firecrawl's strict-Zod 400 behavior unless explicitly requested.
- CLI verbs mirror REST: `scrape`, `discover`, `map`, then later `crawl`, `batch`, `search`. Do not resurrect the removed `extract` alias.
- Draco is DOM-only. `screenshot`, `screenshot@fullPage`, `actions`, and other browser-only formats return explicit **422 needs-browser**, never fake payloads.
- For substantial work: critique the plan first, record the agreed architecture here or in a companion design doc, implement on a feature branch, run fmt/check/build/test/clippy, commit, merge, tag, push, and scrub any temporary Git credentials.
- **Tier 2 containment is the in-process V8 isolate itself** (no host-capability bindings), plus the hosted-cloud infrastructure perimeter. There is no OS process jail to red-team any more; do not reintroduce one. Page JS cannot perform I/O regardless of platform — the only I/O it can cause is the fetches the engine explicitly brokers (script subresources always; data requests only in Render mode, under the mutation-safety policy).

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

### v0.9.0 – v0.12.1 — API surface build-out

- **v0.9.0**: warm Tier 2 pool for the daemon (later superseded by the in-process engine in v0.14).
- **v0.10.0**: API discovery/replay — `--format endpoints`, `POST /v1/discover`, MCP `draco_discover`; ranked `DiscoveredEndpoint` catalog.
- **v0.11.0**: `FormatSet` (`markdown/html/rawHtml/links/json/endpoints`) replacing the coarse output enum; `extract`→`scrape` rename; repeatable `--format`; `onlyMainContent`/`waitFor` honored; map robots `Sitemap:` discovery.
- **v0.11.1**: `includeTags`/`excludeTags` + custom `headers` honored across scrape/crawl surfaces.
- **v0.12.0**: `POST /v1/batch/scrape` + shared async `JobStore`; paginated status; per-page errors.
- **v0.12.1**: `robotsBlocked` parity (robots denial is its own `NetKind::Robots`, routed to `robotsBlocked` not `errors`).

### v0.13.0 — webhooks

- Crawl and batch scrape requests accept `webhook` as either a bare URL string or `{ url, headers, metadata, events }`; lifecycle events `started`/`page`/`completed`/`failed`; payload `{ success, type, id, data, metadata }`; fire-and-forget delivery with retries; never robots-gated.

### v0.13.1 – v0.13.13 — Tier 2 hydration hardening (the SPA saga)

A long run of fixes making the (then jailed, jitless) Tier 2 isolate hydrate real code-split SPAs: absolute synthetic script URLs for inline SvelteKit/Vite boot; webpack/Next dynamic-chunk execution + supervisor-mediated on-demand chunk loading; a Performance API shim; wall-time budgets + `--runtime-log` diagnostics (v0.13.6); frame-identity unification (`window===self===globalThis===top`); honest ESM loading + backfilled DOM element constructors; critical-path prefetch prioritization; `EventSource`/`WebSocket`/SSE no longer aborting hydration and no longer blind-replayed (streaming endpoints); and cross-operation cookie persistence (v0.13.13). All within the OS-jail architecture that v0.14 then replaced.

### v0.13.14 — concurrent on-demand chunk prewarmer

- A per-job prewarmer fetched a requested chunk's dependency closure concurrently in the background, hiding the jail's per-chunk IPC latency. Superseded weeks later by v0.14's in-process async loader (the prewarmer and its import-graph machinery were deleted).

### v0.13.15 — JIT enabled

- Dropped `--jitless`; lifted the jail's W^X `mprotect` guard so JIT could map code. SPA hydration had been 3–10× too slow jitless, blowing the chunk budget. (The jail itself went one release later.)

### v0.14.0 — in-process async engine (the jail is deleted)

- **Amputate, don't band-aid.** The OS process jail (fork/exec + userns/netns air-gap + seccomp + Landlock + per-chunk blocking IPC) was the root cause of Tier 2's serialized, budget-strangled chunk loading. It is **gone entirely** — the `draco-jail` crate (9 files), `prewarm.rs`, and the prefetch/import-graph machinery were removed.
- V8 now runs **in-process** on a current-thread tokio runtime behind an async `ScriptFetcher` seam: `op_raze_load_script` is a real async op and the module loader awaits the fetcher, so `import()`/chunk loads fan out **concurrently** on the event loop like a browser's network stack. Initial external `<script src>` fetched concurrently, executed in document order; an in-flight-load counter keeps the capture window from quiescing mid-load.
- Containment is the isolate itself (no host-capability bindings) plus the infrastructure perimeter. `--no-jail`/`--strict-sandbox` remain as inert CLI no-ops. `Tier2Pool` is now a semaphore bounding concurrent isolates.
- **Process-global immutable chunk cache**: 512 MiB RAM LRU + 2 GiB disk (`~/.cache/draco/chunks`), collision-safe, never worse than a miss; hashed SPA chunks fetched once across scrapes. Data responses are never cached.

### v0.15.0 — Tier 2 Render mode (pure-CSR SPAs render their content)

- **Render mode.** Pure-CSR SPAs paint only after client-side data fetches resolve; Tier 2 stubbed those (right for `discover`, fast for SSR/hybrid), so `scrape` got an empty DOM. The isolate now has two fetch modes: **Observe** (unchanged — record + synthetic stub; used by `discover` and the JSON tier) and **Render** — the page's *safe* data requests (`GET`/`HEAD`, read-style `POST`/`PUT`) are fetched **live** through `draco_net::replay` (pooled client + shared cookie jar) and the page sees the real status/headers/body incl. non-2xx. Mutation-safety reused verbatim from ranking; streaming/analytics never live; `--allow-unsafe-replay` honored. The existing thin/skeleton-shell escalation triggers Render automatically (hidden `--force-render`). Measured: thrill.com 478-char footer → full 42,823-char lobby.
- **Five hydration fixes** that Render mode surfaced: `document.currentScript` points at the real parsed node (SvelteKit mount target, #14); completion-biased Web Animations shim (Svelte 5 transitions); injected `<script type="module">` routed through `import()`; completion-biased IntersectionObserver/ResizeObserver (lazy-load sections); document lifecycle dispatch (`readystatechange`/`DOMContentLoaded`/`load` + `readyState` shadowing).
- **Render observability** (`--runtime-log`): `[raze.fetch]`/`[raze.chunk]`/`[raze.module]`/`[raze.window]` diagnostics stamped `[+ms]` from capture start; log budget 32→96, deduped.
- **Mode-aware capture window**: Observe keeps the tight ~2.5 s cap; Render gets an 8 s floor / 15 s ceiling (a ceiling, not a wait — quiesce ends it early).
- **Content-activity quiesce**: an `is_tracker()` denylist (analytics/session-replay/ad/bot-detection vendors) means trackers are recorded + fetched live but don't hold the capture window open. Every render closes at [last content fetch + quiesce]. Measured: target.com render tier 3999 ms → 2348 ms (−41%), identical content; thrill (real data dependency) unaffected.
- **`discover` replayable-flag alignment** (one eligibility rule feeds catalog + replay) and **stdout JSON cleanliness** (timer/MessagePort errors → `runtime.log`, never stdout).

## Current capability matrix

- Scrape (Markdown + render-then-Markdown for CSR SPAs): shipped.
- Map: shipped.
- Crawl: shipped.
- Batch scrape: shipped.
- MCP: scrape and discover shipped; map/batch/search MCP riders remain to add as needed.
- API discovery: shipped and patched.
- Webhooks: shipped.
- Tier 2 render mode: shipped (v0.15.0).
- Search: **not shipped**. This remains the next major net-new feature (planned below as v0.16.0).

## Next major release: v0.16.0 — search / metasearch

Search is the remaining net-new engine, and it is the recommended next major feature now that the Tier 2 engine is solid. The goal is Firecrawl-compatible search through Draco's own stealth HTTP stack, not an external search API dependency.

(Note: this plan was originally drafted as "v0.14.0" in the older roadmap; v0.14/v0.15 were instead spent on the in-process engine rewrite and Render mode, so search is renumbered to v0.16.0. The design below is unchanged and still current.)

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

Do not over-engineer ranking in v0.16.0. The key property is fault tolerance with understandable behavior.

### Scrape integration

If `scrapeOptions.formats` is present and non-empty:

- For each selected result URL, call the existing scrape ladder with a `Config` derived from `scrapeOptions`.
- Reuse `FormatSet`; do not create a parallel output enum.
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

After REST/CLI search is stable, add `draco_search` to MCP with a schema mirroring `POST /v1/search`. Do this as a rider, not as a standalone architecture fork.

### Tests to add

Unit tests: engine parser fixtures per engine; URL canonicalization + duplicate merge; consensus scoring with partial engine failure; limit default/max; request validation; `scrapeOptions`→`Config` mapping.

Integration-ish tests: mock/fake engines with overlapping results; REST `POST /v1/search` happy path; CLI `draco search` output shape; search with scrape formats using local fixture pages; partial engine failure still returns consensus results.

Gates before ship: `cargo fmt --all -- --check`, `cargo check --no-default-features`, `cargo build --workspace`, `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`.

### Implementation slices

1. **Design confirmation** — re-read the search section of `docs/firecrawl-api-reference.md`; confirm response shape + scrapeOptions behavior; record any intentional divergence here before coding.
2. **Core search types + consensus** — internal `SearchHit`, normalized URL key, consensus scorer, tests. No network yet.
3. **Engine trait + first parser fixtures** — DuckDuckGo HTML first; saved fixture tests; Bing/Brave/Mojeek after the trait is stable.
4. **draco-net fan-out** — `JoinSet` concurrent fetch; tolerate per-engine network/parser errors; diagnostics in Draco extension data if appropriate.
5. **REST endpoint** — `POST /v1/search` parsing, validation, timeout handling, serialization.
6. **ScrapeOptions integration** — convert to `Config`/`FormatSet`; bound scrape concurrency; local-page tests.
7. **CLI** — `draco search` matching REST options; consistent JSON output shape.
8. **MCP rider** — `draco_search` after REST/CLI are green.
9. **Docs, changelog, version, ship** — README usage; CHANGELOG v0.16.0; bump version; full gates; feature branch → merge → tag → push.

## Performance backlog (post-v0.15.0)

The Tier 2 render tier is now correct and its window management is deterministic (every render closes at [last content fetch + quiesce]). The remaining cost is dominated by **CPU-bound hydration** — heavy SPAs peg ~80% of one core running the app's own JS. Candidate work, in rough priority order:

1. **Profile the render tier's CPU** — determine whether the time is in happy-dom's DOM implementation, the glue/polyfills, or the page's own hydration; that decides whether a meaningful engine-side win exists or we're at parity with the page's real cost.
2. **Quiesce trim** — the render quiesce tail is 500 ms; ~350 ms would shave ~150 ms/render at a small early-close risk.
3. **DOM-growth fallback** — generalize `is_tracker()` so an *unlisted* beacon that doesn't grow the DOM also can't hold the window.
4. **`Deno`/isServer fidelity** — `globalThis.Deno` (deno_core's op bridge) leaks into page scope, so libraries like TanStack Query mis-detect a server environment. Hide it from page scripts without breaking op resolution.

## Known constraints and non-goals

- No screenshot/actions/browser automation in Draco. Return 422 needs-browser.
- Do not add paid external search APIs such as Exa/Tavily as Draco dependencies.
- Do not port all SearXNG engines.
- Do not overfit to Google; Google failure must be a normal partial failure.
- Do not make live SERP availability required for unit tests. Use fixtures.
- Keep `--no-default-features` build working; serve-specific deps must remain properly gated.
- Do not reintroduce the OS process jail. Tier 2 containment is the no-host-bindings isolate + the infrastructure perimeter.
- Keep async job store semantics unchanged unless search explicitly needs async jobs; initial search can be synchronous request/response with timeout.

## Suggested immediate next step

Two viable tracks; pick per priority:

- **Search (v0.16.0)** — the next net-new feature. Do the short design spike first: read Firecrawl's search reference in `docs/firecrawl-api-reference.md`; save sample SERP HTML for DuckDuckGo/Bing/Brave/Mojeek; confirm which engines are reachable through `draco-net` in the dev environment; decide the exact partial-failure and scrapeOptions-failure response shape; then implement the slices above.
- **Render-tier performance** — profile the CPU-bound hydration (backlog item 1) before committing to an optimization, so the work is evidence-driven rather than speculative.
