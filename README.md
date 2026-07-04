# Draco

A browserless, tiered data-extraction engine. Draco escalates through the
cheapest successful extraction tier — static embedded state, then Next.js
build-id API replay, then a sandboxed V8 runtime that intercepts an SPA's own
data requests and replays them with a stealth HTTP client.

> **Authoritative design:** the canonical architecture & execution spec is the
> single source of truth for every crate, the frozen `draco-types` wire
> contract, the IPC frame format, the security model, and the implementation
> plan. Implement against it.

## Workspace layout

| Crate | Role |
|-------|------|
| `draco-types` | Frozen wire + result contract (no I/O). **Implemented.** |
| `draco-net` | Stealth TLS/JA4 HTTP client (wreq). *Stub → WS-A.* |
| `draco-static` | Tier 0 static + Tier 1 build-id extraction. *Stub → WS-B.* |
| `draco-jail` | Sandbox supervisor + child (netns/Landlock/seccomp). *Stub → Slice 2.* |
| `draco-runtime` | Tier 2 V8 isolate + interceptor. *Stub → Slice 3.* |
| `draco-core` | Escalation state machine + ranking + replay. *Stub → WS-C.* |
| `draco-cli` | `draco` CLI + output contract. *Stub → WS-D.* |

## Status: Slice 0

The workspace compiles green; `draco-types` is fully implemented with
round-trip tests. All other crates are compiling skeletons with frozen public
signatures and `todo!()` bodies, ready for parallel implementation.

```sh
cargo build --workspace
cargo test  -p draco-types
```

## Platforms (v0.1)

`x86_64-unknown-linux-gnu` (primary, full jail) and `aarch64-apple-darwin`
(dev; Tier 2 runs un-jailed with a warning). The jail's seccomp/Landlock/netns
layers are Linux-only and need kernel ≥ 5.13 + unprivileged user namespaces.

## Planned CI gates

`cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, and `cargo-deny`
(licenses + advisories — to be wired once the dependency set stabilizes).
