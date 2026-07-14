use std::time::{Duration, Instant};

use draco_runtime::{run_capture, CaptureConfig};
use draco_types::RuntimeOutcome;

mod common;
use common::null_fetcher;

fn cfg() -> CaptureConfig {
    CaptureConfig {
        capture_window_ms: 1_000,
        quiesce_ms: 80,
        max_intercepts: 64,
        stub_response_json: r#"{"ok":true}"#.to_string(),
    }
}

fn captured(report: &draco_runtime::CaptureReport, needle: &str) -> bool {
    report
        .requests
        .iter()
        .any(|request| request.url.contains(needle))
}

#[test]
fn happy_dom_internal_fetch_uses_the_broker() {
    let html = r#"<!doctype html><html><head>
<link rel="preload" as="fetch" href="/api/preloaded">
</head><body><script>fetch("/api/page-fetch");</script></body></html>"#;

    let report = run_capture("https://preload.example/", html, &cfg(), null_fetcher());

    assert!(
        captured(&report, "/api/preloaded"),
        "happy-dom internal fetch bypassed the broker: {:?}; logs: {:?}",
        report.requests,
        report.logs
    );
    assert!(captured(&report, "/api/page-fetch"));
    assert!(
        report
            .logs
            .iter()
            .all(|line| !line.contains("send is not a function")),
        "internal fetch reached happy-dom's unavailable Node adapter: {:?}",
        report.logs
    );
}

#[test]
fn worker_object_url_and_url_constructor_are_present() {
    let html = r#"<!doctype html><html><body><script>
const resolved = new URL("../asset", location.href).href;
const objectURL = URL.createObjectURL(new Blob(["worker"]));
const worker = new Worker(objectURL);
const ok = resolved === "https://api.example/asset"
  && objectURL.startsWith("blob:")
  && typeof URL.revokeObjectURL === "function"
  && typeof worker.postMessage === "function"
  && typeof worker.terminate === "function"
  && worker instanceof Worker;
URL.revokeObjectURL(objectURL);
fetch("/api/coverage?ok=" + ok);
</script></body></html>"#;

    let report = run_capture("https://api.example/app/page", html, &cfg(), null_fetcher());

    assert!(
        captured(&report, "/api/coverage?ok=true"),
        "URL/Worker compatibility path failed: {:?}; logs: {:?}",
        report.requests,
        report.logs
    );
}

#[test]
fn canvas_2d_basics_are_non_null_and_stateful() {
    let html = r##"<!doctype html><html><body><canvas id="c"></canvas><script>
const canvas = document.getElementById("c");
const a = canvas.getContext("2d");
const b = canvas.getContext("2d");
a.textBaseline = "middle";
a.fillStyle = "#123456";
a.fillRect(0, 0, 10, 10);
a.beginPath();
a.moveTo(0, 0); a.lineTo(10, 10); a.stroke();
const metric = a.measureText("abcd");
const image = a.createImageData(2, 3);
const ok = a === b
  && a.canvas === canvas
  && a.textBaseline === "middle"
  && metric.width > 0
  && image.data.length === 24;
fetch("/api/canvas?ok=" + ok);
</script></body></html>"##;

    let report = run_capture("https://canvas.example/", html, &cfg(), null_fetcher());

    assert!(
        captured(&report, "/api/canvas?ok=true"),
        "canvas shim did not preserve basic 2D behavior: {:?}; logs: {:?}",
        report.requests,
        report.logs
    );
}

#[test]
fn request_animation_frame_loop_quiesces_before_the_hard_cap() {
    let html = r#"<!doctype html><html><body><script>
let frames = 0;
function paint() {
  frames += 1;
  if (frames === 2) fetch("/api/frame-two");
  requestAnimationFrame(paint);
}
requestAnimationFrame(paint);
</script></body></html>"#;
    let config = CaptureConfig {
        capture_window_ms: 1_000,
        quiesce_ms: 80,
        ..cfg()
    };
    let started = Instant::now();
    let report = run_capture("https://raf.example/", html, &config, null_fetcher());
    let elapsed = started.elapsed();

    assert!(captured(&report, "/api/frame-two"));
    assert_eq!(report.outcome, RuntimeOutcome::Quiesced);
    assert!(
        elapsed < Duration::from_millis(config.capture_window_ms),
        "rAF loop ran to the hard cap: {elapsed:?}; logs: {:?}",
        report.logs
    );
}
