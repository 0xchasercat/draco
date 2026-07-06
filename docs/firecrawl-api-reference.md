# Firecrawl v1 REST API — Exact Field Reference

Compiled 2026-07-06 for building a drop-in-compatible `/v1` daemon. Base URL: `https://api.firecrawl.dev/v1`. Auth: `Authorization: Bearer <key>` (HTTP bearer).

**Important context discovered during research:** As of mid-2026, Firecrawl's public docs site (docs.firecrawl.dev) has made **v2 the default/primary API**, and the standard `/api-reference/endpoint/*` URLs now render v2 documentation. The v1 docs still exist and are served correctly, but only from a parallel, unlisted set of pages at `/api-reference/v1-endpoint/*` (not in the site's sitemap/nav — these are legacy pages kept for backward-compatible links, each explicitly banner-labeled "A new v2 version of this API is now available"). v1 itself is **not deprecated/shut down** — `api.firecrawl.dev/v1` is fully live — but it is in "legacy, still-supported" status, not the actively marketed surface. This matters because many field-level details (especially exact defaults and the true `formats` enum) are stale or incomplete in the still-published v1 OpenAPI/doc pages relative to what the live backend actually accepts. Where I found such gaps, I cross-validated against the backend's own Zod validation source (`apps/api/src/controllers/v1/types.ts`) and the official JS SDK's v1 types, both pulled fresh from the `firecrawl/firecrawl` GitHub repo (`main` branch). Every such override is flagged explicitly below with **[GROUND TRUTH]**.

---

## 0. Sources consulted

1. `https://docs.firecrawl.dev/api-reference/v1-endpoint/scrape.md` — rendered v1 OpenAPI page (confirmed sourced from `api-reference/v1-openapi.json`)
2. `https://docs.firecrawl.dev/api-reference/v1-endpoint/search.md`
3. `https://docs.firecrawl.dev/api-reference/v1-endpoint/batch-scrape.md`
4. `https://docs.firecrawl.dev/api-reference/v1-endpoint/batch-scrape-get.md`
5. `https://docs.firecrawl.dev/api-reference/v1-endpoint/batch-scrape-get-errors.md`
6. `https://docs.firecrawl.dev/api-reference/v1-endpoint/map.md`
7. `https://docs.firecrawl.dev/api-reference/v1-endpoint/crawl-post.md`
8. `https://docs.firecrawl.dev/api-reference/v1-endpoint/crawl-get.md`
9. `https://docs.firecrawl.dev/api-reference/v1-endpoint/crawl-get-errors.md`
10. `https://docs.firecrawl.dev/api-reference/v1-endpoint/crawl-delete.md`
11. `https://docs.firecrawl.dev/api-reference/v1-endpoint/crawl-active.md`
12. `https://docs.firecrawl.dev/webhooks/overview.md`
13. `https://docs.firecrawl.dev/webhooks/events.md`
14. `https://raw.githubusercontent.com/firecrawl/firecrawl/main/apps/api/v1-openapi.json` (GitHub, `main` branch; last relevant commit 2026-01-31) — raw OpenAPI 3.0 spec, `servers: https://api.firecrawl.dev/v1`, confirmed byte-identical in content to what the live docs pages embed
15. `https://raw.githubusercontent.com/firecrawl/firecrawl/main/apps/api/openapi.json` (GitHub, `main` branch; last relevant commit 2026-06-15) — verified identical to `v1-openapi.json` for every path used in this report (`/scrape`, `/search`, `/map`, `/crawl`, `/batch/scrape`)
16. `https://raw.githubusercontent.com/firecrawl/firecrawl/main/apps/api/src/controllers/v1/types.ts` **[GROUND TRUTH]** — the actual server-side Zod request/response schemas that validate every v1 request (most authoritative source available; supersedes OpenAPI/docs where they conflict)
17. `https://raw.githubusercontent.com/firecrawl/firecrawl/main/apps/api/src/controllers/v1/search.ts` **[GROUND TRUTH]** — search controller source
18. `https://raw.githubusercontent.com/firecrawl/firecrawl/main/apps/api/src/controllers/v1/crawl-status.ts` **[GROUND TRUTH]** — crawl/batch-scrape status controller source (pagination, `next` URL construction, 10 MiB byte cap)
19. `https://raw.githubusercontent.com/firecrawl/firecrawl/main/apps/api/src/services/webhook/schema.ts` **[GROUND TRUTH]** — webhook config Zod schema
20. `https://raw.githubusercontent.com/firecrawl/firecrawl/main/apps/js-sdk/firecrawl/src/v1/index.ts` — official JS SDK v1 types + request-building code (used to confirm undocumented wire fields like `lang`/`country`/`filter`/`origin` on `/search`)
21. `https://docs.firecrawl.dev/sitemap.xml` — used to confirm v1 pages are absent from the current site navigation

All GitHub file fetches were from the `main` branch on 2026-07-06; treat as a snapshot, not a pinned release tag (Firecrawl's v1 OpenAPI file was last functionally touched 2026-01-31, so it is fairly stable, but the Zod source in `types.ts` is more actively edited).

---

## 1. `POST /v1/scrape`

### Request body

Top-level object: `{ url: string, ...ScrapeOptions, zeroDataRetention?: boolean }`. All scrape options are **flat/top-level**, not nested.

| Field | Type | Required | Default | Notes |
|---|---|---|---|---|
| `url` | string (URI) | **yes** | — | Must have a valid TLD or be an allowed local address (self-hosted test mode only). Scheme auto-prefixed to `http://` if missing. |
| `formats` | array of enum string | no | `["markdown"]` | See "formats enum" below — **the officially published enum under-documents the real accepted set.** |
| `onlyMainContent` | boolean | no | `true` | Strips headers/navs/footers. |
| `onlyCleanContent` | boolean | no | `false` | **[GROUND TRUTH, undocumented]** Not in OpenAPI/docs at all. |
| `includeTags` | array of string | no | — | CSS-selector-like tag list to keep. |
| `excludeTags` | array of string | no | — | CSS-selector-like tag list to drop. |
| `headers` | object (string→string) | no | — | Sent with the scrape request (cookies, UA, etc). |
| `waitFor` | integer (ms) | no | `0` | **[GROUND TRUTH]** Server caps this at `max 60000`; not mentioned in docs. Must be ≤ `timeout / 2` (validated). |
| `timeout` | integer (ms) | no | `30000` | **[GROUND TRUTH]** `min(1000)`. Auto-bumped to 60000 by the server if `json`/`extract`/`changeTracking` formats or `jsonOptions`/`extract` are used and still at the 30000 default; bumped to 120000 if `proxy` is `stealth`/`enhanced`/`auto` and still at default; bumped to 300000 if `agent` is set. |
| `mobile` | boolean | no | `false` | Emulates mobile device. |
| `skipTlsVerification` | boolean | no | `false` | |
| `parsePDF` | boolean | no | `true` | If `true`: PDF converted to markdown, billed per page. If `false`: PDF returned base64-encoded, flat 1 credit. |
| `jsonOptions` | object `{schema?, systemPrompt?, prompt?}` | no | — | Newer name for LLM-extraction config; `schema` must be JSON Schema. **[GROUND TRUTH]** Server currently treats `extract` as the deprecated predecessor of `jsonOptions` (comment: "Deprecate this to jsonOptions") — both accepted, same shape, mutually exclusive with their respective `formats` entry (`extract`/`json`). |
| `actions` | array of action objects | no | — | See "Actions" below. Total computed wait time (`waitFor` + all `wait` actions) capped server-side. |
| `location` | object `{country?, languages?}` | no | country defaults to `US`/`us-generic` internally | `country`: ISO 3166-1 alpha-2, pattern `^[A-Z]{2}$` in the public schema (server also accepts/normalizes lowercase and a few special values internally). `languages`: array of BCP-47-ish strings, e.g. `en-US`. |
| `removeBase64Images` | boolean | no | `true` | Replaces base64 image data with a placeholder, keeps alt text. |
| `blockAds` | boolean | no | `true` | Also blocks cookie popups. |
| `proxy` | enum: `basic` \| `enhanced` \| `auto` | no | `basic` | **[GROUND TRUTH]** Server Zod enum is actually `basic \| stealth \| enhanced \| auto` — `stealth` is a legacy alias kept for backward compatibility; `enhanced` is the current canonical name (added as an alias for `stealth` in a 2026-01-30 commit). `enhanced`/`auto` cost up to 5 credits/request if triggered. |
| `changeTrackingOptions` | object | no | — | Only meaningful if `changeTracking` in `formats` (which itself requires `markdown` also be present). Fields: `modes` (array of `git-diff`\|`json`), `schema` (JSON Schema, for `json` mode), `prompt` (string), `tag` (string\|null, default `null` — separates change-tracking history into branches). |
| `maxAge` | integer (ms) | no | **`86400000` (24h)** | **[GROUND TRUTH — DISCREPANCY]** The published OpenAPI/docs claim `default: 0` (caching disabled by default). The live server-side Zod schema (`baseScrapeOptions`) actually defaults this to `1 * 24 * 60 * 60 * 1000` = 86,400,000 ms. If you rely on the docs' stated default, cached results may be returned when you didn't expect it. Returns a cached page if younger than this age; else re-scrapes. |
| `storeInCache` | boolean | no | `true` | If `false`, page isn't stored in Firecrawl's index/cache. Forced to `false` automatically when `actions` or `headers` are used (per docs prose). |
| `agent` | object `{model, prompt, sessionId?, waitBeforeClosingMs?}` | no | — | **[GROUND TRUTH, undocumented in OpenAPI]** FIRE-1 agent config; mutually exclusive with setting the same agent model in `jsonOptions.agent`. |
| `geolocation` | object `{country?, languages?}` | no | — | **[GROUND TRUTH]** Deprecated predecessor of `location`. |
| `fastMode` | boolean | no | `false` | **[GROUND TRUTH, undocumented]** |
| `zeroDataRetention` | boolean | no | `false` | Documented only in the embedded YAML (not the top-level schema summary). Enables ZDR; requires contacting `help@firecrawl.dev` to activate for your team. |
| `useMock`, `__experimental_cache`, `__searchPreviewToken`, `__experimental_omce`, `__experimental_omceDomain`, `__forceFirePDF` | various | no | — | **[GROUND TRUTH]** Internal/experimental/debug-only fields present in the validation schema. Treat as unstable/unsupported; do not build compatibility around these. |

Request validation is a Zod `.strict()` object — **unknown top-level fields are rejected** (HTTP 400), so a compatible daemon should reject unrecognized fields too if strict parity is the goal.

#### `formats` enum — full picture

Publicly documented in the live v1 OpenAPI/doc page (8 values):
```
markdown | html | rawHtml | links | screenshot | screenshot@fullPage | json | changeTracking
```

**[GROUND TRUTH]** The actual server-side `Format` type / `baseScrapeOptions.formats` Zod enum (used to validate every `/v1/scrape`, `/v1/batch/scrape`, `/v1/crawl` request) accepts **13 values**:
```
markdown | html | rawHtml | links | screenshot | screenshot@fullPage | extract | json | summary | changeTracking | branding | product | menu
```
Differences from the published docs enum: `extract` (legacy predecessor of `json`), `summary`, `branding`, `product`, `menu` are all real, currently-accepted values not shown in the public `/scrape` formats documentation. Constraints found in source: you may not specify both `screenshot` and `screenshot@fullPage` together; `changeTracking` requires `markdown` also be present in the array.

Note: GitHub's `main`-branch `v1-openapi.json`/`openapi.json` files additionally list `branding` in their formats enum (added by commit "Implement Branding Format", 2025-11-03) even though the *rendered* live docs page for `/v1/scrape` specifically does not show it in its example enum — this is an inconsistency between two views of nominally the same spec file that I could not fully resolve from docs alone; the Zod ground-truth in item 16 above independently confirms `branding` (and `product`/`menu`/`summary`/`extract`) are real accepted values, so treat the fuller 13-value list as authoritative.

The `/search` endpoint's `scrapeOptions.formats` has its own slightly different accepted enum (see Section 2) — it additionally lists `product`/`menu` in the ground-truth schema but the public OpenAPI shows `extract` only (no `changeTracking`, no `branding`/`summary` in that particular sub-schema).

#### Actions array — item shapes

Each item is a discriminated union on `type`:

| `type` | Required extra fields | Optional extra fields | Notes |
|---|---|---|---|
| `wait` | — | `milliseconds` (int ≥1), `selector` (string) | Wait fixed time or for selector. |
| `screenshot` | — | `fullPage` (bool, default `false`), `quality` (int 1–100) | Screenshot URL(s) land in `actions.screenshots`. |
| `click` | `selector` (string) | `all` (bool, default `false`) | `all: true` clicks every match, no error if none match. |
| `write` | `text` (string) | — | Must `click` to focus first; types char-by-char. |
| `press` | `key` (string) | — | e.g. `"Enter"`. |
| `scroll` | — | `direction` (`up`\|`down`, default `down`), `selector` (string) | |
| `scrape` | — | — | Captures current page URL+HTML into `actions.scrapes`. |
| `executeJavascript` | `script` (string) | — | Return values land in `actions.javascriptReturns`. |
| `pdf` | — | `format` (enum: `A0`..`A6`,`Letter`,`Legal`,`Tabloid`,`Ledger`; default `Letter`), `landscape` (bool, default `false`), `scale` (number, default `1`) | **[found in live docs, not in GitHub raw v1-openapi.json — newer addition]** Generated PDFs land in `actions.pdfs`. |

### Response body

`{ success: boolean, data: Document, warning?: string, scrape_id?: string }` (error case: `{success: false, error: string, code?, details?}`).

`data` (the `Document` object) fields:

| Field | Type | Notes |
|---|---|---|
| `markdown` | string | Present when `markdown` in `formats` (default). |
| `html` | string, nullable | Present if `html` in `formats`. Cleaned: strips `<script>`/`<style>`/`<noscript>`/`<meta>`/`<head>`, absolutizes relative URLs, resolves `srcset`. Respects `onlyMainContent`/`includeTags`/`excludeTags`. |
| `rawHtml` | string, nullable | Present if `rawHtml` in `formats`. Completely unmodified. |
| `links` | array of string | Present if `links` in `formats`. |
| `images` | array of string | **[GROUND TRUTH, undocumented in OpenAPI]** |
| `screenshot` | string, nullable | URL; present if `screenshot`/`screenshot@fullPage` in `formats`. **Expires after 24 hours** and can no longer be downloaded after that. |
| `json` / `extract` | any | Present if `json`/`extract` in `formats` (or `jsonOptions`/`extract` given); the extracted object per your schema/prompt. `llm_extraction` is an older/alternate name for this same concept seen in the OpenAPI response schema (labelled "Displayed when using LLM Extraction"). |
| `summary` | string | **[GROUND TRUTH, undocumented in OpenAPI]** Present if `summary` in `formats`. |
| `branding` | object (`BrandingProfile`) | **[GROUND TRUTH, undocumented in OpenAPI for base /scrape]** Present if `branding` in `formats`. Nested shape (from OpenAPI's broader schema definition elsewhere): `logo` (string\|null), `fonts` (array of `{family}`), `colors`, `typography`, `spacing`, `components`, `icons`, `images`, `animations`, `layout`, `tone` (all open/`additionalProperties: true` objects) — I could not find a fully itemized field list for `BrandingProfile`'s sub-objects; treat as an open bag of brand-analysis data. |
| `product` | object (`ProductProfile`) | **[GROUND TRUTH, undocumented anywhere in public docs]** Present if `product` in `formats`. Could not find the field-level shape of `ProductProfile` in any doc; type import only (`../../types/product`). **Not confirmed beyond existence.** |
| `menu` | object (`MenuProfile`) | **[GROUND TRUTH, undocumented anywhere in public docs]** Present if `menu` in `formats`. Same caveat as `product` — existence confirmed, internal shape **not confirmed**. |
| `actions` | object, nullable | Only present if request had an `actions` array. Sub-fields: `screenshots` (array of URL string), `scrapes` (array of `{url, html}`), `javascriptReturns` (array of `{type, value}`), `pdfs` (array of string — generated PDF URLs/data). |
| `changeTracking` | object, nullable | Only present if `changeTracking` in `formats`. `previousScrapeAt` (ISO date-time \| null), `changeStatus` (enum: `new`\|`same`\|`changed`\|`removed`), `visibility` (enum: `visible`\|`hidden`), `diff` (git-style diff string, only in `git-diff` mode), `json` (object, only in `json` mode — before/after key comparison). |
| `warning` | string, nullable | Issues during LLM extraction, etc. |
| `serpResults` | object `{title, description, url}` | **[GROUND TRUTH, undocumented]** Present in some code paths (search-related scrape flows). |
| `metadata` | object | See table below. |

`metadata` object — **complete field list** (cross-referenced from server `Document.metadata` type; the public OpenAPI only names a handful explicitly plus a generic "any other metadata" catch-all):

| Field | Type | Notes |
|---|---|---|
| `title` | string (may appear as string\|array in some responses) | |
| `description` | string (may appear as string\|array) | |
| `language` | string, nullable | |
| `keywords` | string (docs show string\|array of string) | |
| `robots` | string | |
| `ogTitle`, `ogDescription`, `ogUrl`, `ogImage`, `ogAudio`, `ogDeterminer`, `ogLocale`, `ogSiteName`, `ogVideo` | string | Open Graph tags. |
| `ogLocaleAlternate` | array of string | |
| `favicon` | string | Seen in live docs examples; not in the base OpenAPI schema block. |
| `dcTermsCreated`, `dcDateCreated`, `dcDate`, `dcTermsType`, `dcType`, `dcTermsAudience`, `dcTermsSubject`, `dcSubject`, `dcDescription`, `dcTermsKeywords` | string | Dublin Core metadata. (Note: OpenAPI's older wording used `dctermsCreated`/`dctermsType`/etc. — mixed casing exists across sources; the Zod ground-truth uses `dcTermsCreated` etc. with capital T.) |
| `modifiedTime`, `publishedTime` | string | |
| `articleTag`, `articleSection` | string | |
| `url` | string | Final URL after redirects (distinct from `sourceURL`). |
| `sourceURL` | string (URI) | Originally requested URL. |
| `statusCode` | integer | HTTP status code of the page. |
| `scrapeId` | string | **[GROUND TRUTH, undocumented in base schema, seen in webhook examples]** |
| `error` | string, nullable | Error message if the page scrape had an issue. |
| `numPages` | integer | For PDFs: pages actually parsed (capped by parser's maxPages). |
| `totalPages` | integer | For PDFs: true page count before capping; omitted if undeterminable; `totalPages > numPages` signals truncation. |
| `contentType` | string | MIME type, e.g. `text/html`, `application/pdf`. |
| `timezone` | string | Inferred by the scraping engine, when available. |
| `proxyUsed` | enum `basic` \| `stealth` (ground truth type; docs/webhook examples also show `"basic"` literally) | |
| `cacheState` | enum `hit` \| `miss` | |
| `cachedAt` | string (ISO date-time) | |
| `creditsUsed` | number | |
| `postprocessorsUsed` | array of string | **[GROUND TRUTH, undocumented]** |
| `indexId` | string | **[GROUND TRUTH, undocumented]** "ID used to store the document in the index (GCS)" per source comment. |
| `concurrencyLimited` | boolean | Whether throttled by team concurrency limits. |
| `concurrencyQueueDurationMs` | number | Time spent queued; only present when `concurrencyLimited` is true. |
| *(any other key)* | string or array of string | Catch-all for arbitrary HTML meta tags not explicitly enumerated above. |

**Error surfaces for `/v1/scrape`:** `402` `{error: "Payment required to access this resource."}`, `429` `{error: "Request rate limit exceeded. Please wait and try again later."}`, `500` `{error: "An unexpected error occurred on the server."}`. A failed-but-200 style response with `data.metadata.error` populated is also possible per-page (distinct from a hard HTTP error).

---

## 2. `POST /v1/search`

### Request body

Top-level object is a Zod `strictObject` — **unknown fields rejected**.

| Field | Type | Required | Default | Notes |
|---|---|---|---|---|
| `query` | string | **yes** | — | Supports operators: `"exact phrase"`, `-exclude`, `site:`, `inurl:`, `allinurl:`, `intitle:`, `allintitle:`, `related:`. |
| `limit` | integer | no | `5` | `1`–`100` (ground truth: `.max(100)`, positive int). |
| `tbs` | string | no | — | Time filter: `qdr:h`/`qdr:d`/`qdr:w`/`qdr:m`/`qdr:y`, or custom `cdr:1,cd_min:MM/DD/YYYY,cd_max:MM/DD/YYYY`. |
| `location` | string | no | — | Free-text geo target, e.g. `"Germany"` or `"San Francisco,California,United States"`. See `firecrawl.dev/search_locations.json` for the supported list. |
| `timeout` | integer (ms) | no | `60000` | |
| `ignoreInvalidURLs` | boolean | no | `false` | Drops invalid-for-Firecrawl URLs from results instead of failing the whole request; **not** surfaced back in the response body per the OpenAPI schema for this endpoint (contrast with batch/scrape's `invalidURLs` field). |
| `scrapeOptions` | object | no | `{}` (formats defaults to `[]`, i.e. no scraping — just search-result metadata is returned) | Extends `BaseScrapeOptions`; its own `formats` enum in the public schema is `markdown\|html\|rawHtml\|links\|screenshot\|screenshot@fullPage\|json\|extract` (8 values, no `changeTracking`/`branding`/`summary` shown). **[GROUND TRUTH]** actual accepted enum for this sub-schema is `markdown\|html\|rawHtml\|links\|screenshot\|screenshot@fullPage\|extract\|json\|product\|menu` (10 values — includes `product`/`menu`, excludes `summary`/`changeTracking`/`branding`). |
| `filter` | string | no | — | **[GROUND TRUTH — undocumented anywhere in OpenAPI/docs]** Accepted and read by the server (`req.body.filter`); purpose not documented publicly. |
| `lang` | string | no | `"en"` | **[GROUND TRUTH — undocumented]** Accepted and read by the server; older/alternate way to influence search language, likely predates `location`. |
| `country` | string | no | `"us"` if neither `country` nor `location` given, else `undefined` | **[GROUND TRUTH — undocumented]** Accepted and read by the server. |
| `origin` | string | no | `"api"` | **[GROUND TRUTH — SDK telemetry field]** Identifies the calling SDK (e.g. `js-sdk@1.x.x`); harmless to omit; official SDKs always send it. |
| `integration` | string/enum | no | `null` | **[GROUND TRUTH — undocumented]** Present in schema; exact enum values not confirmed. |
| `__searchPreviewToken` | string | no | — | **[GROUND TRUTH — internal, unlisted]** Used for an internal unauthenticated search-preview mode; not part of the public contract. |

There is **no `sources`/`categories` parameter and no separate `news`/`images` sections** in v1 — that is a v2-only concept (v2 introduced multi-source search with `sources: ["web","news","images"]` and correspondingly-typed result sections). v1 `/search` returns one flat array only.

### Response body

`{ success: boolean, data: SearchResultItem[], warning?: string, id?: string }` (error: `408` timeout or `500`, each `{success: false, error: string}`).

Each `data[]` item (fields are **flattened directly onto the result item**, not nested under a `web`/`result` sub-key):

| Field | Type | Notes |
|---|---|---|
| `title` | string | From the search result itself. |
| `description` | string | From the search result itself. |
| `url` | string | |
| `markdown` | string, nullable | Present only if scraping was requested via `scrapeOptions` (i.e., `formats` non-empty). |
| `html` | string, nullable | Present if `html` requested. |
| `rawHtml` | string, nullable | Present if `rawHtml` requested. |
| `links` | array of string | Present if `links` requested. |
| `screenshot` | string, nullable | URL; expires after 24h, same as scrape endpoint. |
| `metadata` | object | Smaller field set than the full scrape metadata: `title`, `description`, `sourceURL`, `statusCode`, `numPages`, `totalPages`, `error` are the ones explicitly documented in the OpenAPI for this endpoint's result item — presumably the fuller scrape metadata list from Section 1 also applies when scraping is actually performed, but I could not independently confirm every extra field appears here (**not fully confirmed** beyond the documented subset). |

If `scrapeOptions.formats` is left empty (the default), only `title`/`description`/`url` are populated — no scrape fields.

---

## 3. `POST /v1/batch/scrape` and `GET /v1/batch/scrape/{id}`

### `POST /v1/batch/scrape` — request

Top-level shape: `{ urls: string[], ...ScrapeOptions, webhook?, maxConcurrency?, ignoreInvalidURLs?, zeroDataRetention? }`. **Scrape options are flattened at the top level, exactly like `/v1/scrape` — there is no nested `scrapeOptions` object for this endpoint** (this differs from `/v1/crawl`, which does nest scrape options under `scrapeOptions`). Confirmed both from the OpenAPI doc (no `scrapeOptions` key appears anywhere in its schema) and from the server-side Zod (`batchScrapeRequestSchemaBase = baseScrapeOptions.extend({urls, ...})`).

| Field | Type | Required | Default | Notes |
|---|---|---|---|---|
| `urls` | array of string (URI) | **yes** | — | **[GROUND TRUTH]** `.min(1)` — must be non-empty. |
| *(all `ScrapeOptions` fields from Section 1)* | — | no | same as Section 1 | `formats`, `onlyMainContent`, `includeTags`, `excludeTags`, `headers`, `waitFor`, `timeout`, `mobile`, `skipTlsVerification`, `parsePDF`, `jsonOptions`, `actions`, `location`, `removeBase64Images`, `blockAds`, `proxy`, `changeTrackingOptions`, `maxAge` (same 24h-default discrepancy noted in Section 1), `storeInCache`. |
| `webhook` | object or string | no | — | See Section 6. Bare string is shorthand for `{url: <string>}`. |
| `maxConcurrency` | integer | no | team's concurrency limit | Per-job concurrency override. |
| `ignoreInvalidURLs` | boolean | no | `false` | If `true`, invalid URLs are dropped and reported in `invalidURLs` instead of failing the whole request. |
| `zeroDataRetention` | boolean | no | `false` | Requires contacting `help@firecrawl.dev`. |
| `appendToId` | UUID string | no | — | **[GROUND TRUTH, undocumented in OpenAPI]** Appends new URLs to an existing batch scrape job. |

### `POST /v1/batch/scrape` — response

`{ success: boolean, id: string, url: string, invalidURLs?: string[] }`. `invalidURLs` is only present (even as an empty array) when `ignoreInvalidURLs` was `true`; otherwise the field is `undefined`/absent.

### `GET /v1/batch/scrape/{id}` — response (status polling)

Path param: `id` (UUID). Response shape `BatchScrapeStatusResponseObj`:

| Field | Type | Notes |
|---|---|---|
| `status` | string enum | Documented as `scraping`\|`completed`\|`failed`; **[GROUND TRUTH]** actual enum is `scraping`\|`completed`\|`failed`\|`cancelled` (4 values). |
| `total` | integer | Total pages attempted. |
| `completed` | integer | Pages successfully scraped so far. |
| `creditsUsed` | integer | |
| `expiresAt` | string (ISO date-time) | |
| `next` | string, nullable | URL to fetch the next page of results; present whenever the job isn't `completed` **or** the current payload exceeds 10 MiB (**[GROUND TRUTH]** exactly `10485760` bytes, computed via `JSON.stringify(scrape).length` summed across documents). Format: `{scheme}://{host}/v1/batch/scrape/{id}?skip={N}&limit={optional, echoes caller's limit}`. **[GROUND TRUTH, undocumented]** `GET` accepts optional `?skip=<int>` and `?limit=<int>` query params for manual pagination — not mentioned anywhere in the OpenAPI spec. |
| `data` | array of Document | Same `Document` shape as Section 1's scrape response `data`, i.e. each item can have `markdown`/`html`/`rawHtml`/`links`/`screenshot`/`metadata`/etc. depending on requested `formats`. |

A distinct failed-response shape also exists at the type level: `{success: false, status: "failed", error: string, completed, total, creditsUsed, expiresAt, data}` (top-level failure, not just per-document).

### `GET /v1/batch/scrape/{id}/errors`

`{ errors: {id, timestamp?, url, error}[], robotsBlocked: string[] }`. `robotsBlocked` lists URLs skipped due to `robots.txt`.

### Error surfaces (both batch endpoints)

`402` payment required, `429` rate limited, `500` server error — same generic `{error: string}` shape as scrape.

---

## 4. `POST /v1/map`

### Request body

| Field | Type | Required | Default (docs) | Default **[GROUND TRUTH]** | Notes |
|---|---|---|---|---|---|
| `url` | string (URI) | **yes** | — | — | Base URL to map from. |
| `search` | string | no | — | — | Filters/ranks discovered links by a search query; "Alpha phase... limited to 500 search results" per docs prose, but "if map finds more results, there is no limit applied." |
| `ignoreSitemap` | boolean | no | `true` | **`false`** | **DISCREPANCY.** The published OpenAPI/docs claim the sitemap is ignored by default (`true`); the live server-side Zod schema (`crawlerOptions.ignoreSitemap`, inherited by map) defaults it to `false`, meaning the sitemap **is** consulted by default. This is a meaningful, testable difference from what the docs say. |
| `sitemapOnly` | boolean | no | `false` | `false` (confirmed matches) | Only return links found in the sitemap. |
| `includeSubdomains` | boolean | no | `true` | `true` (confirmed) | |
| `limit` | integer | no | `5000` | `5000` (confirmed) | **DISCREPANCY on cap:** docs state `maximum: 30000`; **[GROUND TRUTH]** the server's `MAX_MAP_LIMIT = 100000`, and `mapRequestSchema.limit` is `.min(1).max(MAX_MAP_LIMIT)` — i.e. the real ceiling is 100,000, not 30,000. |
| `timeout` | integer (ms) | no | none (no timeout by default) | matches | |
| `location` | object `{country?, languages?}` | no | country `US` | matches shape | Same shape as scrape's `location`. |
| — no `sitemap` enum param — | | | | | v1 does **not** have the unified `sitemap: "include"\|"skip"\|"only"` string enum that v2 introduced; v1 only has the two separate booleans `ignoreSitemap`/`sitemapOnly` shown above. (Confirmed by diffing v1 vs. the current v2 `/map` docs, which added `sitemap` enum + `sitemapCacheBypass`.) |
| `includeSubdomains`, `ignoreQueryParameters` (map has its own default, `true`, distinct from crawl's `false`) | | | | | **[GROUND TRUTH, undocumented]** `ignoreQueryParameters` exists on map with default `true` (crawl's equivalent field defaults `false`). |
| `filterByPath` | boolean | no | — | `true` | **[GROUND TRUTH, undocumented anywhere]** |
| `useIndex` | boolean | no | — | `true` | **[GROUND TRUTH, undocumented in OpenAPI, but present in the official JS SDK's `MapParams` type]** |
| `ignoreCache` | boolean | no | — | `false` | **[GROUND TRUTH, undocumented]** |
| `ignoreRobotsTxt` | boolean | no | — | `false` | **[GROUND TRUTH, undocumented]** Inherited from shared `crawlerOptions`. |
| `headers` | object (string→string) | no | — | — | **[GROUND TRUTH, undocumented]** |
| `useMock` | string | no | — | — | **[GROUND TRUTH — internal/debug]** Not part of the public contract. |

The v1 map request/response has, per an inline source comment, "been transitioned to v2/types.ts while maintaining backwards compatibility" — meaning v1's `/map` route is now a compatibility shim over v2 map logic, which explains why several v2-era fields (`useIndex`, `ignoreCache`, `filterByPath`) are present in the v1 request schema despite never having been publicly documented for v1.

### Response body

`{ success: boolean, links: string[], scrape_id?: string }` (error: `{success: false, error}`).

**`links` is an array of plain strings (URLs), not objects** — confirmed independently by the OpenAPI `MapResponse` schema (`type: array, items: {type: string}`) and the server's `MapResponse` TypeScript type (`links: string[]`). This is a meaningful difference to preserve for drop-in compatibility, since a hypothetical v2-alike design might return `{url, title, description}` objects instead.

---

## 5. `POST /v1/crawl` and `GET /v1/crawl/{id}`

### `POST /v1/crawl` — request

| Field | Type | Required | Default | Notes |
|---|---|---|---|---|
| `url` | string (URI) | **yes** | — | Base URL. Server validates `getURLDepth(url) <= maxDepth` at request time (rejects if the seed URL's own path depth already exceeds `maxDepth`). |
| `includePaths` | array of string | no | `[]` | **Regex**, not substring. Docs explicitly: *"URL pathname regex patterns that include matching URLs... e.g. `includePaths: ["blog/.*"]`"*. Matched against the URL **pathname only** by default. |
| `excludePaths` | array of string | no | `[]` | Same regex-against-pathname semantics as above, inverted. |
| `regexOnFullURL` | boolean | no | `false` | When `true`, `includePaths`/`excludePaths` regexes match against the **full URL including query string** instead of just the pathname. |
| `maxDepth` | integer | no | `10` | Max slashes in the pathname of any crawled URL, counted from the base. |
| `maxDiscoveryDepth` | integer | no | — (unset/unbounded unless provided) | Depth counted by **discovery order**, not URL structure; root + sitemapped pages are discovery-depth 0. |
| `ignoreSitemap` | boolean | no | `false` | Confirmed consistent between docs and ground truth for **crawl** (unlike map, where this diverges). |
| `ignoreQueryParameters` | boolean | no | `false` | Avoid re-scraping same path with different/no query params. (Note: map's equivalent field defaults `true`, not `false`.) |
| `limit` | integer | no | `10000` | Max pages to crawl. |
| `allowBackwardLinks` | boolean | no | `false` | **Deprecated** — docs literally mark it "⚠️ DEPRECATED: Use 'crawlEntireDomain' instead." Still functional; if `crawlEntireDomain` is also given, its value is copied onto `allowBackwardLinks` internally before use. |
| `crawlEntireDomain` | boolean | no | `false` (docs); **[GROUND TRUTH]** no explicit default in Zod (`.optional()`, effectively `undefined` unless `allowBackwardLinks` was set) | `false`: only crawl child paths. `true`: crawl any internal link (siblings/parents too). |
| `allowExternalLinks` | boolean | no | `false` | Follow links to other domains. |
| `allowSubdomains` | boolean | no | `false` | Follow links to subdomains of the main domain. |
| `delay` | number (seconds) | no | — | **[GROUND TRUTH]** `.max(60)` — "cannot exceed 60 seconds" per validation message. Respects target site rate limits; if omitted, crawler may fall back to `robots.txt` crawl-delay directive (per SDK doc comment). |
| `maxConcurrency` | integer | no | team's concurrency limit | |
| `webhook` | object or string | no | — | See Section 6. |
| `scrapeOptions` | object (`ScrapeOptions`, Section 1 shape) | no | full scrape-options defaults (`baseScrapeOptions.parse({})`) | **This IS nested**, unlike batch/scrape. |
| `zeroDataRetention` | boolean | no | `false` | |
| `ignoreRobotsTxt` | boolean | no | `false` | **[GROUND TRUTH, undocumented in OpenAPI]** |
| `deduplicateSimilarURLs` | boolean | no | `true` | **[GROUND TRUTH, undocumented in OpenAPI]** (present as `deduplicateSimilarURLs` in both server schema and the JS SDK's `CrawlParams` type). |

### `POST /v1/crawl` — response

`{ success: boolean, id: string, url: string }` (job kickoff; poll via GET).

### `GET /v1/crawl/{id}` — response

Path param: `id` (UUID). Identical shape to batch/scrape status (`CrawlStatusResponseObj`):

| Field | Type | Notes |
|---|---|---|
| `status` | enum | Docs say `scraping`\|`completed`\|`failed`; **[GROUND TRUTH]** actually 4 values including `cancelled`. |
| `total` | integer | |
| `completed` | integer | |
| `creditsUsed` | integer | |
| `expiresAt` | string (ISO date-time) | |
| `next` | string, nullable | Same 10 MiB / not-yet-completed trigger and `?skip=&limit=` URL format as batch/scrape (Section 3), just pointed at `/v1/crawl/{id}` instead. |
| `data` | array of Document | Same shape as scrape's `data`. |

`GET /v1/crawl/{id}` also accepts the same undocumented `?skip=`/`?limit=` query params as batch/scrape's GET.

### `GET /v1/crawl/{id}/errors`

Same shape as batch/scrape's errors endpoint: `{errors: [...], robotsBlocked: [...]}`.

### `DELETE /v1/crawl/{id}` (bonus — cancellation, not explicitly requested but part of parity)

Response `200`: `{status: "cancelled"}`. `404`: `{error: "Crawl job not found."}`. `500`: generic error.

### `GET /v1/crawl/active` (bonus)

`{ success: true, crawls: [{id, teamId, url, options: {scrapeOptions}}] }` — lists all active crawls for the authenticated team.

---

## 6. Webhooks

### Webhook config object (embedded in `webhook` field of crawl / batch-scrape requests)

Ground-truth Zod schema (`apps/api/src/services/webhook/schema.ts`):

| Field | Type | Required | Default | Notes |
|---|---|---|---|---|
| `url` | string (must be valid URL) | **yes** | — | Can also pass a **bare string** instead of an object as shorthand — it is auto-wrapped as `{url: <string>}`. |
| `headers` | object (string→string) | no | `{}` | Custom headers sent with each webhook delivery. **[GROUND TRUTH]** The header name `x-firecrawl-signature` is blacklisted/rejected if you try to set it yourself — implying Firecrawl uses that header internally to sign outgoing webhook payloads. |
| `metadata` | object (string→string) | no | `{}` | Echoed back verbatim in every webhook payload's `metadata` field. **[GROUND TRUTH]** Values must be strings (`Record<string,string>`), not arbitrary JSON, despite the OpenAPI doc showing `additionalProperties: true` (looser than actual validation). |
| `events` | array of enum string | no | all four: `["completed","failed","page","started"]` | Valid values: `completed`, `page`, `failed`, `started` (bare, unprefixed). |

### Event delivery mechanics (from `webhooks/overview` — appears to apply to v1 and v2 alike; not v1-specific)

- Your endpoint must respond `2xx` within **10 seconds**.
- On failure (timeout/non-2xx/network error), retries at **+1 min, +5 min, +15 min** (3 total retries), then gives up.

### Emitted event `type` values relevant to crawl / batch scrape (prefixed form, from the v1 OpenAPI spec's own field descriptions plus the shared events reference page)

| Event type | Trigger |
|---|---|
| `crawl.started` | Crawl job begins processing. |
| `crawl.page` | A page is scraped during the crawl. |
| `crawl.completed` | Crawl finishes, all pages processed. |
| `crawl.failed` | **[confirmed only via the v1 OpenAPI field description text, not a fully worked example]** Crawl job fails. |
| `batch_scrape.started` | Batch scrape job begins. |
| `batch_scrape.page` | A URL is scraped during the batch. |
| `batch_scrape.completed` | All URLs in the batch processed. |
| `batch_scrape.failed` | **[same caveat as crawl.failed]** Batch scrape job fails. |

Note the asymmetry: the `events` **filter** field on the webhook config uses bare names (`started`/`page`/`completed`/`failed`), but the **emitted** `type` field on each delivered payload uses the prefixed form (`crawl.started`, `batch_scrape.page`, etc.) — these are two different vocabularies for the same four lifecycle stages, and both crawl and batch-scrape jobs share the identical four-stage lifecycle model.

### Webhook payload envelope (shared structure across event types)

```json
{
  "success": true,
  "type": "crawl.page",
  "id": "550e8400-e29b-41d4-a716-446655440000",
  "data": [ /* array (or sometimes object) of event-specific data */ ],
  "metadata": {}
}
```

| Field | Type | Notes |
|---|---|---|
| `success` | boolean | Whether the operation/stage succeeded. |
| `type` | string | e.g. `crawl.page`, `batch_scrape.completed`. |
| `id` | string (UUID) | The crawl/batch-scrape job ID. |
| `data` | array or object | Empty array `[]` for `started`/`completed` events; populated with page `Document`-shaped objects (see Section 1's `data` fields — `markdown`, `metadata.title/description/url/statusCode/contentType/scrapeId/sourceURL/proxyUsed/cacheState/cachedAt/creditsUsed`, etc.) for `page` events. |
| `metadata` | object | Echoes back the `metadata` you configured on the webhook. |
| `error` | string | Present when `success` is `false`. |

Example `crawl.page` payload (from official docs):
```json
{
  "success": true,
  "type": "crawl.page",
  "id": "550e8400-e29b-41d4-a716-446655440000",
  "data": [
    {
      "markdown": "# Page content...",
      "metadata": {
        "title": "Page Title",
        "description": "Page description",
        "url": "https://example.com/page",
        "statusCode": 200,
        "contentType": "text/html",
        "scrapeId": "550e8400-e29b-41d4-a716-446655440001",
        "sourceURL": "https://example.com/page",
        "proxyUsed": "basic",
        "cacheState": "hit",
        "cachedAt": "2025-09-03T21:11:25.636Z",
        "creditsUsed": 1
      }
    }
  ],
  "metadata": {}
}
```
`batch_scrape.page` is byte-for-byte the same shape (only the `type` value and example URLs differ).

**Not directly requested, but adjacent and possibly useful:** the same webhook payload envelope and 4-event lifecycle model (`started`/`page-or-action`/`completed`/`failed`[/`cancelled` for agents]) is reused for Extract, Agent, and Monitor jobs too, with their own `type` prefixes (`extract.*`, `agent.*`, `monitor.*`) and different `data` shapes — these are out of scope for v1 crawl/batch-scrape parity but documented on the same `webhooks/events` page if needed later.

---

## Things I could NOT fully confirm from primary sources

1. **`branding` in the base `/scrape` formats enum** — present in the GitHub `main`-branch `v1-openapi.json`/`openapi.json` files' formats enum, present in the ground-truth Zod `Format` type, but absent from the specific example enum shown on the live-rendered `/api-reference/v1-endpoint/scrape` docs page. I could not determine why the rendered docs page and the raw spec file it claims to source from disagree on this one enum value — possibly a docs-build caching/sync lag, or a manually-trimmed example in the MDX wrapper around the embedded spec. Treat "13 accepted format values including `branding`/`product`/`menu`/`summary`/`extract`" (ground truth) as authoritative for what the server will accept, but be aware the officially *published* enum for this specific endpoint is narrower.
2. **`ProductProfile` and `MenuProfile` field shapes** — confirmed to exist as response object types (`product`/`menu` formats produce `data.product`/`data.menu`), but I could not locate any documentation, example payload, or even the TypeScript type body for these two structures (only saw the import statements). Existence only, not internal shape.
3. **`BrandingProfile` sub-object internals** (`colors`, `typography`, `spacing`, `components`, `icons`, `images`, `animations`, `layout`, `tone`) — the OpenAPI schema marks all of these as open/`additionalProperties: true` with no further itemization anywhere I found. Treat as an unstructured bag.
4. **Whether `/v1/search` result items get the FULL scrape metadata field list (Section 1's table) or only the narrower subset explicitly named in the search OpenAPI schema** (`title`, `description`, `sourceURL`, `statusCode`, `numPages`, `totalPages`, `error`) when scraping is requested. I could not find a full example response with `scrapeOptions` populated to verify this either way.
5. **Exact semantics/purpose of `/v1/search`'s undocumented `filter`, `lang`, `integration`, and `__searchPreviewToken` fields** — confirmed they exist and are read server-side, but their functional effect on results is not documented anywhere public I could find.
6. **Whether `crawl.failed` / `batch_scrape.failed` webhook payloads carry any extra fields beyond the shared envelope** (e.g., an `error` string) — inferred from the general payload-structure docs ("`error` field present when `success` is false") and the OpenAPI's mention of the event names, but no worked JSON example of either failed-event payload was published anywhere I found.
7. **The exact set of ISO 3166-1 country codes and "special countries" accepted by `location.country`** — the server references a `countries` lookup table and a `SPECIAL_COUNTRIES` list but I did not fetch/enumerate their contents.
8. Some field-name casing is inconsistent between the older public OpenAPI text (`dctermsCreated`, `dctermsType`, …) and the newer ground-truth Zod-inferred type (`dcTermsCreated`, `dcTermsType`, …). I could not determine which casing is actually emitted on the wire today — flagged both variants in Section 1's metadata table rather than guessing.
