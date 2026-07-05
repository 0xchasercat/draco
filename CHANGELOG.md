# Changelog

All notable changes to Draco are documented here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/); this project uses SemVer.

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

[0.2.1]: https://github.com/0xchasercat/draco/releases/tag/v0.2.1
[0.2.0]: https://github.com/0xchasercat/draco/releases/tag/v0.2.0
[0.1.1]: https://github.com/0xchasercat/draco/releases/tag/v0.1.1
[0.1.0]: https://github.com/0xchasercat/draco/releases/tag/v0.1.0
