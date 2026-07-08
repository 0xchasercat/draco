// Draco Tier 2 — web-platform completeness polyfills.
//
// Baked into the V8 startup snapshot (see build.rs) AFTER the web-primitive base
// bundle and happy-dom. PRINCIPLE: an SPA hydrating in Draco must never
// hard-crash on a missing *standard* web global. A missing API should degrade to
// an inert, spec-plausible stub that lets hydration proceed — not a
// `ReferenceError: X is not defined` that kills the framework's component tree
// (the class of failure that produced 0-char output on real sites).
//
// Every global is installed ONLY IF ABSENT (`configurable: true`), so:
//   * a real implementation from the base bundle wins if present, and
//   * happy-dom's per-isolate Window mirror (in glue.js) still overwrites these
//     with real DOM impls after snapshot restore where it provides them.
// We only ever fill genuine gaps.
//
// CONSTRAINTS (snapshot-eval context):
//   * Runs in JsRuntimeForSnapshot with NO ops registered — nothing here may
//     CALL a `Deno.core` op at eval time. Defining functions that call ops later
//     (e.g. via `setTimeout`, which the base binds lazily) is fine.
//   * Each group is guarded so one failure cannot abort snapshot evaluation.
//
// indexedDB is intentionally NOT here — it is provided by the vendored, full
// in-memory `fake-indexeddb` engine (js/fake-indexeddb.iife.js) so scripts that
// read data back don't hit a soft-stub wall.
(function (g) {
  "use strict";

  const def = (name, value) => {
    try {
      if (typeof g[name] === "undefined") {
        Object.defineProperty(g, name, {
          value,
          writable: true,
          configurable: true,
        });
      }
    } catch (_) {}
  };

  // --- DOMException (subclassable; bare deno_core V8 lacks it) --------------
  // fake-indexeddb's IDB error classes `extends DOMException` at class-definition
  // (snapshot-eval) time and construct `new DOMException(message, name)` at
  // runtime, so this must be a real, subclassable class — not an object stub.
  try {
    if (typeof g.DOMException === "undefined") {
      const CODES = {
        IndexSizeError: 1, HierarchyRequestError: 3, WrongDocumentError: 4,
        InvalidCharacterError: 5, NoModificationAllowedError: 7, NotFoundError: 8,
        NotSupportedError: 9, InUseAttributeError: 10, InvalidStateError: 11,
        SyntaxError: 12, InvalidModificationError: 13, NamespaceError: 14,
        InvalidAccessError: 15, SecurityError: 18, NetworkError: 19,
        AbortError: 20, URLMismatchError: 21, QuotaExceededError: 22,
        TimeoutError: 23, InvalidNodeTypeError: 24, DataCloneError: 25,
      };
      class DOMException extends Error {
        constructor(message = "", name = "Error") {
          super(message);
          Object.defineProperty(this, "name", {
            value: name,
            writable: true,
            configurable: true,
          });
          Object.defineProperty(this, "code", {
            value: CODES[name] || 0,
            writable: true,
            configurable: true,
          });
        }
      }
      def("DOMException", DOMException);
    }
  } catch (_) {}

  // --- setImmediate / clearImmediate (Node-ism a few libs reach for) -------
  // Defining it short-circuits fake-indexeddb's scheduler chain to a safe path
  // (it otherwise probes a `new Function("return setImmediate")` trick). Backed
  // by the base's real op_sleep timer at call time.
  def("setImmediate", function (fn) {
    const args = Array.prototype.slice.call(arguments, 1);
    return setTimeout(function () {
      fn.apply(undefined, args);
    }, 0);
  });
  def("clearImmediate", function (id) {
    clearTimeout(id);
  });

  // --- Storage: localStorage / sessionStorage (in-memory) ------------------
  try {
    if (typeof g.localStorage === "undefined") {
      class MemoryStorage {
        constructor() {
          Object.defineProperty(this, "_m", { value: new Map() });
        }
        get length() {
          return this._m.size;
        }
        key(i) {
          const ks = Array.from(this._m.keys());
          return i >= 0 && i < ks.length ? ks[i] : null;
        }
        getItem(k) {
          k = String(k);
          return this._m.has(k) ? this._m.get(k) : null;
        }
        setItem(k, v) {
          this._m.set(String(k), String(v));
        }
        removeItem(k) {
          this._m.delete(String(k));
        }
        clear() {
          this._m.clear();
        }
      }
      def("Storage", MemoryStorage);
      def("localStorage", new MemoryStorage());
      def("sessionStorage", new MemoryStorage());
    }
  } catch (_) {}

  // --- matchMedia ----------------------------------------------------------
  def("matchMedia", function (query) {
    return {
      matches: false,
      media: String(query || ""),
      onchange: null,
      addListener() {},
      removeListener() {},
      addEventListener() {},
      removeEventListener() {},
      dispatchEvent() {
        return false;
      },
    };
  });

  // --- Observers (no-op; happy-dom provides MutationObserver) --------------
  const NoopObserver = class {
    constructor(_cb) {}
    observe() {}
    unobserve() {}
    disconnect() {}
    takeRecords() {
      return [];
    }
  };
  def("IntersectionObserver", NoopObserver);
  def("ResizeObserver", NoopObserver);
  def("PerformanceObserver", NoopObserver);

  // --- Idle / animation frame callbacks ------------------------------------
  // Backed by the base's op_sleep timer scheduler (referenced lazily, at call
  // time — safe at snapshot eval). The capture window's quiesce detector treats
  // idle repeating timers as quiesced, so a rAF loop won't pin the job open.
  def("requestIdleCallback", function (cb) {
    return setTimeout(function () {
      try {
        cb({
          didTimeout: false,
          timeRemaining() {
            return 0;
          },
        });
      } catch (_) {}
    }, 1);
  });
  def("cancelIdleCallback", function (id) {
    clearTimeout(id);
  });
  def("requestAnimationFrame", function (cb) {
    return setTimeout(function () {
      try {
        cb(Date.now());
      } catch (_) {}
    }, 16);
  });
  def("cancelAnimationFrame", function (id) {
    clearTimeout(id);
  });

  // structuredClone is provided by the vendored fake-indexeddb.iife.js bundle
  // (@ungap/structured-clone) — a real, spec-compliant impl, not a JSON fallback.

  // --- crypto.getRandomValues / randomUUID ---------------------------------
  // SURVIVAL fallback only (Math.random-based) — NOT cryptographically secure.
  // Tier 2 executes page JS only to extract already-public content; crypto here
  // is never a security boundary. Installed only if the base didn't provide it.
  try {
    if (typeof g.crypto === "undefined") {
      Object.defineProperty(g, "crypto", {
        value: {},
        writable: true,
        configurable: true,
      });
    }
    const c = g.crypto;
    if (c && typeof c.getRandomValues !== "function") {
      Object.defineProperty(c, "getRandomValues", {
        value: function (arr) {
          for (let i = 0; i < arr.length; i++) {
            arr[i] = Math.floor(Math.random() * 256);
          }
          return arr;
        },
        writable: true,
        configurable: true,
      });
    }
    if (c && typeof c.randomUUID !== "function") {
      Object.defineProperty(c, "randomUUID", {
        value: function () {
          return "10000000-1000-4000-8000-100000000000".replace(
            /[018]/g,
            function (ch) {
              const n = Number(ch);
              return (
                n ^
                (Math.floor(Math.random() * 256) & (15 >> (n / 4)))
              ).toString(16);
            },
          );
        },
        writable: true,
        configurable: true,
      });
    }
  } catch (_) {}

  // --- BroadcastChannel (no-op) --------------------------------------------
  def(
    "BroadcastChannel",
    class {
      constructor(name) {
        this.name = String(name || "");
        this.onmessage = null;
        this.onmessageerror = null;
      }
      postMessage() {}
      close() {}
      addEventListener() {}
      removeEventListener() {}
      dispatchEvent() {
        return false;
      }
    },
  );

  // --- Notification (inert, permission denied) -----------------------------
  try {
    if (typeof g.Notification === "undefined") {
      const N = class {
        constructor() {}
        close() {}
        static requestPermission() {
          return Promise.resolve("denied");
        }
      };
      Object.defineProperty(N, "permission", {
        value: "denied",
        configurable: true,
      });
      def("Notification", N);
    }
  } catch (_) {}
})(globalThis);
