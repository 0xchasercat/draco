# Draco Roadmap — Firecrawl parity + metasearch

Status as of v0.10.0 (2026-07-06). This is the canonical, agreed plan; it supersedes ad-hoc
notes. Each release is dependency-ordered; slices within a release are ordered by dependency.

## Where we already are
- **scrape** — `POST /v1/scrape`, Firecrawl-shaped, `markdown`+`json` formats (v0.7.0).
- **map** — `POST /v1/map`, sitemap.xml + on-page hrefs (v0.8.0).
- **crawl** — `POST /v1/crawl` + status/cancel, async BFS jobs (v0.8.0).
- **mcp** — stdio + `/mcp`, tools `draco_scrape` (v0.8.0) + `draco_discover` (v0.10.0).
- **discovery** — `--format endpoints` / `POST /v1/discover`, ranked API catalog (v0.10.0).

So the remaining work is **one net-new engine (search)** plus **parity hardening** — not five
new builds.

## v0.11.0 — API alignment ("change your BASE_URL and it works")
Foundational: search's `scrapeOptions` and batch both reuse the scrape format/response layer,
so this lands first.

**Central design decision:** replace the coarse `OutputFormat { Markdown, Json, Both }`
(draco-core/src/lib.rs:77) with a **`FormatSet`** — the set of requested outputs
`{ markdown, html, rawHtml, links, json, endpoints }`. Both the CLI `--format` (repeatable)
and daemon `formats: []` parse into it; `to_firecrawl` emits each requested field.

Dependency-ordered slices:
- **Pre. Fetch outcome plumbing** — thread `{ final_url, status_code, content_type, body }`
  from draco-net up through `machine.rs` (today only the parsed content flows up; the raw
  body/status/CT are dropped). Prerequisite for rawHtml + metadata parity.
- **A. Contract (draco-types)** — add additive `html: Option<String>`,
  `raw_html: Option<String>` (serde `rawHtml`), `links: Option<Vec<String>>` to
  `ExtractionResult`, same `skip_serializing_if=None` pattern as `markdown`/`endpoints`.
- **B. FormatSet refactor (draco-core)** — introduce the set, thread through `Config` +
  `run_ladder`; existing 286 tests are the guard rail.
- **C. Populate formats** — `rawHtml` = fetch body (utf8-lossy); `html` = Tier 2
  `rendered_html` (tier2.rs:78, already captured) or Tier 0 fetched HTML, through a new
  `draco_static::clean_html` (strip script/style/noscript, reuse the a[href]/img[src]
  absolutizer at content.rs:389, honor onlyMainContent/include/excludeTags); `links` = new
  `extract_links(html, base)` reusing `base_href` (content.rs:466).
- **D. Metadata parity (draco-static)** — add `url` (final URL post-redirect), `statusCode`,
  `contentType` to the metadata map (content.rs:~1137). Existing keys already match Firecrawl
  (title/description/language/og*/keywords/robots/canonical/favicon/sourceURL). Verify casing
  against docs/firecrawl-api-reference.md.
- **E. Request-field parity** — `onlyMainContent` (expose existing behavior, default true),
  `includeTags`/`excludeTags`, `waitFor` (alias → `captureWindowMs`), `headers` (→ draco-net),
  `timeout`, `location` (accept+ignore for now). Unsupported → explicit **HTTP 422** with a
  "Draco is a DOM-only engine, no browser" message: `screenshot`, `screenshot@fullPage`,
  `actions`, `changeTracking`. `maxAge` → accept + documented **no-op** (we have no cache
  layer; note Firecrawl's real default is 24h, not the documented 0).
- **F. Crawl regex fix (crawl.rs)** — `includePaths`/`excludePaths` are currently
  case-sensitive **substring** (`path.contains`, crawl.rs:411/414); Firecrawl semantics are
  **regex against the URL pathname**, with `regexOnFullURL` matching the full URL. Compile to
  `Vec<Regex>` once at plan build (crawl.rs:278); fix the doc comment (crawl.rs:22). Real
  drop-in-compat bug. *Independent of A–E — parallelizable.*
- **G. Map parity (map.rs)** — robots.txt `Sitemap:` directive discovery; `ignoreSitemap` /
  `sitemapOnly` flags; flip `includeSubdomains` default to **true**; align `limit` cap.
  *Independent of A–E — parallelizable.*
- **I. CLI/REST verb alignment** — the CLI mirrors the REST surface, one mental model across
  both: `draco extract` → **`draco scrape`** (matches `/v1/scrape`); new `draco discover <url>`
  (≙ `/v1/discover`; today's `--format endpoints` sugar); new `draco map <url>` (≙ `/v1/map`,
  on top of slice G's callable `map_site`, serve-feature-gated). `extract` is **removed
  entirely** (no deprecated alias — one clean path). `--format` becomes repeatable and accepts
  both wire casing (`rawHtml`) and kebab (`raw-html`). `draco crawl`/`draco batch` arrive with
  v0.12's job semantics; `draco search` with v0.13.
- **H. MCP + tests + docs + ship** — extend `draco_scrape` formats enum; tests per slice;
  CHANGELOG + README (all `draco extract` examples → `draco scrape`); bump 0.11.0; gates; ship.

Confirmed decisions (2026-07-06): FormatSet fully replaces `OutputFormat` (delete, no
parallel enum); unknown request fields stay **accept+ignore** (leniency over Firecrawl's
strict-400 — friendlier drop-in); CLI verbs align to REST as above.

Delegation: F and G are isolated to single files and independent of the format chain — good
foreground-subagent candidates to run in parallel while the A→E chain (shared files:
draco-types, machine.rs, content.rs) is done sequentially to avoid collisions.

## v0.12.0 — batch scrape + webhooks
- `POST /v1/batch/scrape` — reuse the crawl async job store (~80% shared). **Scrape options are
  FLAT at top level**, not nested under `scrapeOptions` (differs from crawl — see reference §3).
- `GET /v1/batch/scrape/{id}` — pagination via `next` + `?skip=&limit=`, **10 MiB**
  (10 485 760 B) payload cap, status enum `scraping|completed|failed|cancelled`.
  `GET …/{id}/errors` → `{errors[], robotsBlocked[]}`.
- **Webhooks** — config `{ url|string, headers, metadata(string vals), events[] }`; 4-event
  lifecycle; filter uses bare names (`started/page/completed/failed`), emitted `type` uses
  prefixed (`crawl.*` / `batch_scrape.*`); retry +1/+5/+15 min, 10 s 2xx deadline. Wire into
  the existing crawl job too.
- SSE = optional Draco **extension** (Firecrawl uses webhooks+poll, not SSE).

## v0.13.0 — search (the flagship)
Fault-tolerant metasearch through draco-net's stealth stack. **The fan-out + consensus model
IS the maintenance-avoidance mechanism** — a handful of robust engines, any of which may rot
or captcha-wall, and consensus still returns good results. (Empirically zero-maintenance in a
prior implementation.) SearXNG is a **selector reference only**, not a dependency or a clone.
- Engine `trait` + a few per-engine SERP parsers (start: DuckDuckGo HTML, Bing, Brave, Mojeek);
  **Google best-effort, expected to fail often — that's fine, others cover the gap.**
- Tokio `JoinSet` fan-out; **consensus scoring** (SearXNG model: sum of `1/rank` across
  engines); tolerant of per-engine failure by design.
- `POST /v1/search` (`query` req, `limit` 5/max 100, `tbs`, `location`, `timeout` 60000,
  `scrapeOptions`) + `draco search`. Response: flat `data[]` with `title/description/url`
  flattened + scrape fields when `scrapeOptions.formats` non-empty. `scrapeOptions` pipes each
  SERP URL through draco-core's ladder — the natural Draco fit; depends on v0.11's format layer.

## MCP tools — riders, not standalone
`draco_map`, `draco_batch` (v0.12), `draco_search` (v0.13) added as each capability lands.

## Cross-cutting decisions
- **No pixel parity, ever.** DOM-only engine; screenshot/actions always 422, never faked.
- **Leniency over strictness** on unknown request fields (Firecrawl uses Zod `.strict()` → 400;
  we accept+ignore, which is friendlier for drop-in). Revisit only if strict parity is asked for.
- **SERP scraping vs search-engine ToS** is a known, accepted tradeoff (same posture as SearXNG).
