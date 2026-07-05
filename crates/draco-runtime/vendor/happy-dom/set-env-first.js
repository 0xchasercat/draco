// Runs FIRST — before any polyfill/library that probes Node globals. Bare deno_core
// provides almost nothing, so we establish a Node-compatible global base here.
// (This is baked into the V8 snapshot at build time.)
(function () {
  var g = globalThis;
  if (!g.global) g.global = g;
  if (!g.self) g.self = g;
  if (!g.SharedArrayBuffer) g.SharedArrayBuffer = g.ArrayBuffer;
  if (!g.process) g.process = {
    env: {}, platform: "linux", arch: "x64", argv: [], version: "v20.0.0",
    versions: { node: "20.0.0" }, cwd: function () { return "/"; },
    nextTick: function (fn) { var a = Array.prototype.slice.call(arguments, 1); Promise.resolve().then(function () { fn.apply(null, a); }); },
    on: function () {}, off: function () {}, once: function () {}, emit: function () {}, exit: function () {},
    hrtime: function () { return [0, 0]; }, stdout: { write: function () {} }, stderr: { write: function () {} },
  };

  // Timer scheduler backed by the Rust async op `op_sleep` (called at call-time so
  // the runtime-registered op resolves after snapshot restore). Each live timer
  // holds an outstanding op_sleep future, keeping the deno_core event loop alive
  // for the capture window; clear* marks the id dead.
  var nextId = 1;
  var live = new Map();
  function arm(id) {
    var t = live.get(id);
    if (!t || t.dead) return;
    Deno.core.ops.op_sleep(t.delay).then(function () {
      var cur = live.get(id);
      if (!cur || cur.dead) return;
      try { cur.cb.apply(g, cur.args); } catch (e) { try { Deno.core.print("[timer] " + (e && e.stack || e) + "\n"); } catch (_) {} }
      var still = live.get(id);
      if (still && !still.dead && still.repeat) arm(id); else live.delete(id);
    });
  }
  function make(cb, delay, args, repeat) {
    var id = nextId++;
    live.set(id, { cb: typeof cb === "function" ? cb : function () {}, delay: Math.max(0, (delay | 0) || 0), args: args || [], repeat: repeat, dead: false });
    arm(id);
    return id;
  }
  g.setTimeout = function (cb, delay) { return make(cb, delay, Array.prototype.slice.call(arguments, 2), false); };
  g.setInterval = function (cb, delay) { return make(cb, delay, Array.prototype.slice.call(arguments, 2), true); };
  g.clearTimeout = function (id) { var t = live.get(id); if (t) t.dead = true; live.delete(id); };
  g.clearInterval = g.clearTimeout;
  g.setImmediate = function (cb) { return make(cb, 0, Array.prototype.slice.call(arguments, 1), false); };
  g.clearImmediate = g.clearTimeout;
  var t0 = Date.now();
  g.requestAnimationFrame = function (cb) { return make(function () { try { cb(Date.now() - t0); } catch (e) {} }, 16, [], false); };
  g.cancelAnimationFrame = g.clearTimeout;


  // MessageChannel / MessagePort — frameworks' schedulers (React) flush work
  // across these. postMessage is backed by queueMicrotask for deterministic
  // progress inside the capture window.
  function _safeErr(e) { try { Deno.core.print("[msgport] " + (e && e.stack || e) + "\n"); } catch (_) {} }
  function MessagePort() { this.onmessage = null; this._peer = null; this._listeners = []; }
  MessagePort.prototype._deliver = function (data) {
    var self = this; var ev = { data: data, target: this, ports: [], source: null };
    queueMicrotask(function () {
      try { if (typeof self.onmessage === "function") self.onmessage(ev); } catch (e) { _safeErr(e); }
      var ls = self._listeners.slice();
      for (var i = 0; i < ls.length; i++) { try { ls[i].call(self, ev); } catch (e) { _safeErr(e); } }
    });
  };
  MessagePort.prototype.postMessage = function (data) { if (this._peer) this._peer._deliver(data); };
  MessagePort.prototype.addEventListener = function (t, fn) { if (t === "message" && typeof fn === "function") this._listeners.push(fn); };
  MessagePort.prototype.removeEventListener = function (t, fn) { if (t !== "message") return; var i = this._listeners.indexOf(fn); if (i >= 0) this._listeners.splice(i, 1); };
  MessagePort.prototype.start = function () {};
  MessagePort.prototype.close = function () { this._peer = null; this._listeners = []; };
  function MessageChannel() { this.port1 = new MessagePort(); this.port2 = new MessagePort(); this.port1._peer = this.port2; this.port2._peer = this.port1; }
  if (!g.MessagePort) g.MessagePort = MessagePort;
  if (!g.MessageChannel) g.MessageChannel = MessageChannel;

  // Snapshot isolates omit some newer V8 features a couple of polyfills probe for.
  try { if (!Object.getOwnPropertyDescriptor(ArrayBuffer.prototype, "resizable")) Object.defineProperty(ArrayBuffer.prototype, "resizable", { get: function () { return false; }, configurable: true }); } catch (e) {}
  try { if (!Object.getOwnPropertyDescriptor(g.SharedArrayBuffer.prototype, "growable")) Object.defineProperty(g.SharedArrayBuffer.prototype, "growable", { get: function () { return false; }, configurable: true }); } catch (e) {}
})();
