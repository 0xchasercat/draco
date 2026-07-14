use draco_runtime::{run_capture, CaptureConfig};

mod common;
use common::null_fetcher;

#[test]
fn oversized_request_body_is_omitted_without_stopping_hydration() {
    let html = r#"<!doctype html><html><body><script>
const oversized = "x".repeat(300 * 1024);
fetch("/api/oversized", {
  method: "POST",
  headers: { "content-type": "application/json" },
  body: oversized,
});
fetch("/api/after-oversized");
</script></body></html>"#;
    let report = run_capture(
        "https://memory.example/",
        html,
        &CaptureConfig {
            capture_window_ms: 1_000,
            quiesce_ms: 100,
            max_intercepts: 64,
            stub_response_json: "{}".to_string(),
        },
        null_fetcher(),
    );

    let oversized = report
        .requests
        .iter()
        .find(|request| request.url.ends_with("/api/oversized"))
        .expect("oversized request should remain observable");
    assert!(oversized.body.is_none());
    assert!(oversized.body_omitted);
    assert!(
        report
            .requests
            .iter()
            .any(|request| request.url.ends_with("/api/after-oversized")),
        "body omission must not stop later hydration work"
    );
    assert!(
        report
            .logs
            .iter()
            .any(|line| line.contains("request body omitted")),
        "body omission should be diagnosable: {:?}",
        report.logs
    );
}

#[test]
fn body_at_the_capture_limit_is_retained_exactly() {
    let html = r#"<!doctype html><html><body><script>
fetch("/api/at-limit", { method: "POST", body: "x".repeat(256 * 1024) });
</script></body></html>"#;
    let report = run_capture(
        "https://memory.example/",
        html,
        &CaptureConfig {
            capture_window_ms: 1_000,
            quiesce_ms: 100,
            max_intercepts: 64,
            stub_response_json: "{}".to_string(),
        },
        null_fetcher(),
    );

    let request = report
        .requests
        .iter()
        .find(|request| request.url.ends_with("/api/at-limit"))
        .expect("request should be captured");
    assert!(!request.body_omitted);
    assert_eq!(request.body.as_ref().map(Vec::len), Some(256 * 1024));
}
