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
  // DOM: a real-enough *node tree*.
  //
  // Earlier this model was deliberately shallow (querySelector returned a fresh
  // throwaway stub, insertBefore ignored its anchor, text/comment nodes had no
  // sibling links). That was enough that hydration glue did not throw, but the
  // rendered output was not observable and mount containers were not stable
  // across lookups. Real client frameworks (Vue's runtime-dom `nodeOps`, React
  // DOM, Svelte) drive a genuine tree during `mount()`: createElement /
  // createTextNode / createComment, then insertBefore(child, anchor) /
  // appendChild / removeChild, reading back parentNode / nextSibling /
  // previousSibling / firstChild / lastChild / childNodes, and setting
  // textContent. So we now maintain a coherent, mutation-consistent node tree.
  //
  // It is still NOT a spec-complete DOM: no layout, no CSS cascade, no live
  // NodeLists, and querySelector understands only `#id`, `.class`, `tag`, and
  // `[attr]` / `[attr=val]` selectors (optionally combined) — enough for a
  // framework to find its mount container and for a test to observe that the
  // framework actually rendered into the tree. We deliberately do not vendor
  // linkedom.
  // ------------------------------------------------------------------
  const STYLE = Object.create(null);

  // Element id -> element registry, so getElementById / querySelector('#x')
  // return the *same* stable node every time (what a real page relies on).
  const idRegistry = new Map();

  function registerId(el, id) {
    if (!id) return;
    // First element with a given id wins (matches getElementById semantics for
    // the common single-id case) but keep the latest as a fallback.
    if (!idRegistry.has(id)) idRegistry.set(id, el);
  }

  // A live-ish `children` view (element nodes only) over a node's childNodes.
  function elementChildren(node) {
    const out = [];
    for (const c of node.childNodes) if (c.nodeType === 1) out.push(c);
    return out;
  }

  // Detach `node` from its current parent, fixing up sibling/parent links.
  function detach(node) {
    const p = node.parentNode;
    if (!p) return;
    const cn = p.childNodes;
    const i = cn.indexOf(node);
    if (i >= 0) cn.splice(i, 1);
    const prev = node.previousSibling;
    const next = node.nextSibling;
    if (prev) prev.nextSibling = next;
    if (next) next.previousSibling = prev;
    node.parentNode = null;
    node.previousSibling = null;
    node.nextSibling = null;
    refreshEnds(p);
  }

  function refreshEnds(node) {
    const cn = node.childNodes;
    node.firstChild = cn.length ? cn[0] : null;
    node.lastChild = cn.length ? cn[cn.length - 1] : null;
  }

  // Core insertion primitive: insert `node` before `ref` in `parent`
  // (append if `ref` is null/undefined). Maintains every tree link. This is the
  // one operation Vue's `nodeOps.insert` funnels through:
  //   insert: (child, parent, anchor) => parent.insertBefore(child, anchor||null)
  function insertBeforeImpl(parent, node, ref) {
    if (node == null) return node;
    // A DocumentFragment inserts its children, then empties.
    if (node.nodeType === 11) {
      const kids = node.childNodes.slice();
      for (const k of kids) insertBeforeImpl(parent, k, ref);
      return node;
    }
    detach(node);
    const cn = parent.childNodes;
    let idx = cn.length;
    if (ref != null) {
      const at = cn.indexOf(ref);
      if (at >= 0) idx = at;
    }
    const prev = idx > 0 ? cn[idx - 1] : null;
    const next = idx < cn.length ? cn[idx] : null;
    cn.splice(idx, 0, node);
    node.parentNode = parent;
    node.previousSibling = prev;
    node.nextSibling = next;
    if (prev) prev.nextSibling = node;
    if (next) next.previousSibling = node;
    refreshEnds(parent);
    if (node.nodeType === 1 && node.id) registerId(node, node.id);
    return node;
  }

  function removeChildImpl(parent, node) {
    if (node && node.parentNode === parent) detach(node);
    return node;
  }

  // Serialize a subtree to HTML-ish text (used by the innerHTML getter and for
  // test observability). Not spec-perfect, but faithful enough to read back the
  // text a framework rendered.
  const VOID_TAGS = new Set([
    "area", "base", "br", "col", "embed", "hr", "img", "input",
    "link", "meta", "param", "source", "track", "wbr",
  ]);
  function serializeChildren(node) {
    let out = "";
    for (const c of node.childNodes) out += serializeNode(c);
    return out;
  }
  function serializeNode(node) {
    if (node.nodeType === 3) return String(node.data == null ? "" : node.data);
    if (node.nodeType === 8) return "<!--" + String(node.data || "") + "-->";
    if (node.nodeType === 11) return serializeChildren(node);
    if (node.nodeType !== 1) return "";
    const tag = String(node.tagName || "div").toLowerCase();
    let attrs = "";
    for (const k of Object.keys(node.attributes)) {
      attrs += " " + k + '="' + String(node.attributes[k]) + '"';
    }
    if (VOID_TAGS.has(tag)) return "<" + tag + attrs + ">";
    return "<" + tag + attrs + ">" + serializeChildren(node) + "</" + tag + ">";
  }

  // Concatenate the text content of a subtree (textContent getter).
  function textOf(node) {
    if (node.nodeType === 3) return String(node.data == null ? "" : node.data);
    if (node.nodeType === 8) return "";
    let out = "";
    for (const c of node.childNodes) out += textOf(c);
    return out;
  }

  // Selector matching against a single element for our supported subset.
  function elementMatches(el, sel) {
    if (el.nodeType !== 1) return false;
    sel = sel.trim();
    if (sel === "*") return true;
    // Compound: split into simple tokens (#id, .class, tag, [attr], [attr=v]).
    const tokens = sel.match(/[#.]?[\w-]+|\[[^\]]+\]/g);
    if (!tokens) return false;
    for (const tok of tokens) {
      if (tok[0] === "#") {
        if (el.id !== tok.slice(1)) return false;
      } else if (tok[0] === ".") {
        const cls = tok.slice(1);
        const list = String(el.className || "").split(/\s+/);
        if (!list.includes(cls)) return false;
      } else if (tok[0] === "[") {
        const m = tok.slice(1, -1).match(/^([\w-]+)(?:=["']?([^"'\]]*)["']?)?$/);
        if (!m) return false;
        const name = m[1];
        if (!(name in el.attributes)) return false;
        if (m[2] !== undefined && String(el.attributes[name]) !== m[2]) return false;
      } else {
        if (String(el.tagName).toLowerCase() !== tok.toLowerCase()) return false;
      }
    }
    return true;
  }

  // Depth-first search for the first descendant matching `sel`. Only the last
  // compound between combinators is honored (descendant combinator collapses to
  // "match the final compound anywhere under root"), which is all a framework's
  // mount lookup needs.
  function queryOne(root, sel) {
    const parts = String(sel).split(",");
    for (const part of parts) {
      const compound = part.trim().split(/\s+/).pop();
      const hit = findFirst(root, (el) => elementMatches(el, compound));
      if (hit) return hit;
    }
    return null;
  }
  function queryAll(root, sel) {
    const out = [];
    const parts = String(sel).split(",");
    for (const part of parts) {
      const compound = part.trim().split(/\s+/).pop();
      walk(root, (el) => {
        if (elementMatches(el, compound) && !out.includes(el)) out.push(el);
      });
    }
    return out;
  }
  function findFirst(node, pred) {
    for (const c of node.childNodes) {
      if (c.nodeType === 1 && pred(c)) return c;
      const inner = findFirst(c, pred);
      if (inner) return inner;
    }
    return null;
  }
  function walk(node, fn) {
    for (const c of node.childNodes) {
      if (c.nodeType === 1) fn(c);
      walk(c, fn);
    }
  }

  // Node factory. `nodeType`: 1 element, 3 text, 8 comment, 11 fragment.
  function makeNode(nodeType, tagName, text) {
    const childNodes = [];
    const attrs = Object.create(null);
    const node = {
      nodeType,
      tagName: nodeType === 1 ? (tagName || "div").toUpperCase() : undefined,
      nodeName:
        nodeType === 3
          ? "#text"
          : nodeType === 8
            ? "#comment"
            : nodeType === 11
              ? "#document-fragment"
              : (tagName || "div").toUpperCase(),
      childNodes,
      attributes: attrs,
      ownerDocument: null,
      parentNode: null,
      firstChild: null,
      lastChild: null,
      nextSibling: null,
      previousSibling: null,
    };

    if (nodeType === 3 || nodeType === 8) {
      // Text / comment: carry a mutable `data`, mirrored by nodeValue/textContent.
      node.data = text == null ? "" : String(text);
      Object.defineProperty(node, "nodeValue", {
        get() {
          return this.data;
        },
        set(v) {
          this.data = v == null ? "" : String(v);
        },
      });
      Object.defineProperty(node, "textContent", {
        get() {
          return this.data;
        },
        set(v) {
          this.data = v == null ? "" : String(v);
        },
      });
      return node;
    }

    // Element / fragment.
    node.id = "";
    node.className = "";
    node.value = "";
    node.checked = false;
    node.style = Object.create(STYLE);
    node.style.setProperty = function (k, v) {
      this[k] = v;
    };
    node.style.removeProperty = function (k) {
      delete this[k];
    };
    node.style.getPropertyValue = function (k) {
      return this[k] == null ? "" : String(this[k]);
    };
    node.dataset = Object.create(null);
    node.namespaceURI = null;

    node.classList = {
      _el: node,
      add(...cs) {
        const set = new Set(String(this._el.className).split(/\s+/).filter(Boolean));
        for (const c of cs) set.add(c);
        this._el.className = Array.from(set).join(" ");
      },
      remove(...cs) {
        const set = new Set(String(this._el.className).split(/\s+/).filter(Boolean));
        for (const c of cs) set.delete(c);
        this._el.className = Array.from(set).join(" ");
      },
      toggle(c) {
        if (this.contains(c)) this.remove(c);
        else this.add(c);
      },
      contains(c) {
        return String(this._el.className).split(/\s+/).includes(c);
      },
    };

    Object.defineProperty(node, "children", {
      get() {
        return elementChildren(this);
      },
    });
    Object.defineProperty(node, "childElementCount", {
      get() {
        return elementChildren(this).length;
      },
    });
    Object.defineProperty(node, "textContent", {
      get() {
        return textOf(this);
      },
      set(v) {
        // Replace all children with a single text node (real setElementText).
        while (this.childNodes.length) removeChildImpl(this, this.childNodes[0]);
        const t = makeNode(3, null, v == null ? "" : String(v));
        insertBeforeImpl(this, t, null);
      },
    });
    Object.defineProperty(node, "innerText", {
      get() {
        return textOf(this);
      },
      set(v) {
        this.textContent = v;
      },
    });
    Object.defineProperty(node, "innerHTML", {
      get() {
        return serializeChildren(this);
      },
      set(v) {
        while (this.childNodes.length) removeChildImpl(this, this.childNodes[0]);
        const frag = parseHTML(String(v), doc);
        insertBeforeImpl(this, frag, null);
      },
    });
    Object.defineProperty(node, "outerHTML", {
      get() {
        return serializeNode(this);
      },
    });

    node.setAttribute = function (k, v) {
      attrs[k] = String(v);
      if (k === "id") {
        this.id = String(v);
        registerId(this, this.id);
      } else if (k === "class") {
        this.className = String(v);
      }
    };
    node.setAttributeNS = function (_ns, k, v) {
      this.setAttribute(k, v);
    };
    node.getAttribute = function (k) {
      if (k === "id") return this.id || null;
      if (k === "class") return this.className || null;
      return k in attrs ? attrs[k] : null;
    };
    node.getAttributeNS = function (_ns, k) {
      return this.getAttribute(k);
    };
    node.removeAttribute = function (k) {
      delete attrs[k];
      if (k === "class") this.className = "";
    };
    node.removeAttributeNS = function (_ns, k) {
      this.removeAttribute(k);
    };
    node.hasAttribute = function (k) {
      if (k === "id") return !!this.id;
      if (k === "class") return !!this.className;
      return k in attrs;
    };
    node.appendChild = function (c) {
      return insertBeforeImpl(this, c, null);
    };
    node.insertBefore = function (c, ref) {
      return insertBeforeImpl(this, c, ref);
    };
    node.removeChild = function (c) {
      return removeChildImpl(this, c);
    };
    node.replaceChild = function (n, o) {
      if (o && o.parentNode === this) {
        insertBeforeImpl(this, n, o);
        removeChildImpl(this, o);
      }
      return o;
    };
    node.append = function (...cs) {
      for (const c of cs) {
        if (typeof c === "string") insertBeforeImpl(this, makeNode(3, null, c), null);
        else insertBeforeImpl(this, c, null);
      }
    };
    node.prepend = function (...cs) {
      const first = this.firstChild;
      for (const c of cs) {
        const n = typeof c === "string" ? makeNode(3, null, c) : c;
        insertBeforeImpl(this, n, first);
      }
    };
    node.after = function (...cs) {
      const p = this.parentNode;
      if (!p) return;
      const ref = this.nextSibling;
      for (const c of cs) {
        const n = typeof c === "string" ? makeNode(3, null, c) : c;
        insertBeforeImpl(p, n, ref);
      }
    };
    node.before = function (...cs) {
      const p = this.parentNode;
      if (!p) return;
      for (const c of cs) {
        const n = typeof c === "string" ? makeNode(3, null, c) : c;
        insertBeforeImpl(p, n, this);
      }
    };
    node.remove = function () {
      detach(this);
    };
    node.cloneNode = function (deep) {
      const copy = makeNode(this.nodeType, this.tagName);
      copy.ownerDocument = this.ownerDocument;
      copy.className = this.className;
      for (const k of Object.keys(attrs)) copy.setAttribute(k, attrs[k]);
      if (this.id) {
        copy.id = this.id;
        copy.setAttribute("id", this.id);
      }
      if (deep) {
        for (const c of this.childNodes) copy.appendChild(c.cloneNode(true));
      }
      return copy;
    };
    node.contains = function (other) {
      let n = other;
      while (n) {
        if (n === this) return true;
        n = n.parentNode;
      }
      return false;
    };
    node.hasChildNodes = function () {
      return this.childNodes.length > 0;
    };
    node.getRootNode = function () {
      let n = this;
      while (n.parentNode) n = n.parentNode;
      return n;
    };
    node.addEventListener = function () {};
    node.removeEventListener = function () {};
    node.dispatchEvent = function () {
      return true;
    };
    node.getBoundingClientRect = function () {
      return { top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0, x: 0, y: 0 };
    };
    node.getClientRects = function () {
      return [];
    };
    node.focus = function () {};
    node.blur = function () {};
    node.click = function () {};
    node.scrollIntoView = function () {};
    node.querySelector = function (sel) {
      return queryOne(this, sel);
    };
    node.querySelectorAll = function (sel) {
      return queryAll(this, sel);
    };
    node.getElementsByTagName = function (tag) {
      const t = String(tag).toLowerCase();
      return queryAll(this, t === "*" ? "*" : t);
    };
    node.getElementsByClassName = function (cls) {
      return queryAll(this, "." + cls);
    };
    node.closest = function (sel) {
      let n = this;
      while (n && n.nodeType === 1) {
        if (elementMatches(n, sel)) return n;
        n = n.parentNode;
      }
      return null;
    };
    node.matches = function (sel) {
      return elementMatches(this, sel);
    };
    node.getContext = function () {
      return null;
    };
    node.insertAdjacentElement = function (pos, el) {
      insertAdjacent(this, pos, el);
      return el;
    };
    node.insertAdjacentHTML = function (pos, html) {
      const frag = parseHTML(String(html), doc);
      insertAdjacent(this, pos, frag);
    };
    node.insertAdjacentText = function (pos, text) {
      insertAdjacent(this, pos, makeNode(3, null, text));
    };
    return node;
  }

  function insertAdjacent(el, pos, node) {
    const p = el.parentNode;
    switch (String(pos).toLowerCase()) {
      case "beforebegin":
        if (p) insertBeforeImpl(p, node, el);
        break;
      case "afterbegin":
        insertBeforeImpl(el, node, el.firstChild);
        break;
      case "beforeend":
        insertBeforeImpl(el, node, null);
        break;
      case "afterend":
        if (p) insertBeforeImpl(p, node, el.nextSibling);
        break;
    }
  }

  // --- A tiny, forgiving HTML parser -------------------------------------
  // Turns a fragment of HTML into a DocumentFragment of real nodes. Used to
  // materialize the page <body> subtree (so a framework's mount container is a
  // stable node) and to back the innerHTML setter. It handles tags, attributes
  // (single/double/unquoted), text, comments, and void elements; it is NOT a
  // spec parser (no namespaces, no error recovery beyond "close what you can").
  function parseHTML(html, ownerDoc) {
    const frag = makeNode(11, null);
    frag.ownerDocument = ownerDoc;
    const stack = [frag];
    let i = 0;
    const n = html.length;
    const top = () => stack[stack.length - 1];
    while (i < n) {
      const lt = html.indexOf("<", i);
      if (lt < 0) {
        appendText(top(), html.slice(i), ownerDoc);
        break;
      }
      if (lt > i) appendText(top(), html.slice(i, lt), ownerDoc);

      if (html.startsWith("<!--", lt)) {
        const end = html.indexOf("-->", lt + 4);
        const stop = end < 0 ? n : end;
        const c = makeNode(8, null, html.slice(lt + 4, stop));
        c.ownerDocument = ownerDoc;
        insertBeforeImpl(top(), c, null);
        i = end < 0 ? n : end + 3;
        continue;
      }
      if (html.startsWith("<!", lt)) {
        // Doctype / declaration: skip to '>'.
        const end = html.indexOf(">", lt);
        i = end < 0 ? n : end + 1;
        continue;
      }
      if (html.startsWith("</", lt)) {
        const end = html.indexOf(">", lt);
        const name = html.slice(lt + 2, end < 0 ? n : end).trim().toLowerCase();
        // Pop until we find a matching open tag (forgiving).
        for (let s = stack.length - 1; s >= 1; s--) {
          if (String(stack[s].tagName).toLowerCase() === name) {
            stack.length = s;
            break;
          }
        }
        i = end < 0 ? n : end + 1;
        continue;
      }

      // Opening tag.
      const end = html.indexOf(">", lt);
      if (end < 0) {
        appendText(top(), html.slice(lt), ownerDoc);
        break;
      }
      let raw = html.slice(lt + 1, end);
      const selfClose = raw.endsWith("/");
      if (selfClose) raw = raw.slice(0, -1);
      const { tag, attrs } = parseOpenTag(raw);
      if (!tag) {
        i = end + 1;
        continue;
      }
      const el = makeNode(1, tag);
      el.ownerDocument = ownerDoc;
      for (const [k, v] of attrs) el.setAttribute(k, v);
      insertBeforeImpl(top(), el, null);
      const lower = tag.toLowerCase();
      if (lower === "script" || lower === "style" || lower === "textarea" || lower === "title") {
        // Rawtext element: consume verbatim to its close tag.
        const close = "</" + lower;
        const ci = html.toLowerCase().indexOf(close, end + 1);
        const stop = ci < 0 ? n : ci;
        if (stop > end + 1) appendText(el, html.slice(end + 1, stop), ownerDoc);
        const gi = ci < 0 ? n : html.indexOf(">", ci);
        i = gi < 0 ? n : gi + 1;
        continue;
      }
      if (!selfClose && !VOID_TAGS.has(lower)) stack.push(el);
      i = end + 1;
    }
    return frag;
  }
  function appendText(parent, text, ownerDoc) {
    if (!text) return;
    const t = makeNode(3, null, text);
    t.ownerDocument = ownerDoc;
    insertBeforeImpl(parent, t, null);
  }
  function parseOpenTag(raw) {
    const m = raw.match(/^\s*([\w:-]+)/);
    if (!m) return { tag: null, attrs: [] };
    const tag = m[1];
    const attrs = [];
    const re = /([\w:-]+)(?:\s*=\s*("[^"]*"|'[^']*'|[^\s"'>]+))?/g;
    let rest = raw.slice(m[0].length);
    let am;
    while ((am = re.exec(rest))) {
      const k = am[1];
      let v = am[2];
      if (v == null) v = "";
      else if (v[0] === '"' || v[0] === "'") v = v.slice(1, -1);
      attrs.push([k, v]);
    }
    return { tag, attrs };
  }

  function makeStubElement(tag) {
    const e = makeNode(1, tag);
    e.ownerDocument = doc;
    return e;
  }

  const documentElement = makeNode(1, "html");
  const headEl = makeNode(1, "head");
  const bodyEl = makeNode(1, "body");
  documentElement.appendChild(headEl);
  documentElement.appendChild(bodyEl);

  const doc = Object.assign(new EventTarget(), {
    nodeType: 9,
    nodeName: "#document",
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
      const e = makeNode(1, tag);
      e.ownerDocument = doc;
      return e;
    },
    createElementNS(ns, tag) {
      const e = doc.createElement(tag);
      e.namespaceURI = ns;
      return e;
    },
    createTextNode(text) {
      const t = makeNode(3, null, text);
      t.ownerDocument = doc;
      return t;
    },
    createDocumentFragment() {
      const f = makeNode(11, null);
      f.ownerDocument = doc;
      return f;
    },
    createComment(text) {
      const c = makeNode(8, null, text);
      c.ownerDocument = doc;
      return c;
    },
    getElementById(id) {
      const hit = idRegistry.get(String(id));
      if (hit) return hit;
      // Fall back to a live tree walk (ids added without setAttribute).
      return findFirst(documentElement, (el) => el.id === String(id));
    },
    querySelector(sel) {
      return queryOne(documentElement, sel);
    },
    querySelectorAll(sel) {
      return queryAll(documentElement, sel);
    },
    getElementsByTagName(tag) {
      const t = String(tag).toLowerCase();
      if (t === "head") return [headEl];
      if (t === "body") return [bodyEl];
      if (t === "html") return [documentElement];
      return queryAll(documentElement, t === "*" ? "*" : t);
    },
    getElementsByClassName(cls) {
      return queryAll(documentElement, "." + cls);
    },
    getElementsByName(name) {
      return queryAll(documentElement, '[name="' + name + '"]');
    },
    contains(node) {
      return documentElement.contains(node);
    },
    createEvent() {
      return new global.Event("event");
    },
    createRange() {
      return {
        setStart() {},
        setEnd() {},
        selectNodeContents() {},
        collapse() {},
        cloneContents() {
          return doc.createDocumentFragment();
        },
        deleteContents() {},
        insertNode() {},
        getBoundingClientRect() {
          return { top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0, x: 0, y: 0 };
        },
        getClientRects() {
          return [];
        },
        createContextualFragment(html) {
          return parseHTML(String(html), doc);
        },
      };
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
    addEventListener() {},
    removeEventListener() {},
  });
  headEl.ownerDocument = doc;
  bodyEl.ownerDocument = doc;
  documentElement.ownerDocument = doc;

  // Materialize the page <body> subtree from the HTML the Rust side injected,
  // so a framework's mount container (e.g. <div id="app">) is a *real, stable*
  // node found by getElementById / querySelector. Best-effort: if parsing yields
  // nothing (or no body markup was injected), we keep the empty <body>.
  const bodyHtml = global.__DRACO_BODY_HTML__;
  if (typeof bodyHtml === "string" && bodyHtml.trim()) {
    try {
      const frag = parseHTML(bodyHtml, doc);
      // Drop <script> elements — their code is executed separately by the Rust
      // driver; we only want the static mount scaffold in the tree.
      const scripts = queryAll(frag, "script");
      for (const s of scripts) detach(s);
      bodyEl.appendChild(frag);
    } catch (e) {
      reportError(e);
    }
  }

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
