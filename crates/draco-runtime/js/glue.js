// draco-runtime — per-isolate runtime glue (runs after the snapshot is restored).
//
// The snapshot already contains the base web-primitive environment and the
// happy-dom classes (globalThis.HappyDOMBundle.Window). For each extraction we:
//   1. construct a fresh happy-dom Window for the target URL,
//   2. mirror its DOM globals onto globalThis so the page's inline scripts (which
//      reference bare `document`/`window`/`Element`/…) resolve against happy-dom,
//   3. install our fetch/XHR interceptor (records via op_raze_fetch, answers with
//      the configured stub) so the SPA hydrates and leaks its endpoints without
//      ever touching the network,
//   4. load the fetched HTML into the document so mount containers exist.
// The Rust side then evaluates the page's inline scripts, drives the capture
// window, and serializes document.documentElement.outerHTML via op_raze_dom.
//
// Inputs (set on globalThis by Rust before this runs):
//   __DRACO_URL__   : string  — the page URL
//   __DRACO_HTML__  : string  — the fetched HTML
//   __DRACO_STUB__  : string  — stub response body (JSON text) for intercepts
"use strict";
(function installGlue() {
  const g = globalThis;
  const url = g.__DRACO_URL__ || "https://localhost/";
  const html = typeof g.__DRACO_HTML__ === "string" ? g.__DRACO_HTML__ : "";
  const stubBody = typeof g.__DRACO_STUB__ === "string" && g.__DRACO_STUB__ ? g.__DRACO_STUB__ : "{}";
  const ops = Deno.core.ops;

  // 1. Fresh Window. Disable happy-dom's own script/CSS machinery: WE evaluate
  //    page scripts (its VM path is stubbed), and layout/CSS is irrelevant to
  //    content extraction.
  const W = g.HappyDOMBundle.Window;
  const w = new W({
    url,
    settings: {
      disableJavaScriptEvaluation: true,
      disableJavaScriptFileLoading: true,
      disableCSSFileLoading: true,
      disableComputedStyleRendering: true,
      errorCapture: "disabled",
    },
  });

  // 2. Mirror the Window's own + prototype globals onto globalThis. Page scripts
  //    reference bare identifiers; those resolve to globalThis, so happy-dom's
  //    document/Element/Event/etc. must live there. Never clobber the JS engine
  //    intrinsics or the deno_core core.
  const SKIP = new Set([
    "globalThis", "global", "self", "eval", "Function", "Deno", "window",
    "top", "parent", "frames", "constructor",
  ]);
  const mirror = (obj) => {
    if (!obj) return;
    for (const k of Object.getOwnPropertyNames(obj)) {
      if (SKIP.has(k)) continue;
      if (Object.prototype.hasOwnProperty.call(g, k) && (k === "location" || k === "document")) {
        // fall through — we want these overwritten below explicitly
      }
      try {
        const v = w[k];
        if (v !== undefined) g[k] = v;
      } catch (_) {
        /* accessor threw — skip */
      }
    }
  };
  mirror(Object.getPrototypeOf(w)); // Window.prototype: DOM class getters
  mirror(w); // own props: document, location, navigator, history, …

  // Unify the global aliases. In a real top-level browsing context
  // `window === self === globalThis === top === parent === frames` — all ONE
  // object. happy-dom instantiates its Window as a *separate* object `w`, so if
  // we point the page-facing `window` at `w` (while `self`/`globalThis` remain
  // the V8 global `g`), the aliases diverge: a library that writes one and reads
  // another sees `undefined`. Next.js is the canonical victim — it writes
  // `window.__NEXT_DATA__ = data` (client/index.js) but the Router reads
  // `self.__NEXT_DATA__.gssp` (router.js), so hydration throws
  // "Cannot read properties of undefined (reading 'gssp')" and aborts before any
  // data fetch. Pointing `window` (and top/parent/frames) at `g` makes every
  // alias the same object, matching the browser. The DOM stays reachable because
  // the DOM globals were just mirrored onto `g` (and `g.document === w.document`
  // below); happy-dom's own internals reference their window through a private
  // Symbol, not the global identifier, so they are unaffected.
  g.window = g;
  g.self = g;
  g.top = g;
  g.parent = g;
  g.frames = g;
  g.document = w.document;
  if (w.navigator) g.navigator = w.navigator;
  if (w.location) g.location = w.location;
  if (w.history) g.history = w.history;

  // Backfill DOM element constructors that happy-dom does not implement but that
  // frameworks reference as BARE globals in `instanceof` / `typeof` guards. A
  // bare reference to an undefined identifier is a ReferenceError (not
  // `undefined`), so a single missing constructor in a hot guard aborts the
  // surrounding code. happy-dom ships 69 `SVG*Element` classes but not
  // `SVGAElement` (SVG `<a>`); SvelteKit's link router runs
  // `el instanceof SVGAElement ? el.href.baseVal : el.href` on hydration and
  // throws "SVGAElement is not defined", killing navigation wiring (seen on
  // chaser.sh). Define each missing name as a subclass of the nearest base
  // happy-dom DOES provide, so `instanceof` is well-typed (false for the common
  // HTML element) instead of throwing. Never clobber a constructor happy-dom
  // already exposes.
  (function backfillMissingElementCtors() {
    const base = (...names) => {
      for (const n of names) if (typeof g[n] === "function") return g[n];
      return g.Element || function () {};
    };
    // name -> preferred base chain (nearest first).
    const missing = {
      SVGAElement: ["SVGGraphicsElement", "SVGGeometryElement", "SVGElement"],
    };
    for (const name of Object.keys(missing)) {
      if (typeof g[name] === "function") continue; // happy-dom has it — leave it.
      const Base = base.apply(null, missing[name]);
      try {
        const Ctor = function () {
          throw new TypeError("Illegal constructor");
        };
        Ctor.prototype = Object.create(Base.prototype, {
          constructor: { value: Ctor, writable: true, configurable: true },
        });
        Object.defineProperty(Ctor, "name", { value: name, configurable: true });
        g[name] = Ctor;
      } catch (_) {
        /* defining failed — leave undefined; guard will still throw, but we did
           our best without masking a real happy-dom regression */
      }
    }
  })();

  // Browser-ish Performance API shim. Some framework/telemetry chunks (Sentry,
  // web-vitals) assume these exist and abort setup if they don't. They are
  // observational APIs; returning empty entries is safer than letting analytics
  // code stop the app before data-fetching code runs.
  function installPerformanceShim(target) {
    try {
      const p = target.performance || (target.performance = {});
      if (typeof p.now !== "function") p.now = () => Date.now();
      if (typeof p.timeOrigin !== "number") p.timeOrigin = Date.now();
      if (typeof p.getEntriesByType !== "function") p.getEntriesByType = () => [];
      if (typeof p.getEntriesByName !== "function") p.getEntriesByName = () => [];
      if (typeof p.getEntries !== "function") p.getEntries = () => [];
      if (typeof p.mark !== "function") p.mark = () => undefined;
      if (typeof p.measure !== "function") p.measure = () => undefined;
      if (typeof p.clearMarks !== "function") p.clearMarks = () => undefined;
      if (typeof p.clearMeasures !== "function") p.clearMeasures = () => undefined;
      if (typeof p.clearResourceTimings !== "function") p.clearResourceTimings = () => undefined;
      if (typeof p.setResourceTimingBufferSize !== "function") p.setResourceTimingBufferSize = () => undefined;
    } catch (_) {}
  }
  installPerformanceShim(g);
  installPerformanceShim(w);
  if (typeof g.PerformanceObserver !== "function") {
    g.PerformanceObserver = class {
      static supportedEntryTypes = [];
      constructor() {}
      observe() {}
      disconnect() {}
      takeRecords() { return []; }
    };
    try { w.PerformanceObserver = g.PerformanceObserver; } catch (_) {}
  }

  // Web Animations API shim (Element.animate / getAnimations). happy-dom ships
  // neither, and Svelte 5's transition runtime calls `element.animate(...)`
  // inside its effect flush — the TypeError aborts the component tree MID-MOUNT,
  // so everything transition-wrapped (typically the page's main content) never
  // renders while transition-free regions (footers) do. The shim is inert and
  // COMPLETION-BIASED: we don't render pixels, so every animation reports
  // "finished" almost immediately and hydration proceeds at full speed.
  //
  // Timing contract: finish is scheduled ~20ms out (one rAF tick — our rAF is
  // setTimeout(16) — plus slack), so handlers attached right after `animate()`
  // returns AND handlers attached inside a first rAF both land before finish.
  // `onfinish` is an accessor: assigned AFTER finish already fired → invoked
  // async immediately, so late subscribers cannot hang a transition. `finished`
  // is a promise resolving with the animation. cancel()/finish() settle early;
  // cancel resolves `finished` too (spec rejects, but an inert shim must never
  // hang a transition chain or spray unhandled rejections into the logs).
  (function installWebAnimations() {
    const ElementCtor = g.Element || (w && w.Element);
    if (!ElementCtor || !ElementCtor.prototype) return;
    if (typeof ElementCtor.prototype.animate === "function") return; // real impl wins
    function makeAnimation(effectTarget) {
      const a = {
        effect: { target: effectTarget },
        playState: "running",
        currentTime: 0,
        startTime: 0,
        playbackRate: 1,
        pending: false,
        _onfinish: null,
        _oncancel: null,
        _listeners: Object.create(null),
        _finished: false,
        _resolveFinished: null,
      };
      a.finished = new Promise((resolve) => { a._resolveFinished = resolve; });
      const fire = (type) => {
        const ev = { type, target: a, currentTarget: a, timelineTime: 0 };
        const h = type === "finish" ? a._onfinish : type === "cancel" ? a._oncancel : null;
        if (typeof h === "function") { try { h.call(a, ev); } catch (_) {} }
        const ls = a._listeners[type];
        if (ls) for (const fn of ls.slice()) { try { fn.call(a, ev); } catch (_) {} }
      };
      const settle = (state) => {
        if (a._finished) return;
        a._finished = true;
        a.playState = state;
        try { a._resolveFinished(a); } catch (_) {}
        fire(state === "finished" ? "finish" : "cancel");
        // A cancel still settles `finished` so awaiting code never hangs.
        if (state !== "finished") { /* resolved above */ }
      };
      Object.defineProperty(a, "onfinish", {
        configurable: true,
        get() { return a._onfinish; },
        set(fn) {
          a._onfinish = fn;
          // Late subscription after finish: invoke async so the caller's
          // transition chain still completes.
          if (a._finished && a.playState === "finished" && typeof fn === "function") {
            setTimeout(() => { try { fn.call(a, { type: "finish", target: a }); } catch (_) {} }, 0);
          }
        },
      });
      Object.defineProperty(a, "oncancel", {
        configurable: true,
        get() { return a._oncancel; },
        set(fn) { a._oncancel = fn; },
      });
      a.addEventListener = function (t, fn) { if (typeof fn === "function") (a._listeners[t] || (a._listeners[t] = [])).push(fn); };
      a.removeEventListener = function (t, fn) { const ls = a._listeners[t]; if (!ls) return; const i = ls.indexOf(fn); if (i >= 0) ls.splice(i, 1); };
      a.play = function () { if (!a._finished) a.playState = "running"; };
      a.pause = function () { if (!a._finished) a.playState = "paused"; };
      a.reverse = function () {};
      a.updatePlaybackRate = function () {};
      a.commitStyles = function () {};
      a.persist = function () {};
      a.finish = function () { settle("finished"); };
      a.cancel = function () { settle("idle"); };
      // Auto-finish shortly after creation (see timing contract above).
      setTimeout(() => settle("finished"), 20);
      return a;
    }
    ElementCtor.prototype.animate = function () { return makeAnimation(this); };
    if (typeof ElementCtor.prototype.getAnimations !== "function") {
      ElementCtor.prototype.getAnimations = function () { return []; };
    }
    const DocCtor = g.Document || (w && w.Document);
    if (DocCtor && DocCtor.prototype && typeof DocCtor.prototype.getAnimations !== "function") {
      try { DocCtor.prototype.getAnimations = function () { return []; }; } catch (_) {}
    }
    if (typeof g.Animation === "undefined") {
      // Bare-constructor form (`new Animation(effect)`) some motion libs probe.
      g.Animation = function Animation() { return makeAnimation(null); };
      try { w.Animation = g.Animation; } catch (_) {}
    }
  })();

  // Completion-biased IntersectionObserver + ResizeObserver. happy-dom and our
  // snapshot polyfills stub these as INERT no-ops that never fire their callback —
  // which silently breaks the most common lazy-content pattern on modern SPAs: a
  // section observes itself and fetches/renders its data only once it scrolls into
  // view (IntersectionObserver) or is measured (ResizeObserver). With a no-op
  // observer the callback never fires, so those sections stay in their initial
  // (skeleton) state forever — the shell hydrates but the data-driven sections
  // never load (exactly thrill.com's game rows). We don't render pixels, so the
  // right bias for a content extractor is "everything is visible/measured, once":
  // report each observed element as fully intersecting on the next tick, which
  // triggers the lazy load. Fire ONCE per observe() — no repeat loop — and the
  // capture window + max_intercepts still bound any infinite-scroll fan-out.
  (function installObservers() {
    function rectOf(el) {
      try {
        const r = el && el.getBoundingClientRect && el.getBoundingClientRect();
        if (r) return r;
      } catch (_) {}
      return { x: 0, y: 0, top: 0, left: 0, bottom: 0, right: 0, width: 0, height: 0 };
    }
    const now = () => { try { return g.performance && g.performance.now ? g.performance.now() : 0; } catch (_) { return 0; } };
    class DracoIntersectionObserver {
      constructor(cb) { this._cb = typeof cb === "function" ? cb : function () {}; this._els = new Set(); }
      observe(el) {
        if (!el || this._els.has(el)) return;
        this._els.add(el);
        setTimeout(() => {
          if (!this._els.has(el)) return; // unobserved/disconnected before firing
          const rect = rectOf(el);
          try {
            this._cb([{
              target: el, isIntersecting: true, intersectionRatio: 1,
              boundingClientRect: rect, intersectionRect: rect, rootBounds: rect, time: now(),
            }], this);
          } catch (_) {}
        }, 0);
      }
      unobserve(el) { this._els.delete(el); }
      disconnect() { this._els.clear(); }
      takeRecords() { return []; }
    }
    class DracoResizeObserver {
      constructor(cb) { this._cb = typeof cb === "function" ? cb : function () {}; this._els = new Set(); }
      observe(el) {
        if (!el || this._els.has(el)) return;
        this._els.add(el);
        setTimeout(() => {
          if (!this._els.has(el)) return;
          const rect = rectOf(el);
          const box = [{ inlineSize: rect.width || 0, blockSize: rect.height || 0 }];
          try {
            this._cb([{
              target: el, contentRect: rect,
              borderBoxSize: box, contentBoxSize: box, devicePixelContentBoxSize: box,
            }], this);
          } catch (_) {}
        }, 0);
      }
      unobserve(el) { this._els.delete(el); }
      disconnect() { this._els.clear(); }
      takeRecords() { return []; }
    }
    try { g.IntersectionObserver = DracoIntersectionObserver; } catch (_) {}
    try { w.IntersectionObserver = DracoIntersectionObserver; } catch (_) {}
    try { g.ResizeObserver = DracoResizeObserver; } catch (_) {}
    try { w.ResizeObserver = DracoResizeObserver; } catch (_) {}
  })();

  // 3. fetch / XHR interceptor → op_raze_fetch. Reuse happy-dom's Headers when
  //    present; otherwise a tiny shim below.
  function headersToPairs(h) {
    const out = [];
    if (!h) return out;
    if (typeof h.forEach === "function" && !Array.isArray(h)) { h.forEach((v, k) => out.push([String(k), String(v)])); return out; }
    if (Array.isArray(h)) { for (const p of h) if (p && p.length >= 2) out.push([String(p[0]), String(p[1])]); return out; }
    if (typeof h === "object") for (const k of Object.keys(h)) out.push([k, String(h[k])]);
    return out;
  }
  function bodyToString(body) {
    if (body == null) return null;
    if (typeof body === "string") return body;
    try { if (body instanceof g.URLSearchParams) return body.toString(); } catch (_) {}
    if (typeof body === "object") {
      if (typeof body.toString === "function") { const s = body.toString(); if (s !== "[object Object]") return s; }
      try { return JSON.stringify(body); } catch (_) { return null; }
    }
    return String(body);
  }
  function absolutize(u) {
    const base = (w.location && w.location.href) || url;
    try { return ops.op_resolve_url(base, String(u)); } catch (_) { return String(u); }
  }
  // Async: op_raze_fetch records the request (always) and, in Render mode, may
  // fetch it live via draco-net and return the REAL {status,headers,body}. Awaits
  // that op and parses its JSON; on any failure falls back to the synthetic stub.
  async function record(via, method, u, headers, bodyStr) {
    const req = { via, method: (method || "GET").toUpperCase(), url: absolutize(u), headers: headers || [], body: bodyStr == null ? null : String(bodyStr) };
    let respJson;
    try { respJson = await ops.op_raze_fetch(JSON.stringify(req)); } catch (_) { respJson = null; }
    if (!respJson) return { status: 200, headers: [["content-type", "application/json"]], body: stubBody };
    try { return JSON.parse(respJson); } catch (_) { return { status: 200, headers: [["content-type", "application/json"]], body: stubBody }; }
  }
  const HeadersCtor = g.Headers || class { constructor(i){ this._m = new Map(); for (const [k,v] of headersToPairs(i)) this._m.set(String(k).toLowerCase(), String(v)); } get(k){ const v=this._m.get(String(k).toLowerCase()); return v==null?null:v; } has(k){ return this._m.has(String(k).toLowerCase()); } forEach(cb){ this._m.forEach((v,k)=>cb(v,k,this)); } };
  function makeResponse(stub, finalUrl) {
    const status = (stub && stub.status) || 200;
    const bodyText = stub && typeof stub.body === "string" ? stub.body : "{}";
    const headers = new HeadersCtor((stub && stub.headers) || []);
    let used = false;
    const guard = () => { if (used) throw new TypeError("body already consumed"); used = true; };
    // Minimal ReadableStream stand-in for `response.body`. Streaming-fetch code
    // (`res.body.getReader().read()`, common in SvelteKit data loaders and
    // fetch-based SSE) throws `Cannot read properties of undefined (reading
    // 'getReader')` when `body` is absent, aborting hydration. We expose an
    // already-closed stream: `getReader().read()` resolves `{done:true}` at once,
    // so the reader loop terminates cleanly with no data (the isolate is
    // air-gapped — there is no real stream), never throwing. One-shot text/json
    // bodies still come through `text()`/`json()` above.
    const emptyStreamBody = () => ({
      locked: false,
      getReader() {
        return {
          read() { return Promise.resolve({ done: true, value: undefined }); },
          releaseLock() {},
          cancel() { return Promise.resolve(); },
        };
      },
      cancel() { return Promise.resolve(); },
    });
    return {
      ok: status >= 200 && status < 300, status, statusText: status === 200 ? "OK" : "",
      url: finalUrl, redirected: false, type: "basic", headers, bodyUsed: false,
      body: emptyStreamBody(),
      async text() { guard(); return bodyText; },
      async json() { guard(); return JSON.parse(bodyText); },
      async arrayBuffer() { guard(); return new TextEncoder().encode(bodyText).buffer; },
      async blob() { guard(); return { size: bodyText.length, type: "", text: async () => bodyText }; },
      clone() { return makeResponse(stub, finalUrl); },
    };
  }
  const doFetch = async function fetch(input, init) {
    init = init || {};
    let u, method = init.method || "GET", headers = headersToPairs(init.headers), body = init.body;
    if (input && typeof input === "object" && "url" in input) {
      u = input.url;
      if (!init.method && input.method) method = input.method;
      if ((!init.headers || headers.length === 0) && input.headers) headers = headersToPairs(input.headers);
      if (init.body == null && input.body != null) body = input.body;
    } else { u = String(input); }
    const stub = await record("fetch", method, u, headers, bodyToString(body));
    return makeResponse(stub, absolutize(u));
  };
  g.fetch = doFetch;
  try { w.fetch = doFetch; } catch (_) {}

  const UNSENT = 0, OPENED = 1, HEADERS_RECEIVED = 2, LOADING = 3, DONE = 4;
  class XHR {
    constructor() { this.readyState = UNSENT; this.status = 0; this.statusText = ""; this.responseText = ""; this.response = ""; this.responseType = ""; this.responseURL = ""; this.onreadystatechange = null; this.onload = null; this.onloadend = null; this.onerror = null; this._m = "GET"; this._u = ""; this._h = []; this._rh = []; this._l = Object.create(null); this._ab = false; }
    addEventListener(t, fn) { if (typeof fn === "function") (this._l[t] || (this._l[t] = [])).push(fn); }
    removeEventListener(t, fn) { const a = this._l[t]; if (!a) return; const i = a.indexOf(fn); if (i >= 0) a.splice(i, 1); }
    _emit(t) { const ev = { type: t, target: this, currentTarget: this }; const d = this["on" + t]; if (typeof d === "function") { try { d.call(this, ev); } catch (_) {} } const a = this._l[t]; if (a) for (const fn of a.slice()) { try { fn.call(this, ev); } catch (_) {} } }
    open(m, u) { this._m = m || "GET"; this._u = u || ""; this.readyState = OPENED; this._emit("readystatechange"); }
    setRequestHeader(k, v) { this._h.push([String(k), String(v)]); }
    getResponseHeader(k) { const kk = String(k).toLowerCase(); const hit = this._rh.find((p) => p[0].toLowerCase() === kk); return hit ? hit[1] : null; }
    getAllResponseHeaders() { return this._rh.map(([k, v]) => k + ": " + v).join("\r\n") + "\r\n"; }
    overrideMimeType() {}
    abort() { this._ab = true; this._emit("abort"); }
    send(body) {
      if (this._ab) return;
      // record() is async (it may fetch live in Render mode); deliver on resolve.
      // The arrow callback preserves `this`; delivery is a microtask/turn later,
      // same observable ordering as the old queueMicrotask path.
      record("xhr", this._m, this._u, this._h, body == null ? null : bodyToString(body)).then((stub) => {
        if (this._ab) return;
        this.status = (stub && stub.status) || 200; this.statusText = this.status === 200 ? "OK" : "";
        this._rh = (stub && stub.headers) || [];
        const bt = stub && typeof stub.body === "string" ? stub.body : "{}";
        this.responseText = bt; this.responseURL = absolutize(this._u);
        this.response = this.responseType === "json" ? (() => { try { return JSON.parse(bt); } catch (_) { return null; } })() : bt;
        this.readyState = DONE; this._emit("readystatechange"); this._emit("load"); this._emit("loadend");
      });
    }
  }
  g.XMLHttpRequest = XHR;

  // Streaming-connection APIs (EventSource / WebSocket). SPAs commonly open one
  // during init — stake.com's app bootstrap does `new EventSource(...)` — and a
  // bare reference to a missing constructor is a ReferenceError that aborts
  // hydration before any data fetch runs (same failure class as the missing DOM
  // constructors backfilled above). happy-dom ships neither. We install no-op
  // stubs that (a) never throw, and (b) RECORD the connection's URL as an
  // intercepted request, because an SSE/WebSocket endpoint is exactly the kind of
  // API surface `discover` exists to find. They never emit events (the isolate is
  // air-gapped), so nothing downstream blocks waiting on a live stream.
  function installStreamingCtor(name, defaultMethod) {
    // Always install (override), unlike the DOM-constructor backfill above:
    // happy-dom's own EventSource is absent and its WebSocket is a non-functional
    // stub that throws "ws does not work in the browser" — both useless in the
    // air-gapped isolate. Same posture as the unconditional `fetch`/XHR override.
    const Ctor = function (u) {
      this.url = absolutize(u);
      this.readyState = 0;
      this.onopen = null;
      this.onmessage = null;
      this.onerror = null;
      this.onclose = null;
      this._l = Object.create(null);
      try {
        // Fire-and-forget: record the streaming endpoint (discover cares about it).
        // record() is async now; swallow any rejection since the stream is inert.
        const p = record("fetch", defaultMethod, u, [["accept", "text/event-stream"]], null);
        if (p && typeof p.then === "function") p.catch(function () {});
      } catch (_) {}
    };
    Ctor.prototype.addEventListener = function (t, fn) {
      if (typeof fn === "function") (this._l[t] || (this._l[t] = [])).push(fn);
    };
    Ctor.prototype.removeEventListener = function (t, fn) {
      const a = this._l[t];
      if (!a) return;
      const i = a.indexOf(fn);
      if (i >= 0) a.splice(i, 1);
    };
    Ctor.prototype.close = function () {
      this.readyState = 2;
    };
    Ctor.prototype.send = function () {}; // WebSocket.send: no-op (air-gapped).
    // Standard readyState constants.
    Ctor.CONNECTING = 0;
    Ctor.OPEN = 1;
    Ctor.CLOSING = 2;
    Ctor.CLOSED = 3;
    try {
      Object.defineProperty(Ctor, "name", { value: name, configurable: true });
    } catch (_) {}
    g[name] = Ctor;
    try {
      w[name] = Ctor;
    } catch (_) {}
  }
  installStreamingCtor("EventSource", "GET");
  installStreamingCtor("WebSocket", "GET");

  // Dynamic script chunk loader hook. Frameworks such as Next/Webpack load lazy
  // chunks by creating <script src="/_next/static/chunks/..."></script> and
  // resolving a promise from onload/onerror. happy-dom's own script loading is
  // disabled, so we take over: when such a node is appended/inserted we kick off an
  // ASYNC in-process fetch (op_raze_load_script → draco-net + chunk cache) and eval
  // the source when it resolves, firing load/error. The fetch does NOT block the
  // insertion, so a burst of chunk appends fans out CONCURRENTLY on the event loop
  // (the whole point of the in-process async engine) instead of one blocking round
  // trip at a time. A miss fires onerror so page code sees a network-miss shape.
  function scriptSrc(node) {
    try {
      if (!node || String(node.tagName || "").toLowerCase() !== "script") return null;
      const raw = node.src || (node.getAttribute && node.getAttribute("src"));
      return raw ? absolutize(raw) : null;
    } catch (_) { return null; }
  }
  function fireScriptEvent(node, type, url) {
    try {
      const ev = { type, target: node, currentTarget: node, srcElement: node, url };
      const h = node && node["on" + type];
      if (typeof h === "function") h.call(node, ev);
      if (node && typeof node.dispatchEvent === "function" && typeof Event === "function") {
        node.dispatchEvent(new Event(type));
      }
    } catch (_) {}
  }
  function maybeRunScriptNode(node) {
    const u = scriptSrc(node);
    if (!u || node.__dracoLoaded) return false;
    node.__dracoLoaded = true;
    let type = "";
    try { type = String((node.type || (node.getAttribute && node.getAttribute("type")) || "")).toLowerCase(); } catch (_) {}
    if (type === "module") {
      // A dynamically inserted <script type="module" src=…> must be loaded AND
      // evaluated as an ES module — indirect eval() can't run import/export
      // syntax. Route it through native dynamic import(), which resolves via the
      // same MapModuleLoader (→ ScriptFetcher) the page's own import()s use.
      // Returning true skips happy-dom's own (disabled) module loader, which only
      // logs a NotSupportedError. The src is absolute, so resolution is
      // base-independent; failure fires `error` (non-fatal), never aborts.
      let ip = null;
      try { ip = import(u); } catch (_) { ip = null; }
      if (!ip || typeof ip.then !== "function") { fireScriptEvent(node, "error", u); return true; }
      ip.then(
        function () { fireScriptEvent(node, "load", u); },
        function (e) { logSwallowed("module-script", e); fireScriptEvent(node, "error", u); },
      );
      return true;
    }
    // Classic <script src>: kick off the chunk fetch ASYNCHRONOUSLY and return true
    // (handled) at once — a dynamically inserted <script src> loads off-thread in a
    // browser, so we must not block insertion. op_raze_load_script is an async op
    // (returns a Promise): eval + fire `load` when it resolves, fire `error` on a
    // miss/throw. Because we return immediately, a burst of appended chunks fetches
    // CONCURRENTLY on the event loop instead of serializing one round-trip at a time.
    let p = null;
    try { p = ops.op_raze_load_script(u); } catch (_) { p = null; }
    if (!p || typeof p.then !== "function") { fireScriptEvent(node, "error", u); return true; }
    p.then(
      (src) => {
        if (typeof src !== "string") { fireScriptEvent(node, "error", u); return; }
        try {
          // Indirect eval runs in global scope. //# sourceURL gives stack traces an
          // absolute URL and lets relative dynamic imports inside chunks resolve.
          (0, eval)(src + "\n//# sourceURL=" + u);
          fireScriptEvent(node, "load", u);
        } catch (e) {
          logSwallowed("script", e);
          fireScriptEvent(node, "error", u);
        }
      },
      (e) => { logSwallowed("script", e); fireScriptEvent(node, "error", u); },
    );
    return true;
  }
  function hookInsertion(proto, name) {
    if (!proto || typeof proto[name] !== "function") return;
    const orig = proto[name];
    proto[name] = function (...args) {
      // Take over <script src> loading BEFORE real insertion (maybeRunScriptNode
      // kicks off the async fetch and returns true immediately) so happy-dom's
      // disabled file loader never tries — and fails — to fetch it itself.
      const handled = (() => { try { return maybeRunScriptNode(args[0]); } catch (_) { return false; } })();
      if (handled && name !== "append") return args[0];
      if (handled && name === "append") return undefined;
      return orig.apply(this, args);
    };
  }
  try {
    hookInsertion(g.Node && g.Node.prototype, "appendChild");
    hookInsertion(g.Node && g.Node.prototype, "insertBefore");
    hookInsertion(g.Element && g.Element.prototype, "append");
    hookInsertion(w.Node && w.Node.prototype, "appendChild");
    hookInsertion(w.Node && w.Node.prototype, "insertBefore");
    hookInsertion(w.Element && w.Element.prototype, "append");
  } catch (_) {}

  // 4. Load the fetched HTML so the framework's mount container exists. Prefer
  //    write() (full-document parse); fall back to setting body/head innerHTML.
  if (html) {
    try {
      w.document.write(html);
    } catch (_) {
      try {
        const m = html.match(/<body[^>]*>([\s\S]*?)<\/body>/i);
        w.document.body.innerHTML = m ? m[1] : html;
      } catch (_2) {}
    }
  }

  // 5. Async-error containment. A throwing/rejecting third-party script must not
  //    abort the capture loop (browsers keep running; a later script's data fetch
  //    should still surface). Swallow unhandled rejections and reported
  //    exceptions instead of letting deno_core dispatch them fatally.
  let swallowed = 0;
  const logSwallowed = (kind, e) => {
    // Always record into the capture report (op_raze_log enforces count/length
    // bounds) — this is how hydration failures become visible to the supervisor
    // (`runtime.log` trace steps) without a browser devtools.
    try { ops.op_raze_log("[" + kind + "] " + (e && (e.stack || e.message) || e)); } catch (_) {}
    if (swallowed++ >= 5) return;
    try { Deno.core.print("[glue] swallowed " + kind + ": " + (e && e.stack || e) + "\n", true); } catch (_) {}
  };
  try {
    if (Deno.core.setUnhandledPromiseRejectionHandler) {
      Deno.core.setUnhandledPromiseRejectionHandler((_promise, reason) => { logSwallowed("rejection", reason); return true; });
    }
    if (Deno.core.setReportExceptionCallback) {
      Deno.core.setReportExceptionCallback((err) => { logSwallowed("exception", err); });
    }
  } catch (_) { /* hooks unavailable — per-script try/catch in Rust still isolates sync throws */ }

  // Route console.error/console.warn into the capture report too — frameworks
  // narrate their hydration failures there (React hydration errors, webpack
  // chunk loaders, Next.js hints), and those lines are often the only clue to
  // *why* a page produced no intercepts. Original console behavior preserved;
  // op_raze_log bounds count/length so a console flood cannot balloon anything.
  const hookConsole = (c) => {
    if (!c) return;
    for (const level of ["error", "warn"]) {
      const orig = typeof c[level] === "function" ? c[level].bind(c) : null;
      try {
        c[level] = function (...args) {
          try {
            const line = args.map((a) => {
              if (typeof a === "string") return a;
              if (a && a.stack) return String(a.stack);
              try { return JSON.stringify(a); } catch (_) { return String(a); }
            }).join(" ");
            ops.op_raze_log("[console." + level + "] " + line);
          } catch (_) {}
          if (orig) { try { orig(...args); } catch (_) {} }
        };
      } catch (_) {}
    }
  };
  try { hookConsole(g.console); } catch (_) {}
  try { if (w.console && w.console !== g.console) hookConsole(w.console); } catch (_) {}

  // 6. Per-script `document.currentScript`. happy-dom's own script runner is
  //    disabled, so WE evaluate page scripts and currentScript would otherwise be
  //    null. Frameworks read `document.currentScript.parentElement` to locate
  //    their mount node — SvelteKit's client bootstrap mounts into exactly that —
  //    so we point currentScript at the REAL parsed <script> node for the block
  //    being run, matched by inline source text (or external src). That node is
  //    correctly parented in the document tree (e.g. inside the app's mount <div>),
  //    so the mount lands where the framework expects. Grafting a synthetic
  //    <script> onto <head> (the previous behavior) gave a NON-null but WRONG
  //    parent, silently misdirecting the mount into <head> — worse than null.
  const csClaimed = new WeakSet();
  g.__dracoSetCurrentScript = function (inlineSrc, externalUrl) {
    try {
      let node = null;
      const scripts = w.document.getElementsByTagName("script");
      if (externalUrl != null) {
        for (let i = 0; i < scripts.length; i++) {
          const s = scripts[i];
          if (csClaimed.has(s)) continue;
          const raw = s.getAttribute && s.getAttribute("src");
          if (!raw) continue;
          if (s.src === externalUrl || absolutize(raw) === externalUrl) { node = s; break; }
        }
      } else if (inlineSrc != null) {
        const want = String(inlineSrc).trim();
        for (let i = 0; i < scripts.length; i++) {
          const s = scripts[i];
          if (csClaimed.has(s)) continue;
          if (s.src) continue; // inline only
          if ((s.textContent || "").trim() === want) { node = s; break; }
        }
      }
      if (node) {
        csClaimed.add(node);
      } else {
        // Real node not found (shouldn't happen for a parsed inline script): an
        // inert synthetic node parented to <body>, never <head>, so a
        // currentScript.parentElement read lands on a plausible container rather
        // than misdirecting a mount into the document head.
        node = w.document.createElement("script");
        try { (w.document.body || w.document.documentElement || w.document.head).appendChild(node); } catch (_) {}
      }
      Object.defineProperty(w.document, "currentScript", { value: node, configurable: true });
    } catch (_) {}
  };
  g.__dracoClearCurrentScript = function () {
    try { Object.defineProperty(w.document, "currentScript", { value: null, configurable: true }); } catch (_) {}
  };

  // 7. Document lifecycle: readyState / readystatechange / DOMContentLoaded /
  //    window load. We load the HTML via document.write() and evaluate the page's
  //    scripts ourselves, and happy-dom never runs the loading lifecycle in that
  //    path — readyState never advances and neither event dispatches. Framework
  //    boot code commonly gates its DATA loading on exactly these signals (the
  //    classic `document.readyState === "complete" ? run() :
  //    window.addEventListener("load", run)`), so without them the shell hydrates
  //    but the gated data fetches never fire (thrill.com: player/tickets fired,
  //    while the load-gated providers/geolocation/license calls never did). A
  //    real browser fires BOTH events almost immediately after parsing — before
  //    dynamic import()s settle — so late-running chunk code observes
  //    readyState === "complete" and proceeds.
  //
  //    While our page scripts run, the browser-faithful state is "loading"
  //    (scripts execute during parse), so we shadow readyState now and the
  //    runtime calls __dracoFireLifecycle() once the document-order scripts have
  //    evaluated — the parsing-finished moment. Window-level dispatch is
  //    self-adapting: page code registers listeners on globalThis (the page's
  //    `window`), whose listener registry may or may not be shared with the
  //    happy-dom Window, so probes detect whether a dispatch reached the other
  //    target and fire a synthetic one only when it did not (never double-fires
  //    a shared registry).
  let dracoRs = "loading";
  try {
    Object.defineProperty(w.document, "readyState", {
      configurable: true,
      get: function () { return dracoRs; },
    });
  } catch (_) {}
  let gDclSeen = false, wLoadSeen = false;
  try { if (g !== w && typeof g.addEventListener === "function") g.addEventListener("DOMContentLoaded", function () { gDclSeen = true; }); } catch (_) {}
  try { w.addEventListener("load", function () { wLoadSeen = true; }); } catch (_) {}
  function mkEvent(name, opts) {
    try { return new (w.Event || g.Event)(name, opts || {}); } catch (_) { return { type: name }; }
  }
  let lifecycleFired = false;
  g.__dracoFireLifecycle = function () {
    if (lifecycleFired) return;
    lifecycleFired = true;
    try {
      dracoRs = "interactive";
      try { w.document.dispatchEvent(mkEvent("readystatechange")); } catch (_) {}
      // DOMContentLoaded targets the document and bubbles to window.
      try { w.document.dispatchEvent(mkEvent("DOMContentLoaded", { bubbles: true })); } catch (_) {}
      // If the bubble did not reach globalThis-registered listeners (separate
      // registry), fire a synthetic window-level DCL there.
      if (!gDclSeen && g !== w && typeof g.dispatchEvent === "function") {
        try { g.dispatchEvent(mkEvent("DOMContentLoaded")); } catch (_) {}
      }
      dracoRs = "complete";
      try { w.document.dispatchEvent(mkEvent("readystatechange")); } catch (_) {}
      // load targets the window. Page code's `window` is globalThis; dispatch
      // there first, then cover the happy-dom Window if it was not reached.
      let gLoadOk = false;
      if (g !== w && typeof g.dispatchEvent === "function") {
        try { g.dispatchEvent(mkEvent("load")); gLoadOk = true; } catch (_) {}
      }
      if (!wLoadSeen) {
        try { w.dispatchEvent(mkEvent("load")); } catch (_) {}
      }
      // Direct handler properties (onload) assigned but not reached by either
      // dispatch (e.g. assigned on the alias without a wired registry).
      if (!gLoadOk && !wLoadSeen && typeof g.onload === "function") {
        try { g.onload(mkEvent("load")); } catch (_) {}
      }
    } catch (_) {}
  };

  // Expose the serializer the Rust side calls after the capture window.
  g.__dracoSerialize = function () {
    try { return w.document.documentElement.outerHTML; } catch (_) { return ""; }
  };
})();
