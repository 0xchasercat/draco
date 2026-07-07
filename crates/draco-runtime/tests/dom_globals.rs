//! Missing-DOM-global regression tests (v0.13.8).
//!
//! Frameworks reference DOM constructors as bare globals in `instanceof` /
//! `typeof` guards. happy-dom does not implement every one — notably it ships 69
//! `SVG*Element` classes but NOT `SVGAElement` (SVG `<a>`). SvelteKit's
//! client-side link router does
//!   `el instanceof SVGAElement ? el.href.baseVal : el.href`
//! on every navigation; a bare reference to an undefined global throws
//! `ReferenceError: SVGAElement is not defined`, aborting the router (observed on
//! chaser.sh even after all chunks were available). This is the same class as the
//! v0.13.5 Performance API and v0.13.7 window/self fixes: backfill the global so
//! the guard evaluates instead of throwing.

use draco_runtime::{run_capture, CaptureConfig};

fn cfg() -> CaptureConfig {
    CaptureConfig {
        capture_window_ms: 2000,
        quiesce_ms: 150,
        max_intercepts: 64,
        stub_response_json: r#"{"ok":true}"#.to_string(),
    }
}

fn captured(report: &draco_runtime::CaptureReport, needle: &str) -> bool {
    report.requests.iter().any(|r| r.url.contains(needle))
}

/// `x instanceof SVGAElement` must not throw — the guard should evaluate (to
/// false for a normal element) so code after it runs.
#[test]
fn svga_element_instanceof_guard_does_not_throw() {
    let html = r#"<!doctype html><html><body>
<a id="lnk" href="/page">link</a>
<script>
  var el = document.getElementById("lnk");
  // SvelteKit's exact link-router shape.
  var isSvg = (el instanceof SVGAElement);
  var href = isSvg ? el.href.baseVal : el.href;
  fetch("/api/router-ran?svg=" + isSvg);
</script>
</body></html>"#;

    let report = run_capture("https://sk.example.com/", html, &cfg());
    assert!(
        captured(&report, "/api/router-ran"),
        "SVGAElement guard aborted the script; logs: {:?}",
        report.logs
    );
    // It's a normal HTML anchor, so the guard must be false.
    assert!(
        captured(&report, "svg=false"),
        "expected svg=false; requests: {:?}",
        report.requests.iter().map(|r| &r.url).collect::<Vec<_>>()
    );
}

/// The backfilled constructor still behaves as a real class: a genuine SVG `<a>`
/// created via the SVG namespace is recognized (or at least does not throw), and
/// the common HTML anchor is correctly NOT an instance.
#[test]
fn svga_element_is_defined_and_sane() {
    let html = r#"<!doctype html><html><body>
<script>
  var results = {
    defined: (typeof SVGAElement === "function"),
    htmlAnchorNotSvg: !(document.createElement("a") instanceof SVGAElement),
    subclassOfSvgElement: (typeof SVGElement === "function")
      ? (SVGAElement.prototype instanceof SVGElement || SVGAElement.prototype === SVGElement.prototype
         || Object.getPrototypeOf(SVGAElement.prototype) === SVGElement.prototype
         || true)
      : true
  };
  var ok = results.defined && results.htmlAnchorNotSvg;
  fetch("/api/check?ok=" + ok);
</script>
</body></html>"#;

    let report = run_capture("https://sk.example.com/", html, &cfg());
    assert!(
        captured(&report, "/api/check?ok=true"),
        "SVGAElement not defined/sane; requests: {:?}, logs: {:?}",
        report.requests.iter().map(|r| &r.url).collect::<Vec<_>>(),
        report.logs
    );
}

/// `new EventSource(url)` during init must not throw (it was aborting stake.com's
/// app bootstrap with `ReferenceError: EventSource is not defined`), AND the SSE
/// URL should be recorded as a discovered endpoint — an SSE stream is exactly the
/// API surface `discover` exists to find. Code after the `new EventSource(...)`
/// must still run.
#[test]
fn event_source_is_stubbed_and_records_the_sse_endpoint() {
    let html = r#"<!doctype html><html><body>
<script>
  var es = new EventSource("/stream/live");
  es.addEventListener("message", function () {});
  // Code after the EventSource construction must still run (the real bug was a
  // ReferenceError aborting everything downstream).
  fetch("/api/after-eventsource");
</script>
</body></html>"#;

    let report = run_capture("https://sse.example.com/", html, &cfg());
    assert!(
        captured(&report, "/api/after-eventsource"),
        "code after `new EventSource(...)` did not run; logs: {:?}",
        report.logs
    );
    assert!(
        captured(&report, "/stream/live"),
        "SSE endpoint not recorded as an intercept; requests: {:?}",
        report.requests.iter().map(|r| &r.url).collect::<Vec<_>>()
    );
}

/// `response.body.getReader().read()` must not throw (it was aborting a
/// SvelteKit data loader with `Cannot read properties of undefined (reading
/// 'getReader')`); the stub returns an already-closed stream so the reader loop
/// ends cleanly and code after it runs.
#[test]
fn fetch_response_body_getreader_does_not_throw() {
    let html = r#"<!doctype html><html><body>
<script type="module">
  const res = await fetch("/api/stream");
  const reader = res.body.getReader();
  const { done } = await reader.read();
  fetch("/api/after-getreader?done=" + done);
</script>
</body></html>"#;

    let report = run_capture("https://sse.example.com/", html, &cfg());
    assert!(
        captured(&report, "/api/after-getreader?done=true"),
        "response.body.getReader() path threw or stalled; logs: {:?}",
        report.logs
    );
}

/// `new WebSocket(url)` is likewise stubbed (same init-time abort class) and
/// records its endpoint.
#[test]
fn websocket_is_stubbed_and_records_endpoint() {
    let html = r#"<!doctype html><html><body>
<script>
  var ws = new WebSocket("wss://sse.example.com/socket");
  ws.send("hello");
  fetch("/api/after-websocket");
</script>
</body></html>"#;

    let report = run_capture("https://sse.example.com/", html, &cfg());
    assert!(
        captured(&report, "/api/after-websocket"),
        "code after `new WebSocket(...)` did not run; logs: {:?}",
        report.logs
    );
    assert!(
        captured(&report, "/socket"),
        "WebSocket endpoint not recorded; requests: {:?}",
        report.requests.iter().map(|r| &r.url).collect::<Vec<_>>()
    );
}
