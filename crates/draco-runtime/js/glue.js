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
  function record(via, method, u, headers, bodyStr) {
    const req = { via, method: (method || "GET").toUpperCase(), url: absolutize(u), headers: headers || [], body: bodyStr == null ? null : String(bodyStr) };
    let respJson;
    try { respJson = ops.op_raze_fetch(JSON.stringify(req)); } catch (_) { respJson = null; }
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
    return {
      ok: status >= 200 && status < 300, status, statusText: status === 200 ? "OK" : "",
      url: finalUrl, redirected: false, type: "basic", headers, bodyUsed: false,
      async text() { guard(); return bodyText; },
      async json() { guard(); return JSON.parse(bodyText); },
      async arrayBuffer() { guard(); return new TextEncoder().encode(bodyText).buffer; },
      async blob() { guard(); return { size: bodyText.length, type: "", text: async () => bodyText }; },
      clone() { return makeResponse(stub, finalUrl); },
    };
  }
  const doFetch = function fetch(input, init) {
    init = init || {};
    let u, method = init.method || "GET", headers = headersToPairs(init.headers), body = init.body;
    if (input && typeof input === "object" && "url" in input) {
      u = input.url;
      if (!init.method && input.method) method = input.method;
      if ((!init.headers || headers.length === 0) && input.headers) headers = headersToPairs(input.headers);
      if (init.body == null && input.body != null) body = input.body;
    } else { u = String(input); }
    const stub = record("fetch", method, u, headers, bodyToString(body));
    return Promise.resolve(makeResponse(stub, absolutize(u)));
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
      const stub = record("xhr", this._m, this._u, this._h, body == null ? null : bodyToString(body));
      queueMicrotask(() => {
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

  // Dynamic script chunk loader hook. Frameworks such as Next/Webpack load lazy
  // chunks by creating <script src="/_next/static/chunks/..."></script> and
  // resolving a promise from onload/onerror. happy-dom's script loading is disabled
  // (the isolate is air-gapped), so execute prefetched chunk sources synchronously
  // when such script nodes are appended/inserted. Missing chunks fire onerror so
  // page code sees the same failure shape as a network miss.
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
    let src = null;
    try { src = ops.op_raze_resource(u); } catch (_) { src = null; }
    if (typeof src !== "string") {
      try { src = ops.op_raze_load_script(u); } catch (_) { src = null; }
    }
    if (typeof src !== "string") { fireScriptEvent(node, "error", u); return false; }
    try {
      // Indirect eval runs in global scope. //# sourceURL gives stack traces an
      // absolute URL and lets relative dynamic imports inside chunks resolve.
      (0, eval)(src + "\n//# sourceURL=" + u);
      fireScriptEvent(node, "load", u);
      return true;
    } catch (e) {
      logSwallowed("script", e);
      fireScriptEvent(node, "error", u);
      return true;
    }
  }
  function hookInsertion(proto, name) {
    if (!proto || typeof proto[name] !== "function") return;
    const orig = proto[name];
    proto[name] = function (...args) {
      // Run known prefetched scripts BEFORE insertion so happy-dom's disabled file
      // loader never tries (and fails) to fetch them itself.
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
    try { Deno.core.print("[glue] swallowed " + kind + ": " + (e && e.stack || e) + "\n"); } catch (_) {}
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

  // 6. Per-inline-script `document.currentScript`. When WE evaluate page scripts
  //    (rather than happy-dom's own runner) currentScript is null; analytics/tag
  //    scripts commonly read `document.currentScript.parentElement`. Point it at a
  //    fresh <script> appended to <head> for the currently running inline script.
  g.__dracoSetCurrentScript = function () {
    try {
      const s = w.document.createElement("script");
      w.document.head.appendChild(s);
      Object.defineProperty(w.document, "currentScript", { value: s, configurable: true });
    } catch (_) {}
  };
  g.__dracoClearCurrentScript = function () {
    try { Object.defineProperty(w.document, "currentScript", { value: null, configurable: true }); } catch (_) {}
  };

  // Expose the serializer the Rust side calls after the capture window.
  g.__dracoSerialize = function () {
    try { return w.document.documentElement.outerHTML; } catch (_) { return ""; }
  };
})();
