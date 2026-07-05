//! Integration test: run the built `draco` binary end-to-end and confirm the
//! flags are wired through to `draco_core::extract` and the status→exit-code
//! mapping fires. `draco_core::extract` is a WS-C stub that returns
//! `Status::Error`, so a real invocation must exit 1 and still print a
//! well-formed `ExtractionResult` on stdout.

use std::process::Command;

use serde_json::Value;

/// Path to the freshly built `draco` binary, injected by Cargo for this test.
fn draco_bin() -> &'static str {
    env!("CARGO_BIN_EXE_draco")
}

#[test]
fn extract_wires_through_and_maps_stub_error_to_exit_1() {
    let output = Command::new(draco_bin())
        .args(["extract", "https://example.com/product/42"])
        .output()
        .expect("run draco binary");

    // The core stub yields Status::Error → exit code 1 (spec §12).
    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // stdout must carry a parseable ExtractionResult echoing the URL + status.
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let json: Value = serde_json::from_str(stdout.trim()).expect("stdout is JSON");
    assert_eq!(json["url"], "https://example.com/product/42");
    assert_eq!(json["status"], "error");
}

#[test]
fn extract_with_filter_still_runs_and_exits_1_on_stub_error() {
    // Passing --extract must not crash the pipeline even though the stub
    // produces no data to filter; the run still terminates with exit 1.
    let output = Command::new(draco_bin())
        .args([
            "extract",
            "https://example.com",
            "--extract",
            "$.products[*].price",
            "--pretty",
        ])
        .output()
        .expect("run draco binary");

    assert_eq!(output.status.code(), Some(1));

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    // --pretty ⇒ multi-line output.
    assert!(stdout.contains('\n'), "pretty output should be multi-line");
    let json: Value = serde_json::from_str(stdout.trim()).expect("stdout is JSON");
    assert_eq!(json["status"], "error");
}

#[test]
fn invalid_jsonpath_is_rejected_at_parse_but_binary_still_runs() {
    // The stub returns no data, so a bad path is only reachable in unit tests;
    // here we simply confirm the flag is accepted by the parser and the binary
    // exits cleanly with the stub error rather than a clap usage error (exit 2
    // from clap would collide with the `unsupported` status code otherwise).
    let output = Command::new(draco_bin())
        .args(["extract", "https://example.com", "--extract", "$.a"])
        .output()
        .expect("run draco binary");
    assert_eq!(output.status.code(), Some(1));
}
