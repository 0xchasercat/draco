# Vendored happy-dom DOM engine (Tier 2)

The Tier 2 isolate runs a real DOM — **happy-dom** — layered on a base of
ecosystem web-primitive polyfills, bundled with **Rolldown** (Oxc) into two
classic IIFEs that are baked into a V8 startup snapshot at build time
(`../../build.rs`). No `deno_runtime`, no hand-rolled DOM.

## Bundles (committed, consumed by build.rs → snapshot)
- `../../js/base.iife.js` — Node/web global base (whatwg-url, text-encoding,
  structured-clone) + a Node-compat shim (`set-env-first.js`, incl. op_sleep-backed
  timers) + dead-path Node builtin stubs.
- `../../js/happydom.iife.js` — happy-dom, with Node builtins aliased to the stubs.

## Regenerate (only when bumping happy-dom)
```
cd crates/draco-runtime/vendor/happy-dom
npm install
node build.mjs                 # writes ../../js/{base,happydom}.iife.js
```
`build.mjs` output paths point at `../../js/`. Requires network (npm) — run
manually, never in `cargo build`.
