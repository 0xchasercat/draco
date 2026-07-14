// Draco Tier 2 — per-isolate compatibility shims installed after Window mirroring.
(function installRuntimeCoverage(g) {
  "use strict";

  const pageWindow = (() => {
    try {
      return g.document && g.document.defaultView;
    } catch (_) {
      return null;
    }
  })();
  const expose = (name, value) => {
    try { g[name] = value; } catch (_) {}
    try { if (pageWindow) pageWindow[name] = value; } catch (_) {}
  };

  // Patch the exact happy-dom URL constructor mirrored onto page scope. Its
  // inherited createObjectURL() reaches URL$1.createObjectURL(), which delegates
  // to the bundled base URL via super.createObjectURL() and throws because that
  // base has no object-URL implementation. Define own statics on the page-visible
  // constructor; never replace it, so new URL() and its prototype stay intact.
  try {
    let nextObjectURL = 1;
    const createObjectURL = function () {
      let origin = "null";
      try {
        if (g.location && g.location.origin && g.location.origin !== "null") {
          origin = g.location.origin;
        }
      } catch (_) {}
      return "blob:" + origin + "/draco-" + nextObjectURL++;
    };
    const revokeObjectURL = function () {};
    const installObjectURLStatics = (URLCtor) => {
      if (typeof URLCtor !== "function") return;
      Object.defineProperty(URLCtor, "createObjectURL", {
        value: createObjectURL,
        writable: true,
        configurable: true,
      });
      Object.defineProperty(URLCtor, "revokeObjectURL", {
        value: revokeObjectURL,
        writable: true,
        configurable: true,
      });
    };

    installObjectURLStatics(g.URL);
    if (pageWindow && pageWindow.URL !== g.URL) {
      installObjectURLStatics(pageWindow.URL);
    }
  } catch (_) {}

  // Worker code cannot receive host capabilities in this air-gapped isolate. An
  // inert EventTarget-shaped constructor is still enough for feature detection
  // and worker-optional bundles to keep hydrating instead of throwing at boot.
  try {
    if (typeof g.Worker !== "function") {
      const EventTargetBase = typeof g.EventTarget === "function" ? g.EventTarget : class {};
      class Worker extends EventTargetBase {
        constructor(scriptURL, options) {
          super();
          this.url = String(scriptURL || "");
          this.type = options && options.type === "module" ? "module" : "classic";
          this.onmessage = null;
          this.onmessageerror = null;
          this.onerror = null;
        }
        postMessage() {}
        terminate() {}
      }
      expose("Worker", Worker);
    }
  } catch (_) {}

  // happy-dom intentionally returns null without a native canvas adapter. Supply
  // the state and no-op drawing surface app boot code expects; this does not claim
  // to render pixels.
  try {
    const Canvas = g.HTMLCanvasElement;
    if (Canvas && Canvas.prototype) {
      class CanvasRenderingContext2D {
        constructor(canvas) {
          this.canvas = canvas;
          this.fillStyle = "#000000";
          this.strokeStyle = "#000000";
          this.globalAlpha = 1;
          this.globalCompositeOperation = "source-over";
          this.lineWidth = 1;
          this.lineCap = "butt";
          this.lineJoin = "miter";
          this.miterLimit = 10;
          this.font = "10px sans-serif";
          this.textAlign = "start";
          this.textBaseline = "alphabetic";
          this.imageSmoothingEnabled = true;
          this._stack = [];
        }
        save() {
          this._stack.push({
            fillStyle: this.fillStyle,
            strokeStyle: this.strokeStyle,
            globalAlpha: this.globalAlpha,
            lineWidth: this.lineWidth,
            font: this.font,
            textAlign: this.textAlign,
            textBaseline: this.textBaseline,
          });
        }
        restore() { const state = this._stack.pop(); if (state) Object.assign(this, state); }
        beginPath() {}
        closePath() {}
        moveTo() {}
        lineTo() {}
        rect() {}
        roundRect() {}
        arc() {}
        arcTo() {}
        ellipse() {}
        bezierCurveTo() {}
        quadraticCurveTo() {}
        clip() {}
        fill() {}
        stroke() {}
        clearRect() {}
        fillRect() {}
        strokeRect() {}
        fillText() {}
        strokeText() {}
        drawImage() {}
        translate() {}
        rotate() {}
        scale() {}
        transform() {}
        setTransform() {}
        resetTransform() {}
        setLineDash() {}
        getLineDash() { return []; }
        measureText(text) {
          const width = String(text).length * 6;
          return {
            width,
            actualBoundingBoxLeft: 0,
            actualBoundingBoxRight: width,
            actualBoundingBoxAscent: 8,
            actualBoundingBoxDescent: 2,
          };
        }
        createLinearGradient() { return { addColorStop() {} }; }
        createRadialGradient() { return { addColorStop() {} }; }
        createConicGradient() { return { addColorStop() {} }; }
        createPattern() { return null; }
        createImageData(width, height) {
          const w = Math.max(0, Number(width) || 0);
          const h = Math.max(0, Number(height) || 0);
          return { width: w, height: h, data: new Uint8ClampedArray(w * h * 4) };
        }
        getImageData(_x, _y, width, height) { return this.createImageData(width, height); }
        putImageData() {}
        isPointInPath() { return false; }
        isPointInStroke() { return false; }
      }

      const original = Canvas.prototype.getContext;
      const contexts = new WeakMap();
      Canvas.prototype.getContext = function (type, attrs) {
        if (String(type).toLowerCase() === "2d") {
          let context = contexts.get(this);
          if (!context) {
            context = new CanvasRenderingContext2D(this);
            contexts.set(this, context);
          }
          return context;
        }
        return typeof original === "function" ? original.call(this, type, attrs) : null;
      };
      if (typeof Canvas.prototype.toDataURL !== "function") {
        Canvas.prototype.toDataURL = function () { return "data:image/png;base64,"; };
      }
      if (typeof g.CanvasRenderingContext2D !== "function") {
        expose("CanvasRenderingContext2D", CanvasRenderingContext2D);
      }
    }
  } catch (_) {}
})(globalThis);
