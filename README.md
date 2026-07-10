# Draco

A fast, stealth, **native-Rust web scraper** — a lighter alternative to Firecrawl
/ Browserbase. Point it at a URL and get clean **Markdown + metadata** back, using
a browser-faithful TLS/JA4 fingerprint to reach pages that block ordinary clients.
No Node, no headless-Chrome fleet, no per-request browser boot.

```sh
draco scrape https://example.com          # → clean Markdown on stdout
```

For a standard HTML page that's a single fingerprinted fetch + parse — typically
**~300 ms, no browser** — and the Markdown pipeline mirrors Firecrawl's
(deterministic main-content extraction + a Turndown/GFM-equivalent converter),
implemented natively in Rust. Client-rendered SPAs (whose content only appears
after JavaScript runs) are handled too, via
[render-then-Markdown](#client-rendered-spas--markdown-render-then-markdown) — no
headless browser fleet.

## What you get

- **`markdown`** — the page's main content as clean Markdown: headings, links
  (absolutized), lists, blockquotes, fenced code blocks (with language), and GFM
  tables. Boilerplate (nav / header / aside / footer / ads) is stripped, scripts
  and styles never leak, base64 images are elided.
- **`metadata`** — `title`, `description`, `language`, `canonical`, `favicon`,
  every `og:*` / `twitter:*` / `article:*` tag, plus `sourceURL`, `statusCode`,
  `contentType`.
- **`trace` + `timing`** — exactly which steps ran and where the milliseconds went.

## Install / build

```sh
git clone https://github.com/0xchasercat/draco && cd draco
cargo build --release
```

Build prerequisites (for wreq's BoringSSL + bindgen; and V8, only for the optional
`json` mode): `cmake`, a C/C++ compiler, `clang`/`libclang`, `perl`, `pkg-config`.
- Debian/Ubuntu: `apt install build-essential cmake clang libclang-dev perl pkg-config`
- Fedora: `dnf install gcc gcc-c++ cmake clang clang-devel llvm-devel perl pkgconf`
- macOS: Xcode Command Line Tools + `brew install cmake`

## Usage

```sh
# Default: URL → Markdown on stdout (great for piping)
draco scrape https://example.com > page.md

# Full envelope (markdown + metadata + trace) as JSON
draco scrape https://example.com --json --pretty

# Stealth + politeness
draco scrape https://example.com --proxy socks5://127.0.0.1:9050 --delay 500
```

Exit codes: `0` success · `1` error · `2` unsupported · `3` needs_browser.

### Optional: JSON-API extraction (`--format json`)

Beyond Markdown, Draco can extract the **structured data an SPA loads from its own
API** — a power feature for data-driven sites. It escalates through the cheapest
tier that yields data:

1. **Static embedded state** — `__NEXT_DATA__`, JSON-LD, `window.__NUXT__`.
2. **Next.js build-id replay** — fetch `/_next/data/<buildId>/…​.json` directly.
3. **Runtime interception** — boot an **in-process V8 isolate** (restored from a
   build-time DOM-engine snapshot in single-digit milliseconds, JIT on), let the
   page's JS hydrate, intercept the `fetch`/`XHR` it fires for its data, rank the
   intercepts, and replay the winner with the stealth client. The isolate is a
   *discovery oracle*, not a renderer — data requests are answered with a
   synthetic stub, and page JS has no host bindings: it cannot perform I/O.

```sh
draco scrape https://app.example.com --format json --pretty       # data[]
draco scrape https://app.example.com --format json --extract '$.props.pageProps'
draco scrape https://app.example.com --format both                # markdown + data
```

Flags: `--format <markdown|html|raw-html|links|json|endpoints|both>` (repeatable;
default `markdown`; `both` = `markdown`+`json`), `--json`, `--extract
<JSONPATH>`, `--no-main-content`, `--wait-for <ms>`, `--tier-max <0|1|2>`, `--proxy`, `--delay <ms>`, `--timeout <ms>`,
`--capture-window-ms <ms>`, `--ignore-robots`, `--allow-unsafe-replay`,
`--runtime-log`, `--pretty`. (`--no-jail` / `--strict-sandbox` are still accepted
for compatibility and are inert — see [Security model](#security-model-only-relevant-to-tier-2).)

Debugging a page that hydrates to nothing? `--runtime-log` (also on `discover`,
and as `runtimeLog` on the daemon/MCP) surfaces the isolate's page-side
diagnostics — swallowed exceptions, `console.error` lines, every brokered fetch
(`[raze.fetch] METHOD URL → STATUS (bytes, live|stub)`), failed chunk/module
loads, and why/when the capture window closed — all stamped `[+ms]` from capture
start, so the trace reads as a timeline: browser-devtools visibility, no browser.

### Client-rendered SPAs → Markdown (render-then-Markdown)

Some pages render their *content* only after JavaScript runs — the fetched HTML is
a thin shell (an empty `<div id="root">`). Draco handles these automatically: when
the initial parse finds almost no content and Tier 2 is permitted (the default),
it hydrates the shell in the same in-process V8 isolate, serializes the **live
DOM**, splices the shell's real `<head>` (title / Open Graph / canonical) onto the
hydrated `<body>`, and re-runs the exact same content engine over it. You get clean
Markdown from a client-rendered page with no headless browser — the trace shows a
`runtime.render` step and `source_tier: runtime_interception`.

```sh
draco scrape https://spa.example.com            # thin shell → hydrated Markdown
draco scrape https://spa.example.com --tier-max 1   # opt out: static shell only
```

This also covers **skeleton screens**: a page that ships lots of chrome but whose
content rails are still `Loading…` is detected as an incomplete render (regardless
of length) and escalated the same way. `Loading…` placeholder lines are always
stripped from the output, so that noise never reaches you even if the render pass
is capped (`--tier-max 1`) or can't improve the page.

**Pure-CSR SPAs: live data, safely.** Some SPAs ship no embedded state at all —
the content exists only behind the JSON APIs the page calls after it hydrates.
For exactly this escalation (and only it), the isolate's fetch broker switches to
**Render mode**: the page's *safe* data requests — `GET`/`HEAD`, and read-style
`POST`/`PUT` (GraphQL / JSON-RPC-shaped) — are fetched **live** through the same
stealth client and shared cookie jar, and the page sees the real
status/headers/body, including non-2xx, so a framework router runs its native
success/error paths. State-changing requests stay stubbed unless
`--allow-unsafe-replay`; streaming endpoints and analytics beacons are never
fetched live. `discover` and the JSON tier keep the record-and-stub Observe mode.
The capture window closes as soon as the page's **content** activity settles —
analytics/session-replay beacons are recorded but cannot pin the window open —
with a hard ceiling as backstop.

**External scripts & ES modules** are handled too. The isolate runs a page's
external `<script src>` and `<script type="module">` (with `import` / dynamic
`import()`), not just inline scripts. Script subresources are fetched **on
demand and concurrently** — chunk loads fan out on the isolate's event loop like
a browser's network stack — through the pooled stealth client and a
process-global immutable chunk cache (512 MiB RAM + 2 GiB disk), so a hashed SPA
chunk is fetched once across scrapes. Page JS itself still performs zero I/O:
every byte is brokered by the engine's ops.

A thin shell that can't be improved (hydration adds nothing, or the isolate is
unavailable) falls back to the static shell — never a crash, never a regression.

### Daemon mode (`draco serve`)

Run Draco as a **persistent HTTP daemon with a Firecrawl-compatible REST API** —
the process stays warm (no per-scrape binary spawn), and existing Firecrawl
clients can point at it unchanged:

```sh
draco serve                    # http://127.0.0.1:3002 (Firecrawl's default port)
draco serve --host 0.0.0.0 --port 8080 --max-concurrency 16
```

```sh
curl -X POST http://127.0.0.1:3002/v1/scrape \
  -H 'content-type: application/json' \
  -d '{"url": "https://spa.example.com", "formats": ["markdown"]}'
# → { "success": true, "data": { "markdown": …, "metadata": { "title", "sourceURL", … } } }
```

- `formats`: `"markdown"` (default) and/or `"json"` (the tiered JSON-API
  extraction, under `data.json`). Formats Draco doesn't produce yet (`html`,
  `rawHtml`, `links`, `screenshot`) are rejected with a clear `400`.
- Unknown Firecrawl fields (`onlyMainContent`, `waitFor`, …) are accepted and
  ignored; failures use the `{ "success": false, "error": … }` envelope
  (`502` upstream/network, `422` unsupported target, `400` bad request).
- Draco extensions per request: `tierMax`, `captureWindowMs`, `noJail`,
  `allowUnsafeReplay`, `ignoreRobots`, `proxy` — plus `timeout` (Firecrawl's).
  Server-wide defaults come from the `draco serve` flags.
- Every response carries a `draco` object (`sourceTier`, `timing`, `trace`) —
  the same honest execution report as the CLI envelope.
- `GET /health` → `{ "status": "ok", "version": … }`.

Concurrency is bounded (`--max-concurrency`, default 8); excess requests queue.
Warm-process SPA hydration answers in ~150 ms end-to-end on the local benchmark
fixture (fetch → hydrate → serialize → Markdown).

**Isolate concurrency.** Tier 2 scrapes run in **fresh, in-process V8
isolates** — one per job, restored from the build-time DOM snapshot in
single-digit milliseconds, so there is no per-request browser boot to amortize
and never any cross-scrape state, cookie, or DOM bleed. `--isolate-pool-size`
bounds how many isolates run concurrently (default `0` = auto ≈ CPU count);
excess Tier 2 work queues. (`--isolate-max-jobs` is accepted for compatibility
and inert — there are no long-lived workers to recycle.)

Beyond scraping, the daemon speaks two more Firecrawl endpoints:

- **`POST /v1/map`** — fast site URL discovery: merges `/sitemap.xml` (sitemap
  indexes followed one level) with the page's own links; same-host filtered
  (`includeSubdomains` opt-in), deduped, `search`-filtered, `limit`-capped.
  ```sh
  curl -X POST localhost:3002/v1/map -H 'content-type: application/json' \
    -d '{"url": "https://docs.example.com", "search": "guide"}'
  # → { "success": true, "links": [ … ] }
  ```
- **`POST /v1/crawl`** — async crawl jobs: a bounded same-host BFS (`limit`
  default 10, cap 100; `maxDepth` default 2; `includePaths`/`excludePaths`
  path filters) where every page runs the full extraction ladder — crawled
  SPAs hydrate like single scrapes. Frontier links are harvested from each
  page's Markdown (already absolutized; JS-injected links included when the
  render escalation ran). Poll `GET /v1/crawl/{id}` for
  `{ status, total, completed, data: [ per-page results ] }`; `DELETE` cancels.
  Jobs are in-memory and share the daemon's concurrency budget. Status is
  paginated (`?skip=&limit=`, `next` when more remains); `GET /v1/crawl/{id}/errors`
  lists per-page failures.
- **`POST /v1/batch/scrape`** — scrape a list of URLs as one async job. Scrape
  options are **flat** at the top level (`formats`, `onlyMainContent`,
  `includeTags`/`excludeTags`, `headers`, `waitFor`, …), applied to every URL;
  `ignoreInvalidURLs` drops non-http(s) URLs into an `invalidURLs` list instead
  of failing the request. URLs run in parallel, bounded by `--max-concurrency`.

  ```sh
  curl -X POST localhost:3002/v1/batch/scrape -H 'content-type: application/json' \
    -d '{"urls": ["https://a.example", "https://b.example"], "formats": ["markdown"]}'
  # → { "success": true, "id": "7", "url": "/v1/batch/scrape/7" }
  ```

  Poll `GET /v1/batch/scrape/{id}` (paginated `?skip=&limit=`, `next` when more
  remains) for `{ status, total, completed, creditsUsed, expiresAt, next, data }`;
  `GET /v1/batch/scrape/{id}/errors` lists per-URL failures; `DELETE` cancels.
- **Webhooks** — crawl and batch requests accept a `webhook` (a bare URL string
  or `{ url, headers, metadata, events }`). The job fires `started`, `page`
  (with the scraped document), `completed`, and `failed` events — payload
  `{ success, type, id, data, metadata }`, `type` prefixed by job kind
  (`crawl.page`, `batch_scrape.completed`). Delivery is fire-and-forget with a
  10s deadline and +1/+5/+15min retries; the endpoint is never robots-gated.

  ```sh
  curl -X POST localhost:3002/v1/crawl -H 'content-type: application/json' \
    -d '{"url": "https://site.example", "webhook": "https://my.app/hook"}'
  ```

### API discovery/replay (`endpoints` / `POST /v1/discover`)

Client-rendered pages load their content from their own JSON APIs. Draco's
Tier 2 isolate already intercepts every `fetch`/XHR to pick a replay winner —
**discovery** surfaces the *full ranked catalog* so you can see (and replay)
the APIs behind a SPA:

```sh
draco scrape https://shop.example --format endpoints --pretty   # catalog + replayed winner
```
```sh
curl -X POST localhost:3002/v1/discover -H 'content-type: application/json' \
  -d '{"url": "https://shop.example"}'
# → { "success": true, "endpoints": [ { "method","url","via","score","replayable","headers" }, … ],
#     "data": <replayed winner JSON | null> }
```

Each endpoint carries a `score` (higher = more likely the real data API) and a
`replayable` flag (clears the viability bar and is replay-safe — one eligibility
rule feeds both the catalog and the replay engine). Ranked best-first; the
analytics beacons and static assets sort to the bottom. On `/v1/scrape`,
`formats: ["endpoints"]` returns the catalog under `data.endpoints` and composes
with `markdown`/`json`.

### Web search (`draco search` / `POST /v1/search`)

Search the web through Draco's **own stealth HTTP stack** — no external search
API, and **no rendering** (SERP results are server-rendered HTML; booting the
isolate for them would be pure waste). Several engines are queried **in
parallel** and merged by **reciprocal-rank consensus**, so an engine that
captcha-walls, geo-blocks, or rots is a normal *partial* failure the survivors
absorb — the request only fails if **every** engine does. (This is the SearXNG
model — used as a selector/behavior reference, not a dependency or a wholesale
parser port.)

```sh
draco search "rust web scraper"                       # → ranked results (JSON)
draco search "rust web scraper" --limit 10
draco search "rust web scraper" --format markdown     # also scrape each result
```
```sh
curl -X POST localhost:3002/v1/search -H 'content-type: application/json' \
  -d '{"query": "rust web scraper", "limit": 5}'
# → { "success": true,
#     "data": [ { "title", "description", "url" }, … ],
#     "draco": { "engines": [ { "engine", "status", "results"? }, … ] } }
```

- Engine set: DuckDuckGo (HTML endpoint), Bing, Brave, Baidu, ZapMeta, Yandex —
  behind a swappable `SearchEngine` trait, each parser fixture-tested so parser
  drift needs no live search. Google/Mojeek-class blocks are expected and
  tolerated.
- Request (Firecrawl-shaped; unknown fields accepted-and-ignored): `query`
  (required), `limit` (default 5, 1–100), `tbs`, `location`, `timeout` (default
  60000), `scrapeOptions`, plus Draco `proxy`/`ignoreRobots`. Response is a flat
  `data[]` of `{ title, description, url }`; total engine failure → `502`,
  partial → consensus of survivors.
- **`scrapeOptions.formats`** runs each result URL through the scrape ladder and
  merges the `Document` fields (`markdown`/`html`/`rawHtml`/`links`/`json`/
  `metadata`) onto the hit — same `FormatSet`, bounded concurrency.

### MCP server (`draco mcp` / `POST /mcp`)

Draco's scraping is available as **Model Context Protocol tools** for agent
clients (Claude Desktop/Code, editors, orchestrators):

```sh
draco mcp                        # stdio transport (newline-delimited JSON-RPC)
```

```json
{ "mcpServers": { "draco": { "command": "draco", "args": ["mcp"] } } }
```

The same server is bound on the daemon at `POST /mcp` (minimal Streamable-HTTP
subset: single-message POST → single JSON response, `202` for notifications).
Three tools, all annotated read-only:
- `draco_scrape` (`url`, `formats: ["markdown"|"json"|"endpoints"]`, `tierMax`,
  `captureWindowMs`, `timeout`, `ignoreRobots`) — scrape to Markdown/JSON.
- `draco_discover` (`url`, `tierMax`, `captureWindowMs`, `timeout`,
  `ignoreRobots`, `allowUnsafeReplay`) — the ranked API-endpoint catalog + the
  replayed winner, for agents that want a page's data API.
- `draco_search` (`query`, `limit`, `tbs`, `location`, `timeout`, `formats`) —
  parallel multi-engine web search merged by reciprocal-rank consensus; with
  `formats` it also scrapes each result and merges the content onto the hit.

Tool-level failures come back as `isError` results the model can react to;
protocol misuse is a proper JSON-RPC error.

## Workspace layout

| Crate | Role |
|-------|------|
| `draco-types` | Wire + result contract (no I/O) |
| `draco-net` | Stealth TLS/JA4 HTTP client (wreq/BoringSSL): cookie jar, proxy, robots, backoff |
| `draco-static` | **Markdown + metadata extraction** (Firecrawl-parity) · JSON embedded-state · build-id replay |
| `draco-runtime` | Tier 2 **in-process V8 isolate** (JIT): real happy-dom DOM engine baked into a build-time V8 snapshot; `fetch`/`XHR` interception; Observe/Render fetch modes; concurrent async chunk loading |
| `draco-core` | Escalation state machine, challenge short-circuit, ranking, replay, chunk cache |
| `draco-cli` | The `draco` CLI + output contract |

## Feature flags

- **default (`tier2`, `serve`)** — everything: the V8 isolate for `--format json`
  runtime interception / render-then-Markdown, plus the `draco serve` daemon.
- **`serve`** — the persistent HTTP daemon (axum). Independent of `tier2`:
  `--no-default-features --features serve` exposes the same REST API with the
  ladder capped at the static tiers.
- **`--no-default-features`** — a lean build with **no V8/axum linked**.
  Markdown scraping and static/build-id JSON extraction still work; runtime
  interception reports `unsupported`. Smaller binary, faster build.

```sh
cargo build -p draco-cli --no-default-features   # lean, V8-free, axum-free
```

## Security model (only relevant to Tier 2)

Markdown scraping of a static page executes no page JavaScript. Tier 2 (runtime
interception / render-then-Markdown) does, so containment matters. Draco's
containment is the **V8 isolate itself**: the context has **no host-capability
bindings** — the only ops exposed to page JS record an intercepted request, load
a script chunk, log a diagnostic, sleep, and resolve URLs. There is no network,
filesystem, or process access; the only I/O page JS can *cause* is the fetches
the engine explicitly brokers (script subresources always; data requests only in
Render mode, under the mutation-safety policy). **This is the same class of
isolation Puppeteer/Playwright/jsdom rely on**, works identically on macOS and
Linux, and needs zero configuration. JIT is on (`--single-threaded`, so V8
spawns no background threads); the achieved posture shows in the `trace` as a
`runtime.sandbox` step (`isolate: in-process v8 (no host bindings)`).

The OS process jail of earlier releases (fork + userns/netns air-gap + seccomp +
Landlock) was **retired in v0.14**: its per-chunk blocking IPC was the engine's
throughput ceiling, and for hosted deployments the security perimeter belongs to
the infrastructure layer (stateless, ephemeral, unidirectional workers).
`--no-jail` and `--strict-sandbox` are accepted for CLI compatibility and are
inert.

Draco does **not** defeat JS challenge walls (Cloudflare/DataDome/…); a genuine
interstitial (blocking status + real challenge page) short-circuits to
`needs_browser`. A normal `200` behind a CDN is never treated as a challenge.

## Platforms

| Platform | Markdown scrape | JSON Tier 0/1 | Tier 2 isolate |
|----------|:---:|:---:|:---:|
| **Linux** `x86_64-gnu` | ✅ | ✅ | ✅ |
| **macOS** `aarch64-darwin` | ✅ | ✅ | ✅ |

Both are **first-class** — Tier 2 is the same in-process isolate on both, with
identical behavior and zero platform-specific configuration.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

On a memory-/disk-constrained box (CI containers, the ~4 GiB build sandbox),
run the gates through the guarded wrapper instead — it pins single-job builds
and refuses to start when disk is low rather than filling it and losing the
session:

```sh
bash scripts/gate.sh            # fmt + clippy + test, disk-guarded
bash scripts/reclaim.sh         # free regenerable build artifacts in a pinch
```

See **[docs/SANDBOX.md](docs/SANDBOX.md)** for the full runbook (and the
commit-before-build rule that makes a lost sandbox cost nothing).

## Compliance & intended use

Draco is for public data, properties you operate, and APIs you're permitted to
use. Defaults are polite (robots.txt respected, per-host rate limiting, bounded
retries). JA4/TLS emulation is for compatibility, not to defeat authentication or
access controls. You are responsible for compliance with target sites' Terms of
Service and applicable law.

## License

MIT OR Apache-2.0.
