# Changelog

All notable changes to Draco are documented here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/); this project uses SemVer.

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
