//! End-to-end Tier 2 smoke test (bonus; `#[ignore]`d).
//!
//! Spawns the **real** `draco` binary against a localhost fixture with
//! `--no-jail`, exercising the whole Slice 4 path un-jailed:
//!
//! ```text
//!   draco extract → Tier 0 miss → Tier 2:
//!     spawn `draco __jail` (un-jailed fork) → Hydrate over fd-3 IPC →
//!     V8 isolate hydrates the SPA → intercepts `GET /api/data` →
//!     rank → replay it → JSON body → Success / runtime_interception
//! ```
//!
//! Why `#[ignore]`:
//! * It boots a real V8 isolate in the forked child.
//! * The Tier 0 fetch + Tier 2 replay go through `draco-net` (wreq), which needs
//!   the BoringSSL runtime — the same reason `draco-net`'s live test is ignored;
//!   it is not available in every CI sandbox.
//! * `--no-jail` still forks/execs the binary; the *jailed* path additionally
//!   needs kernel ≥ 5.13 + unprivileged userns (validated on bare metal).
//!
//! Run on a suitable host with:
//!     cargo test -p draco-cli --test e2e_tier2 -- --ignored --nocapture

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::thread;

use serde_json::Value;

fn draco_bin() -> &'static str {
    env!("CARGO_BIN_EXE_draco")
}

/// A one-request-at-a-time HTTP/1.1 fixture: serves an SPA page that fetches
/// `/api/data`, the JSON for `/api/data`, and an allow-all `/robots.txt`. Runs on
/// a background thread until the listener is dropped; returns its base URL.
fn spawn_fixture() -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    let addr = listener.local_addr().expect("addr");
    let base = format!("http://{addr}");

    let handle = thread::spawn(move || {
        // Serve a bounded number of connections then stop, so the thread cannot
        // hang the test run forever if the client behaves unexpectedly.
        for _ in 0..16 {
            match listener.accept() {
                Ok((stream, _)) => handle_conn(stream),
                Err(_) => break,
            }
        }
    });

    (base, handle)
}

fn handle_conn(mut stream: TcpStream) {
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");

    let (content_type, body): (&str, String) = if path.starts_with("/api/data") {
        (
            "application/json",
            r#"{"price":42,"title":"Widget","items":[1,2,3]}"#.to_string(),
        )
    } else if path == "/robots.txt" {
        ("text/plain", "User-agent: *\nDisallow:\n".to_string())
    } else {
        // The SPA page: an inline script that fetches the JSON endpoint.
        (
            "text/html",
            r#"<!doctype html><html><head><title>Fixture SPA</title></head>
<body><div id="app">loading…</div>
<script>
  fetch("/api/data", { headers: { "accept": "application/json" } })
    .then(r => r.json())
    .then(d => { window.__data = d; });
</script>
</body></html>"#
                .to_string(),
        )
    };

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

#[test]
#[ignore = "e2e: boots real V8 in an un-jailed forked child + needs draco-net's BoringSSL runtime; run on a suitable host with --ignored"]
fn tier2_unjailed_end_to_end_extracts_json() {
    let (base, _fixture) = spawn_fixture();

    let output = Command::new(draco_bin())
        .args([
            "extract",
            &format!("{base}/product/1"),
            "--no-jail",
            "--tier-max",
            "2",
            "--ignore-robots",
            // Keep the capture window snappy for the test.
            "--capture-window-ms",
            "1500",
        ])
        .output()
        .expect("run draco binary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("--- draco stderr ---\n{stderr}");

    // Success → exit 0 (spec §12).
    assert_eq!(
        output.status.code(),
        Some(0),
        "expected success exit; stdout={stdout}\nstderr={stderr}"
    );

    let json: Value = serde_json::from_str(stdout.trim()).expect("stdout is JSON");
    assert_eq!(json["status"], "success");
    assert_eq!(json["source_tier"], "runtime_interception");
    assert_eq!(json["data"]["price"], 42);
    assert_eq!(json["data"]["title"], "Widget");
}
