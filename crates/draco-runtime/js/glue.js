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
  g.window = w;
  g.document = w.document;
  if (w.navigator) g.navigator = w.navigator;
  if (w.location) g.location = w.location;
  if (w.history) g.history = w.history;

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
