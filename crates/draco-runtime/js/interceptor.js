// draco-runtime — fetch / XMLHttpRequest interceptor (Slice 3, canonical §8).
//
// Executed at isolate startup, after polyfill.js. Overrides globalThis.fetch and
// globalThis.XMLHttpRequest so every outbound request is (a) recorded in the
// Rust capture buffer and (b) answered with a synthetic stub response, so the
// page keeps hydrating and reveals more endpoints instead of hanging on a
// network call that would never complete in the isolate.
//
// The Rust op `op_raze_fetch(request_json)` records the request and returns a
// JSON string describing the stub response:
//   { "status": u16, "headers": [[k,v],...], "body": string }
// `body` is the stub body text (from cfg.stub_response_json).

"use strict";
(function installInterceptor(global) {
  const ops = Deno.core.ops;

  // Normalize a fetch Headers-ish argument into [[k, v], ...].
  function headersToPairs(h) {
    const out = [];
    if (!h) return out;
    if (typeof h.forEach === "function" && !Array.isArray(h)) {
      // Headers instance or Map.
      h.forEach((v, k) => out.push([String(k), String(v)]));
      return out;
    }
    if (Array.isArray(h)) {
      for (const pair of h) {
        if (pair && pair.length >= 2) out.push([String(pair[0]), String(pair[1])]);
      }
      return out;
    }
    if (typeof h === "object") {
      for (const k of Object.keys(h)) out.push([k, String(h[k])]);
    }
    return out;
  }

  function bodyToString(body) {
    if (body == null) return null;
    if (typeof body === "string") return body;
    try {
      if (body instanceof URLSearchParams) return body.toString();
    } catch (_) {
      /* URLSearchParams may be absent */
    }
    if (typeof body === "object") {
      // FormData-ish / Blob-ish: best-effort. Bundles mostly send JSON strings.
      if (typeof body.toString === "function") {
        const s = body.toString();
        if (s !== "[object Object]") return s;
      }
      try {
        return JSON.stringify(body);
      } catch (_) {
        return null;
      }
    }
    return String(body);
  }

  // Resolve a possibly-relative URL against location.href. Bare deno_core has
  // no WHATWG URL global, so we resolve in Rust via op_resolve_url.
  function absolutize(url) {
    const base = (global.location && global.location.href) || "https://localhost/";
    try {
      return ops.op_resolve_url(base, String(url));
    } catch (_) {
      return String(url);
    }
  }

  function record(via, method, url, headers, bodyStr) {
    const req = {
      via, // "fetch" | "xhr"
      method: (method || "GET").toUpperCase(),
      url: absolutize(url),
      headers: headers || [],
      body: bodyStr == null ? null : String(bodyStr),
    };
    // Returns the stub response JSON (or throws if max_intercepts exceeded —
    // we swallow that so the page keeps running).
    let respJson;
    try {
      respJson = ops.op_raze_fetch(JSON.stringify(req));
    } catch (e) {
      respJson = null;
    }
    if (!respJson) {
      return { status: 200, headers: [["content-type", "application/json"]], body: "{}" };
    }
    try {
      return JSON.parse(respJson);
    } catch (_) {
      return { status: 200, headers: [["content-type", "application/json"]], body: "{}" };
    }
  }

  // ------------------------------------------------------------------
  // fetch
  // ------------------------------------------------------------------
  global.fetch = function fetch(input, init) {
    init = init || {};
    let url;
    let method = init.method || "GET";
    let headers = headersToPairs(init.headers);
    let body = init.body;

    if (input && typeof input === "object" && "url" in input) {
      // Request object.
      url = input.url;
      if (!init.method && input.method) method = input.method;
      if ((!init.headers || headers.length === 0) && input.headers) {
        headers = headersToPairs(input.headers);
      }
      if (init.body == null && input.body != null) body = input.body;
    } else {
      url = String(input);
    }

    const stub = record("fetch", method, url, headers, bodyToString(body));
    return Promise.resolve(makeResponse(stub, absolutize(url)));
  };

  // Minimal Response object sufficient for `.json()`, `.text()`, `.ok`, etc.
  function makeResponse(stub, finalUrl) {
    const status = (stub && stub.status) || 200;
    const bodyText = stub && typeof stub.body === "string" ? stub.body : "{}";
    const headerPairs = (stub && stub.headers) || [];
    const headers = new global.Headers(headerPairs);
    let used = false;
    const guard = () => {
      if (used) throw new TypeError("body already consumed");
      used = true;
    };
    return {
      ok: status >= 200 && status < 300,
      status,
      statusText: status === 200 ? "OK" : "",
      url: finalUrl,
      redirected: false,
      type: "basic",
      headers,
      bodyUsed: false,
      async text() {
        guard();
        return bodyText;
      },
      async json() {
        guard();
        return JSON.parse(bodyText);
      },
      async arrayBuffer() {
        guard();
        const enc = new TextEncoder();
        return enc.encode(bodyText).buffer;
      },
      async blob() {
        guard();
        return { size: bodyText.length, type: "", text: async () => bodyText };
      },
      async formData() {
        guard();
        return new global.FormData();
      },
      clone() {
        return makeResponse(stub, finalUrl);
      },
    };
  }

  // Headers shim if deno_core doesn't provide one.
  if (typeof global.Headers !== "function") {
    global.Headers = class Headers {
      constructor(init) {
        this._m = new Map();
        if (init) {
          const pairs = headersToPairs(init);
          for (const [k, v] of pairs) this._m.set(String(k).toLowerCase(), String(v));
        }
      }
      get(k) {
        const v = this._m.get(String(k).toLowerCase());
        return v == null ? null : v;
      }
      set(k, v) {
        this._m.set(String(k).toLowerCase(), String(v));
      }
      append(k, v) {
        const key = String(k).toLowerCase();
        this._m.set(key, this._m.has(key) ? this._m.get(key) + ", " + v : String(v));
      }
      has(k) {
        return this._m.has(String(k).toLowerCase());
      }
      delete(k) {
        this._m.delete(String(k).toLowerCase());
      }
      forEach(cb, thisArg) {
        this._m.forEach((v, k) => cb.call(thisArg, v, k, this));
      }
      keys() {
        return this._m.keys();
      }
      values() {
        return this._m.values();
      }
      entries() {
        return this._m.entries();
      }
      [Symbol.iterator]() {
        return this._m.entries();
      }
    };
  }
  if (typeof global.FormData !== "function") {
    global.FormData = class FormData {
      constructor() {
        this._d = [];
      }
      append(k, v) {
        this._d.push([k, v]);
      }
      get(k) {
        const e = this._d.find((p) => p[0] === k);
        return e ? e[1] : null;
      }
      getAll(k) {
        return this._d.filter((p) => p[0] === k).map((p) => p[1]);
      }
      has(k) {
        return this._d.some((p) => p[0] === k);
      }
      delete(k) {
        this._d = this._d.filter((p) => p[0] !== k);
      }
      forEach(cb) {
        for (const [k, v] of this._d) cb(v, k, this);
      }
    };
  }
  // Request shim (used when code does `new Request(...)`).
  if (typeof global.Request !== "function") {
    global.Request = class Request {
      constructor(input, init) {
        init = init || {};
        if (input && typeof input === "object" && "url" in input) {
          this.url = input.url;
          this.method = init.method || input.method || "GET";
        } else {
          this.url = String(input);
          this.method = init.method || "GET";
        }
        this.headers = new global.Headers(init.headers);
        this.body = init.body != null ? init.body : null;
        this._bodyInit = this.body;
      }
    };
  }

  // ------------------------------------------------------------------
  // XMLHttpRequest
  // ------------------------------------------------------------------
  const UNSENT = 0,
    OPENED = 1,
    HEADERS_RECEIVED = 2,
    LOADING = 3,
    DONE = 4;

  global.XMLHttpRequest = class XMLHttpRequest {
    constructor() {
      this.readyState = UNSENT;
      this.status = 0;
      this.statusText = "";
      this.responseText = "";
      this.response = "";
      this.responseType = "";
      this.responseURL = "";
      this.timeout = 0;
      this.withCredentials = false;
      this.onreadystatechange = null;
      this.onload = null;
      this.onloadend = null;
      this.onerror = null;
      this.onabort = null;
      this.ontimeout = null;
      this._method = "GET";
      this._url = "";
      this._reqHeaders = [];
      this._respHeaders = [];
      this._listeners = Object.create(null);
      this._aborted = false;
    }
    addEventListener(type, fn) {
      if (typeof fn === "function") (this._listeners[type] || (this._listeners[type] = [])).push(fn);
    }
    removeEventListener(type, fn) {
      const a = this._listeners[type];
      if (!a) return;
      const i = a.indexOf(fn);
      if (i >= 0) a.splice(i, 1);
    }
    _emit(type) {
      const ev = { type, target: this, currentTarget: this, loaded: 0, total: 0 };
      const direct = this["on" + type];
      if (typeof direct === "function") {
        try {
          direct.call(this, ev);
        } catch (e) {
          /* swallow */
        }
      }
      const arr = this._listeners[type];
      if (arr) {
        for (const fn of arr.slice()) {
          try {
            fn.call(this, ev);
          } catch (e) {
            /* swallow */
          }
        }
      }
    }
    open(method, url) {
      this._method = method || "GET";
      this._url = url || "";
      this.readyState = OPENED;
      this._emit("readystatechange");
    }
    setRequestHeader(k, v) {
      this._reqHeaders.push([String(k), String(v)]);
    }
    getResponseHeader(k) {
      const kk = String(k).toLowerCase();
      const hit = this._respHeaders.find((p) => p[0].toLowerCase() === kk);
      return hit ? hit[1] : null;
    }
    getAllResponseHeaders() {
      return this._respHeaders.map(([k, v]) => k + ": " + v).join("\r\n") + "\r\n";
    }
    overrideMimeType() {}
    abort() {
      this._aborted = true;
      this._emit("abort");
    }
    send(body) {
      if (this._aborted) return;
      const stub = record(
        "xhr",
        this._method,
        this._url,
        this._reqHeaders,
        body == null ? null : bodyToString(body),
      );
      // Resolve asynchronously (microtask) to mirror real XHR semantics.
      queueMicrotask(() => {
        if (this._aborted) return;
        this.status = (stub && stub.status) || 200;
        this.statusText = this.status === 200 ? "OK" : "";
        this._respHeaders = (stub && stub.headers) || [];
        const bodyText = stub && typeof stub.body === "string" ? stub.body : "{}";
        this.responseText = bodyText;
        this.responseURL = absolutize(this._url);
        if (this.responseType === "json") {
          try {
            this.response = JSON.parse(bodyText);
          } catch (_) {
            this.response = null;
          }
        } else {
          this.response = bodyText;
        }
        this.readyState = HEADERS_RECEIVED;
        this._emit("readystatechange");
        this.readyState = LOADING;
        this._emit("readystatechange");
        this.readyState = DONE;
        this._emit("readystatechange");
        this._emit("load");
        this._emit("loadend");
      });
    }
  };
  global.XMLHttpRequest.UNSENT = UNSENT;
  global.XMLHttpRequest.OPENED = OPENED;
  global.XMLHttpRequest.HEADERS_RECEIVED = HEADERS_RECEIVED;
  global.XMLHttpRequest.LOADING = LOADING;
  global.XMLHttpRequest.DONE = DONE;
})(globalThis);
