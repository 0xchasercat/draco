# Draco

A browserless, tiered data-extraction engine. Draco escalates through the
**cheapest successful extraction tier** and stops as soon as one yields data:

1. **Tier 0 — static embedded state.** Parse the raw HTML for `__NEXT_DATA__`,
   JSON-LD, and object-literal `window.__NUXT__`. No JS executed.
2. **Tier 1 — heuristic API replay.** Discover a Next.js `buildId` and fetch the
   `/_next/data/<buildId>/…​.json` endpoint directly. Still no JS.
3. **Tier 2 — runtime interception.** Boot a jailed, jitless V8 isolate, let the
   page's own SPA code hydrate, intercept the `fetch`/`XHR` it fires for its
   data, rank the intercepts, and **replay the winner** with the stealth HTTP
   client — returning the raw JSON the app itself consumed.

The browser engine is a *discovery oracle*, not a renderer: Draco never paints a
page, it just learns which API the page wanted and calls it directly.

> **Design:** the canonical architecture & execution spec remains the reference
> for the frozen `draco-types` wire contract, the IPC frame format, and the
> security model. This README reflects what shipped in **v0.1.0**.

## Install / build

```sh
git clone https://github.com/0xchasercat/draco && cd draco
cargo build --release
```

Build prerequisites (for wreq's BoringSSL + bindgen, and deno_core's V8):
`cmake`, a C/C++ compiler, `clang`/`libclang`, `perl`, `pkg-config`.
- Debian/Ubuntu: `apt install build-essential cmake clang libclang-dev perl pkg-config`
- Fedora: `dnf install gcc gcc-c++ cmake clang clang-devel llvm-devel perl pkgconf`
- macOS: Xcode Command Line Tools + `brew install cmake`

## Usage

```sh
# Full ladder (Tier 0 → 1 → 2):
draco extract "https://example.com/product/42" --pretty

# Cap the ladder (0 = static only, 1 = +build-id, 2 = +runtime):
draco extract "https://example.com" --tier-max 1

# Filter the output with JSONPath:
draco extract "https://example.com" --extract '$.props.pageProps'

# Politeness + stealth:
draco extract "https://example.com" --proxy socks5://127.0.0.1:9050 --delay 500
```

Output is always an `ExtractionResult` JSON on stdout: `status`, `source_tier`,
`data`, a `timing` breakdown, and a full escalation `trace`. Exit codes:
`0` success · `1` error · `2` unsupported · `3` needs_browser.

Selected flags: `--extract <JSONPATH>`, `--tier-max <0|1|2>`, `--proxy`,
`--delay <ms>`, `--timeout <ms>`, `--capture-window-ms <ms>`, `--ignore-robots`,
`--no-jail` (dev: run Tier 2 un-jailed), `--allow-unsafe-replay` (permit
replaying a state-changing request), `--pretty`.

## Workspace layout

| Crate | Role |
|-------|------|
| `draco-types` | Frozen wire + result contract (no I/O) |
| `draco-net` | Stealth TLS/JA4 HTTP client (wreq/BoringSSL): cookie jar, proxy, robots, backoff |
| `draco-static` | Tier 0 static extraction + Tier 1 build-id replay |
| `draco-jail` | Sandbox supervisor + jailed child: userns/netns air-gap, Landlock, two-phase seccomp, IPC codec |
| `draco-runtime` | Tier 2 V8 isolate (jitless), DOM + scheduler polyfill, `fetch`/`XHR` interception, capture window |
| `draco-core` | Escalation state machine, challenge short-circuit, ranking policy, replay |
| `draco-cli` | The `draco` CLI + output contract |

## Feature flags

- **default (`tier2`)** — the full engine, including the jailed V8 runtime.
- **`--no-default-features`** — a lean Tier 0/1 build with **no V8 and no jail
  linked** (smaller, faster to build); Tier 2 reports `unsupported`.

```sh
cargo build -p draco-cli --no-default-features   # lean, V8-free
```

## Security model (Tier 2)

Tier 2 evaluates a page's own untrusted JavaScript, so containment matters.
Draco's **primary** containment is the isolate itself: the V8 context is created
with **no host-capability bindings**. The only ops exposed to page JS are
`op_raze_fetch` (records the intercepted request and returns a stub), `op_sleep`,
and `op_resolve_url` — there is no `fetch`-to-network, no filesystem, and no
process API. Page JS therefore cannot perform I/O of any kind. **This is the same
class of isolation Puppeteer, Playwright, and jsdom rely on**; it works
identically on macOS and Linux and needs zero configuration. Draco calls this
**isolate mode**.

On **Linux**, Draco adds OS-level **defense-in-depth** automatically and
transparently — a hardening layer against a hypothetical V8-engine exploit:
- a **seccomp-bpf** filter that kills the dangerous breakout syscalls (`execve`,
  `socket`/`connect`, `ptrace`, `mount`, `bpf`, executable `mprotect`, …) — a
  robust *denylist* that needs **no per-host tuning**;
- a **network-namespace** air-gap and a **Landlock** filesystem lockdown when the
  kernel supports them (best-effort; silently skipped if not).

V8 runs `--jitless --single-threaded` (no executable memory). The achieved level
is reported in the result `trace` as a `runtime.sandbox` step — e.g.
`hardened: seccomp+netns+landlock` or `isolate: v8 no host bindings (macos)`.
There are no scary warnings and nothing to set up: **isolate mode is a fully
supported default everywhere; Linux simply gets more.** `--strict-sandbox` opts
into a maximal default-deny seccomp allowlist; `--no-jail` skips the OS layer.

Draco does **not** defeat JS challenge walls (Cloudflare/DataDome/…); a genuine
interstitial (a blocking status with a real challenge page) short-circuits to
`needs_browser`. A normal `200` behind a CDN is never treated as a challenge.

## Platforms

| Platform | Tier 0/1 | Tier 2 (isolate: V8, no host bindings) | Tier 2 OS hardening |
|----------|:--------:|:--------------------------------------:|---------------------|
| **Linux** `x86_64-gnu` | ✅ | ✅ | ✅ automatic — seccomp always; netns + Landlock when the kernel supports them |
| **macOS** `aarch64-darwin` | ✅ | ✅ | isolate mode (Seatbelt hardening is on the roadmap) |

Both are **first-class**. The optional Linux OS-hardening layer can be verified on
a real kernel per **[docs/BARE_METAL_VALIDATION.md](docs/BARE_METAL_VALIDATION.md)**
— that's for confirming the extra hardening, not a requirement to run Draco.

## Development

```sh
cargo test --workspace                                  # full suite
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
