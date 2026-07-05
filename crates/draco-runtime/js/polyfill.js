// draco-runtime — vendored, in-repo DOM + scheduler polyfill (Slice 3, canonical §8).
//
// This file is executed once at isolate startup (NOT a build.rs snapshot — see
// lib.rs module docs for the rationale). It is intentionally hand-written and
// pragmatic: the goal is that mainstream SPA bundles (React/Vue/Svelte runtime
// glue, analytics beacons, hydration shims) do not throw on load, so their
// `fetch`/`XMLHttpRequest` calls fire and we can capture the endpoints. It is
// NOT a spec-complete DOM. We deliberately do not vendor linkedom.
//
// Everything here runs in the V8 global scope. deno_core already provides
// `globalThis.queueMicrotask` and a minimal `console`; we build the rest.
//
// Timers are backed by the Rust async op `op_sleep(ms)` (see lib.rs). A pending
// timer keeps the deno_core event loop non-idle, which is exactly the signal the
// Rust capture-window driver uses to detect quiescence.

"use strict";
(function bootstrap(global) {
  const ops = Deno.core.ops;

  // ------------------------------------------------------------------
  // console (ensure it exists; deno_core provides one, but be defensive)
  // ------------------------------------------------------------------
  if (!global.console) {
    const w = (s) => {
      try {
        Deno.core.print(String(s) + "\n");
      } catch (_) {
        /* ignore */
      }
    };
    global.console = {
      log: w,
      info: w,
      warn: w,
      error: w,
      debug: w,
      trace: w,
      dir: w,
      group: () => {},
      groupEnd: () => {},
      table: () => {},
      assert: () => {},
      count: () => {},
      time: () => {},
      timeEnd: () => {},
    };
  }

  // ------------------------------------------------------------------
  // Timer scheduler: setTimeout / setInterval / clear*, requestAnimationFrame
  //
  // Backed by op_sleep (async). Each active timer holds an outstanding op_sleep
  // future, which keeps the event loop alive. clearTimeout marks the id dead so
  // the callback is skipped when the sleep resolves.
  // ------------------------------------------------------------------
  let nextTimerId = 1;
  const liveTimers = new Map(); // id -> { repeat, delay, cb, args, dead }

  function armSleep(id) {
    const t = liveTimers.get(id);
    if (!t || t.dead) return;
    // op_sleep resolves after `delay` ms of wall time.
    ops.op_sleep(t.delay).then(() => {
      const cur = liveTimers.get(id);
      if (!cur || cur.dead) return;
      try {
        cur.cb.apply(global, cur.args);
      } catch (e) {
        reportError(e);
      }
      // Re-check: the callback may have cleared this timer.
      const still = liveTimers.get(id);
      if (still && !still.dead && still.repeat) {
        armSleep(id);
      } else {
        liveTimers.delete(id);
      }
    });
  }

  function makeTimer(cb, delay, args, repeat) {
    const id = nextTimerId++;
    const fn = typeof cb === "function" ? cb : () => {};
    const d = Math.max(0, (delay | 0) || 0);
    liveTimers.set(id, { repeat, delay: d, cb: fn, args: args || [], dead: false });
    armSleep(id);
    return id;
  }

  global.setTimeout = function (cb, delay, ...args) {
    return makeTimer(cb, delay, args, false);
  };
  global.setInterval = function (cb, delay, ...args) {
    return makeTimer(cb, delay, args, true);
  };
  global.clearTimeout = function (id) {
    const t = liveTimers.get(id);
    if (t) t.dead = true;
    liveTimers.delete(id);
  };
  global.clearInterval = global.clearTimeout;

  // setImmediate → 0ms timer (some bundles / polyfills reach for it).
  global.setImmediate = function (cb, ...args) {
    return makeTimer(cb, 0, args, false);
  };
  global.clearImmediate = global.clearTimeout;

  // requestAnimationFrame → ~60fps timer; callback gets a DOMHighResTimeStamp.
  const startTime = Date.now();
  global.requestAnimationFrame = function (cb) {
    return makeTimer(
      () => {
        try {
          cb(Date.now() - startTime);
        } catch (e) {
          reportError(e);
        }
      },
      16,
      [],
      false,
    );
  };
  global.cancelAnimationFrame = global.clearTimeout;

  function reportError(e) {
    try {
      const msg = e && e.stack ? e.stack : String(e);
      Deno.core.print("[polyfill] uncaught in timer: " + msg + "\n");
    } catch (_) {
      /* ignore */
    }
  }
  if (!global.reportError) global.reportError = reportError;

  // ------------------------------------------------------------------
  // MessageChannel / MessagePort — React's scheduler uses these to schedule
  // work off the microtask queue. We back postMessage with queueMicrotask so
  // the scheduler makes progress deterministically.
  // ------------------------------------------------------------------
  class MessagePort {
    constructor() {
      this.onmessage = null;
      this._peer = null;
      this._listeners = [];
      this._started = false;
    }
    _deliver(data) {
      const ev = { data, target: this, ports: [], source: null };
      queueMicrotask(() => {
        try {
          if (typeof this.onmessage === "function") this.onmessage(ev);
        } catch (e) {
          reportError(e);
        }
        for (const l of this._listeners.slice()) {
          try {
            l.call(this, ev);
          } catch (e) {
            reportError(e);
          }
        }
      });
    }
    postMessage(data) {
      if (this._peer) this._peer._deliver(data);
    }
    addEventListener(type, fn) {
      if (type === "message" && typeof fn === "function") this._listeners.push(fn);
    }
    removeEventListener(type, fn) {
      if (type !== "message") return;
      const i = this._listeners.indexOf(fn);
      if (i >= 0) this._listeners.splice(i, 1);
    }
    start() {
      this._started = true;
    }
    close() {
      this._peer = null;
      this._listeners = [];
    }
  }
  class MessageChannel {
    constructor() {
      this.port1 = new MessagePort();
      this.port2 = new MessagePort();
      this.port1._peer = this.port2;
      this.port2._peer = this.port1;
    }
  }
  global.MessageChannel = MessageChannel;
  global.MessagePort = MessagePort;

  // ------------------------------------------------------------------
  // Minimal EventTarget (many libs extend / expect it)
  // ------------------------------------------------------------------
  class EventTarget {
    constructor() {
      this.__listeners = Object.create(null);
    }
    addEventListener(type, fn) {
      if (typeof fn !== "function") return;
      (this.__listeners[type] || (this.__listeners[type] = [])).push(fn);
    }
    removeEventListener(type, fn) {
      const arr = this.__listeners[type];
      if (!arr) return;
      const i = arr.indexOf(fn);
      if (i >= 0) arr.splice(i, 1);
    }
    dispatchEvent(ev) {
      const type = ev && ev.type;
      const arr = this.__listeners[type];
      if (arr) {
        for (const fn of arr.slice()) {
          try {
            fn.call(this, ev);
          } catch (e) {
            reportError(e);
          }
        }
      }
      return true;
    }
  }
  global.EventTarget = EventTarget;
  if (!global.Event) {
    global.Event = class Event {
      constructor(type, init) {
        this.type = type;
        this.bubbles = !!(init && init.bubbles);
        this.cancelable = !!(init && init.cancelable);
        this.defaultPrevented = false;
      }
      preventDefault() {
        this.defaultPrevented = true;
      }
      stopPropagation() {}
      stopImmediatePropagation() {}
    };
  }
  if (!global.CustomEvent) {
    global.CustomEvent = class CustomEvent extends global.Event {
      constructor(type, init) {
        super(type, init);
        this.detail = init && init.detail;
      }
    };
  }

  // ------------------------------------------------------------------
  // DOM: a deliberately shallow document/element model. querySelector &
  // friends return a stub element (never null-throws), createElement yields a
  // detached node, appendChild is a no-op that returns the child. Enough that
  // hydration glue and "mount into #root" code paths don't explode.
  // ------------------------------------------------------------------
  const STYLE = Object.create(null);
  function makeStubElement(tagName) {
    const children = [];
    const attrs = Object.create(null);
    const el = {
      nodeType: 1,
      tagName: (tagName || "div").toUpperCase(),
      nodeName: (tagName || "div").toUpperCase(),
      id: "",
      className: "",
      innerHTML: "",
      textContent: "",
      innerText: "",
      value: "",
      checked: false,
      style: Object.create(STYLE),
      dataset: Object.create(null),
      children,
      childNodes: children,
      attributes: attrs,
      classList: {
        add() {},
        remove() {},
        toggle() {},
        contains() {
          return false;
        },
      },
      ownerDocument: null,
      parentNode: null,
      firstChild: null,
      lastChild: null,
      nextSibling: null,
      previousSibling: null,
      setAttribute(k, v) {
        attrs[k] = String(v);
        if (k === "id") this.id = String(v);
      },
      getAttribute(k) {
        return k in attrs ? attrs[k] : null;
      },
      removeAttribute(k) {
        delete attrs[k];
      },
      hasAttribute(k) {
        return k in attrs;
      },
      appendChild(c) {
        children.push(c);
        if (c) c.parentNode = this;
        this.firstChild = children[0] || null;
        this.lastChild = children[children.length - 1] || null;
        return c;
      },
      insertBefore(c, _ref) {
        children.push(c);
        if (c) c.parentNode = this;
        return c;
      },
      removeChild(c) {
        const i = children.indexOf(c);
        if (i >= 0) children.splice(i, 1);
        return c;
      },
      replaceChild(n, _o) {
        return n;
      },
      cloneNode() {
        return makeStubElement(this.tagName);
      },
      contains() {
        return false;
      },
      addEventListener() {},
      removeEventListener() {},
      dispatchEvent() {
        return true;
      },
      getBoundingClientRect() {
        return { top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0, x: 0, y: 0 };
      },
      focus() {},
      blur() {},
      click() {},
      remove() {},
      querySelector() {
        return makeStubElement("div");
      },
      querySelectorAll() {
        return [];
      },
      getElementsByTagName() {
        return [];
      },
      getElementsByClassName() {
        return [];
      },
      append() {},
      prepend() {},
      after() {},
      before() {},
      setProperty() {},
      getContext() {
        return null;
      },
    };
    return el;
  }

  const documentElement = makeStubElement("html");
  const headEl = makeStubElement("head");
  const bodyEl = makeStubElement("body");
  documentElement.appendChild(headEl);
  documentElement.appendChild(bodyEl);

  const doc = Object.assign(new EventTarget(), {
    nodeType: 9,
    documentElement,
    head: headEl,
    body: bodyEl,
    title: "",
    readyState: "complete",
    cookie: "",
    referrer: "",
    characterSet: "UTF-8",
    compatMode: "CSS1Compat",
    hidden: false,
    visibilityState: "visible",
    createElement(tag) {
      const e = makeStubElement(tag);
      e.ownerDocument = doc;
      return e;
    },
    createElementNS(_ns, tag) {
      return doc.createElement(tag);
    },
    createTextNode(text) {
      return { nodeType: 3, textContent: String(text), data: String(text), parentNode: null };
    },
    createDocumentFragment() {
      return makeStubElement("fragment");
    },
    createComment(text) {
      return { nodeType: 8, textContent: String(text), data: String(text) };
    },
    getElementById() {
      return makeStubElement("div");
    },
    querySelector() {
      return makeStubElement("div");
    },
    querySelectorAll() {
      return [];
    },
    getElementsByTagName(tag) {
      if (tag === "head") return [headEl];
      if (tag === "body") return [bodyEl];
      return [];
    },
    getElementsByClassName() {
      return [];
    },
    getElementsByName() {
      return [];
    },
    createEvent() {
      return new global.Event("event");
    },
    write() {},
    writeln() {},
    open() {},
    close() {},
    execCommand() {
      return false;
    },
    elementFromPoint() {
      return null;
    },
  });
  headEl.ownerDocument = doc;
  bodyEl.ownerDocument = doc;
  documentElement.ownerDocument = doc;
  global.document = doc;

  // Node type constants some libs read off Node.
  global.Node = {
    ELEMENT_NODE: 1,
    TEXT_NODE: 3,
    COMMENT_NODE: 8,
    DOCUMENT_NODE: 9,
    DOCUMENT_FRAGMENT_NODE: 11,
  };
  global.HTMLElement = function HTMLElement() {};
  global.Element = function Element() {};
  global.SVGElement = function SVGElement() {};

  // ------------------------------------------------------------------
  // URL shim + location / history / navigator
  //
  // Bare deno_core does NOT ship the WHATWG URL global, so we provide a
  // pragmatic parser. Resolution of relative URLs is delegated to the Rust op
  // `op_resolve_url` (real WHATWG join); component parsing is a regex good
  // enough for location fields and typical `new URL(x).pathname` usage.
  // ------------------------------------------------------------------
  function parseUrlComponents(href) {
    // scheme://host[:port]/path?query#hash
    const m = String(href).match(
      /^([a-zA-Z][a-zA-Z0-9+.-]*:)\/\/([^/?#]*)([^?#]*)(\?[^#]*)?(#.*)?$/,
    );
    if (!m) {
      return {
        href: String(href),
        protocol: "https:",
        host: "localhost",
        hostname: "localhost",
        port: "",
        pathname: "/",
        search: "",
        hash: "",
        origin: "https://localhost",
      };
    }
    const protocol = m[1];
    const authority = m[2] || "";
    const colon = authority.lastIndexOf(":");
    const hasPort = colon > authority.lastIndexOf("]"); // avoid IPv6 brackets
    const hostname = hasPort ? authority.slice(0, colon) : authority;
    const port = hasPort ? authority.slice(colon + 1) : "";
    return {
      href: String(href),
      protocol,
      host: authority,
      hostname,
      port,
      pathname: m[3] || "/",
      search: m[4] || "",
      hash: m[5] || "",
      origin: protocol + "//" + authority,
    };
  }

  global.URL = class URL {
    constructor(url, base) {
      let resolved = String(url);
      if (base != null) {
        try {
          resolved = ops.op_resolve_url(String(base), String(url));
        } catch (_) {
          resolved = String(url);
        }
      }
      const c = parseUrlComponents(resolved);
      this.href = c.href;
      this.protocol = c.protocol;
      this.host = c.host;
      this.hostname = c.hostname;
      this.port = c.port;
      this.pathname = c.pathname;
      this.search = c.search;
      this.hash = c.hash;
      this.origin = c.origin;
      const qi = this.search.indexOf("?");
      this.searchParams = new global.URLSearchParams(
        qi >= 0 ? this.search.slice(qi + 1) : this.search.replace(/^\?/, ""),
      );
    }
    toString() {
      return this.href;
    }
    toJSON() {
      return this.href;
    }
  };
  if (typeof global.URLSearchParams !== "function") {
    global.URLSearchParams = class URLSearchParams {
      constructor(init) {
        this._p = [];
        if (typeof init === "string") {
          for (const part of init.replace(/^\?/, "").split("&")) {
            if (!part) continue;
            const eq = part.indexOf("=");
            const k = eq >= 0 ? part.slice(0, eq) : part;
            const v = eq >= 0 ? part.slice(eq + 1) : "";
            this._p.push([decodeURIComponent(k), decodeURIComponent(v)]);
          }
        } else if (init && typeof init.forEach === "function") {
          init.forEach((v, k) => this._p.push([String(k), String(v)]));
        } else if (init && typeof init === "object") {
          for (const k of Object.keys(init)) this._p.push([k, String(init[k])]);
        }
      }
      get(k) {
        const e = this._p.find((p) => p[0] === k);
        return e ? e[1] : null;
      }
      getAll(k) {
        return this._p.filter((p) => p[0] === k).map((p) => p[1]);
      }
      has(k) {
        return this._p.some((p) => p[0] === k);
      }
      set(k, v) {
        this.delete(k);
        this._p.push([k, String(v)]);
      }
      append(k, v) {
        this._p.push([k, String(v)]);
      }
      delete(k) {
        this._p = this._p.filter((p) => p[0] !== k);
      }
      forEach(cb, thisArg) {
        for (const [k, v] of this._p) cb.call(thisArg, v, k, this);
      }
      keys() {
        return this._p.map((p) => p[0])[Symbol.iterator]();
      }
      values() {
        return this._p.map((p) => p[1])[Symbol.iterator]();
      }
      entries() {
        return this._p.slice()[Symbol.iterator]();
      }
      toString() {
        return this._p
          .map(([k, v]) => encodeURIComponent(k) + "=" + encodeURIComponent(v))
          .join("&");
      }
      [Symbol.iterator]() {
        return this.entries();
      }
    };
  }

  // __DRACO_URL__ is injected by the Rust side before this file runs.
  const rawUrl = global.__DRACO_URL__ || "https://localhost/";
  const parsed = parseUrlComponents(rawUrl);
  const loc = {
    href: rawUrl,
    protocol: parsed.protocol,
    host: parsed.host,
    hostname: parsed.hostname,
    port: parsed.port,
    pathname: parsed.pathname,
    search: parsed.search,
    hash: parsed.hash,
    origin: parsed.origin,
    assign() {},
    replace() {},
    reload() {},
    toString() {
      return this.href;
    },
  };
  global.location = loc;
  doc.location = loc;
  doc.URL = rawUrl;
  doc.documentURI = rawUrl;

  global.history = {
    length: 1,
    state: null,
    scrollRestoration: "auto",
    pushState(state) {
      this.state = state;
    },
    replaceState(state) {
      this.state = state;
    },
    back() {},
    forward() {},
    go() {},
  };

  global.navigator = {
    userAgent:
      "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) draco/0.1 Safari/537.36",
    language: "en-US",
    languages: ["en-US", "en"],
    platform: "Linux x86_64",
    onLine: true,
    cookieEnabled: true,
    hardwareConcurrency: 1,
    maxTouchPoints: 0,
    vendor: "",
    product: "Gecko",
    doNotTrack: null,
    sendBeacon(url, data) {
      // Beacons are real intercepts (analytics/telemetry). Route to fetch so
      // they are captured too.
      try {
        global.fetch(url, { method: "POST", body: data, keepalive: true });
      } catch (_) {
        /* ignore */
      }
      return true;
    },
    clipboard: { writeText: () => Promise.resolve(), readText: () => Promise.resolve("") },
    serviceWorker: { register: () => Promise.reject(new Error("no sw")), ready: new Promise(() => {}) },
  };

  // ------------------------------------------------------------------
  // Storage (in-memory localStorage / sessionStorage)
  // ------------------------------------------------------------------
  function makeStorage() {
    const map = new Map();
    return {
      getItem(k) {
        k = String(k);
        return map.has(k) ? map.get(k) : null;
      },
      setItem(k, v) {
        map.set(String(k), String(v));
      },
      removeItem(k) {
        map.delete(String(k));
      },
      clear() {
        map.clear();
      },
      key(i) {
        return Array.from(map.keys())[i] ?? null;
      },
      get length() {
        return map.size;
      },
    };
  }
  global.localStorage = makeStorage();
  global.sessionStorage = makeStorage();

  // ------------------------------------------------------------------
  // window plumbing + assorted globals bundles poke at
  // ------------------------------------------------------------------
  global.window = global;
  global.self = global;
  global.top = global;
  global.parent = global;
  global.frames = global;
  global.globalThis = global;
  global.name = "";
  global.closed = false;
  global.length = 0;
  global.devicePixelRatio = 1;
  global.innerWidth = 1280;
  global.innerHeight = 800;
  global.outerWidth = 1280;
  global.outerHeight = 800;
  global.screenX = 0;
  global.screenY = 0;
  global.pageXOffset = 0;
  global.pageYOffset = 0;
  global.scrollX = 0;
  global.scrollY = 0;
  global.screen = {
    width: 1280,
    height: 800,
    availWidth: 1280,
    availHeight: 800,
    colorDepth: 24,
    pixelDepth: 24,
    orientation: { type: "landscape-primary", angle: 0 },
  };

  // window is an EventTarget too.
  const winEvents = new EventTarget();
  global.addEventListener = winEvents.addEventListener.bind(winEvents);
  global.removeEventListener = winEvents.removeEventListener.bind(winEvents);
  global.dispatchEvent = winEvents.dispatchEvent.bind(winEvents);

  global.scroll = function () {};
  global.scrollTo = function () {};
  global.scrollBy = function () {};
  global.alert = function () {};
  global.confirm = function () {
    return false;
  };
  global.prompt = function () {
    return null;
  };
  global.open = function () {
    return null;
  };
  global.close = function () {};
  global.focus = function () {};
  global.blur = function () {};
  global.matchMedia = function (q) {
    return {
      matches: false,
      media: q,
      onchange: null,
      addListener() {},
      removeListener() {},
      addEventListener() {},
      removeEventListener() {},
      dispatchEvent() {
        return true;
      },
    };
  };
  global.getComputedStyle = function () {
    return {
      getPropertyValue() {
        return "";
      },
    };
  };
  global.requestIdleCallback = function (cb) {
    return makeTimer(
      () => cb({ didTimeout: false, timeRemaining: () => 0 }),
      1,
      [],
      false,
    );
  };
  global.cancelIdleCallback = global.clearTimeout;

  // performance (deno_core may provide one; be defensive).
  if (!global.performance || typeof global.performance.now !== "function") {
    const perfStart = Date.now();
    global.performance = {
      now() {
        return Date.now() - perfStart;
      },
      timeOrigin: perfStart,
      mark() {},
      measure() {},
      getEntriesByName() {
        return [];
      },
      getEntriesByType() {
        return [];
      },
      clearMarks() {},
      clearMeasures() {},
    };
  }

  // MutationObserver / ResizeObserver / IntersectionObserver — no-op shims so
  // component libraries that instantiate them on mount don't throw.
  class NoopObserver {
    constructor(cb) {
      this._cb = cb;
    }
    observe() {}
    unobserve() {}
    disconnect() {}
    takeRecords() {
      return [];
    }
  }
  global.MutationObserver = NoopObserver;
  global.ResizeObserver = NoopObserver;
  global.IntersectionObserver = NoopObserver;
  global.PerformanceObserver = NoopObserver;

})(globalThis);
