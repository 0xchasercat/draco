# Draco

A fast, stealth, **native-Rust web scraper** — a lighter alternative to Firecrawl
/ Browserbase. Point it at a URL and get clean **Markdown + metadata** back, using
a browser-faithful TLS/JA4 fingerprint to reach pages that block ordinary clients.
No Node, no headless-Chrome fleet, no per-request browser boot.

```sh
draco extract https://example.com          # → clean Markdown on stdout
```

For a standard HTML page that's a single fingerprinted fetch + parse — typically
**~300 ms, no browser** — and the Markdown pipeline mirrors Firecrawl's
(deterministic main-content extraction + a Turndown/GFM-equivalent converter),
implemented natively in Rust.

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
draco extract https://example.com > page.md

# Full envelope (markdown + metadata + trace) as JSON
draco extract https://example.com --json --pretty

# Stealth + politeness
draco extract https://example.com --proxy socks5://127.0.0.1:9050 --delay 500
```

Exit codes: `0` success · `1` error · `2` unsupported · `3` needs_browser.

### Optional: JSON-API extraction (`--format json`)

Beyond Markdown, Draco can extract the **structured data an SPA loads from its own
API** — a power feature for data-driven sites. It escalates through the cheapest
tier that yields data:

1. **Static embedded state** — `__NEXT_DATA__`, JSON-LD, `window.__NUXT__`.
2. **Next.js build-id replay** — fetch `/_next/data/<buildId>/…​.json` directly.
3. **Runtime interception** — boot a jailed, jitless V8 isolate, let the page's JS
   hydrate, intercept the `fetch`/`XHR` it fires for its data, rank the intercepts,
   and replay the winner with the stealth client. The isolate is a *discovery
   oracle*, not a renderer.

```sh
draco extract https://app.example.com --format json --pretty       # data[]
draco extract https://app.example.com --format json --extract '$.props.pageProps'
draco extract https://app.example.com --format both                # markdown + data
```

Flags: `--format <markdown|json|both>` (default `markdown`), `--json`, `--extract
<JSONPATH>`, `--tier-max <0|1|2>`, `--proxy`, `--delay <ms>`, `--timeout <ms>`,
`--capture-window-ms <ms>`, `--ignore-robots`, `--no-jail`, `--strict-sandbox`,
`--allow-unsafe-replay`, `--pretty`.

> **Roadmap:** JS-rendered SPAs whose *content* (not just data) requires the DOM
> — Draco flags a thin shell today and will escalate to render-then-Markdown next,
> reusing the Tier 2 isolate.

## Workspace layout

| Crate | Role |
|-------|------|
| `draco-types` | Wire + result contract (no I/O) |
| `draco-net` | Stealth TLS/JA4 HTTP client (wreq/BoringSSL): cookie jar, proxy, robots, backoff |
| `draco-static` | **Markdown + metadata extraction** (Firecrawl-parity) · JSON embedded-state · build-id replay |
| `draco-jail` | Sandbox supervisor + jailed child: userns/netns air-gap, Landlock, seccomp, IPC codec |
| `draco-runtime` | Tier 2 V8 isolate (jitless), DOM + scheduler polyfill, `fetch`/`XHR` interception |
| `draco-core` | Escalation state machine, challenge short-circuit, ranking, replay |
| `draco-cli` | The `draco` CLI + output contract |

## Feature flags

- **default (`tier2`)** — everything, including the V8 isolate for `--format json`
  runtime interception.
- **`--no-default-features`** — a lean build with **no V8/jail linked**. Markdown
  scraping and static/build-id JSON extraction still work; runtime interception
  reports `unsupported`. Smaller binary, faster build.

```sh
cargo build -p draco-cli --no-default-features   # lean, V8-free
```

## Security model (only relevant to `--format json` Tier 2)

Markdown scraping executes no page JavaScript. The optional Tier 2 does, so
containment matters. Draco's **primary** containment is the isolate itself: the V8
context has **no host-capability bindings** — the only ops exposed to page JS
record the intercepted request, sleep, and resolve URLs. There is no
network/filesystem/process access, so page JS cannot perform I/O. **This is the
same class of isolation Puppeteer/Playwright/jsdom rely on**, works identically on
macOS and Linux, and needs zero configuration (Draco calls it **isolate mode**).

On **Linux**, Draco adds OS-level defense-in-depth automatically: a **seccomp-bpf
denylist** (kills breakout syscalls — `execve`, `socket`/`connect`, `ptrace`,
`mount`, `bpf`, executable `mprotect`; no per-host tuning), plus a
**network-namespace** air-gap and **Landlock** FS lockdown when the kernel
supports them. V8 runs `--jitless`. The achieved level shows in the `trace` as a
`runtime.sandbox` step (`hardened: …` / `isolate: …`). `--strict-sandbox` opts
into a maximal allowlist; `--no-jail` skips the OS layer.

Draco does **not** defeat JS challenge walls (Cloudflare/DataDome/…); a genuine
interstitial (blocking status + real challenge page) short-circuits to
`needs_browser`. A normal `200` behind a CDN is never treated as a challenge.

## Platforms

| Platform | Markdown scrape | JSON Tier 0/1 | Tier 2 isolate | Tier 2 OS hardening |
|----------|:---:|:---:|:---:|---|
| **Linux** `x86_64-gnu` | ✅ | ✅ | ✅ | ✅ auto (seccomp always; netns + Landlock when supported) |
| **macOS** `aarch64-darwin` | ✅ | ✅ | ✅ | isolate mode (Seatbelt hardening on the roadmap) |

Both are **first-class**. The optional Linux hardening can be verified on a real
kernel per **[docs/BARE_METAL_VALIDATION.md](docs/BARE_METAL_VALIDATION.md)** —
that confirms the extra hardening; it is not required to run Draco.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## Compliance & intended use

Draco is for public data, properties you operate, and APIs you're permitted to
use. Defaults are polite (robots.txt respected, per-host rate limiting, bounded
retries). JA4/TLS emulation is for compatibility, not to defeat authentication or
access controls. You are responsible for compliance with target sites' Terms of
Service and applicable law.

## License

MIT OR Apache-2.0.
