// Draco Tier 2 — hooks that must exist before happy-dom constructs the page Window.
(function installPrelude(g) {
  "use strict";

  // Keep V8's WHATWG URL constructor. happy-dom's mirrored URL constructor can
  // be incomplete in this embedder; runtime_coverage.js restores this after the
  // Window is mirrored and adds the object-URL methods without replacing it.
  try {
    Object.defineProperty(g, "__DRACO_NATIVE_URL__", {
      value: g.URL,
      configurable: true,
    });
  } catch (_) {}

  // happy-dom's internal resource loader does not call window.fetch. It creates
  // its private Fetch class, whose Node http/https adapter is intentionally absent
  // in Draco's browser-only snapshot; without an interceptor Fetch.sendRequest()
  // reaches an undefined request function and throws "send is not a function".
  // Inject the supported fetch interceptor into every page Window up front so
  // preload/resource fetches stay inside the same brokered op as page fetch/XHR.
  try {
    const bundle = g.HappyDOMBundle;
    const BaseWindow = bundle && bundle.Window;
    if (typeof BaseWindow !== "function") return;

    class DracoWindow extends BaseWindow {
      constructor(options) {
        const input = options || {};
        const settings = input.settings || {};
        const fetchSettings = settings.fetch || {};
        const prior = fetchSettings.interceptor || {};
        const interceptor = {
          ...prior,
          async beforeAsyncRequest(context) {
            if (typeof prior.beforeAsyncRequest === "function") {
              const handled = await prior.beforeAsyncRequest(context);
              if (handled) return handled;
            }

            const request = context.request;
            const window = context.window;
            const headers = [];
            try {
              request.headers.forEach((value, name) => {
                headers.push([String(name), String(value)]);
              });
            } catch (_) {}

            const payload = JSON.stringify({
              via: "fetch",
              method: String(request.method || "GET").toUpperCase(),
              url: String(request.url),
              headers,
              body: null,
            });
            let response;
            try {
              response = JSON.parse(await Deno.core.ops.op_raze_fetch(payload));
            } catch (_) {
              response = {
                status: 200,
                headers: [["content-type", "application/json"]],
                body: "{}",
              };
            }

            return new window.Response(
              response && typeof response.body === "string" ? response.body : "{}",
              {
                status: (response && response.status) || 200,
                headers: (response && response.headers) || [],
              },
            );
          },
        };
        super({
          ...input,
          settings: {
            ...settings,
            fetch: { ...fetchSettings, interceptor },
          },
        });
      }
    }

    Object.defineProperty(bundle, "Window", {
      value: DracoWindow,
      writable: true,
      configurable: true,
    });
  } catch (_) {}
})(globalThis);
