//! End-to-end Markdown-scrape test.
//!
//! Serves a static HTML fixture over a localhost ephemeral port and drives the
//! **real** `draco` binary through its default (Markdown) path:
//!
//! ```text
//!   draco extract → net.fetch (draco-net) → challenge check → static.markdown
//!     → clean Markdown of the main content, boilerplate dropped
//! ```
//!
//! It asserts the fast path returns non-empty Markdown with the fixture's
//! headings, a metadata envelope with the expected keys (via `--json`), and that
//! it never escalates to a tier (no `tier1.*`/`runtime.*` trace steps) — and is
//! quick.
//!
//! Why `#[ignore]` (matching `e2e_tier2.rs`): the Tier 0 fetch goes through
//! `draco-net` (wreq), which needs the BoringSSL runtime — not available in
//! every CI sandbox. Run on a suitable host with:
//!
//!     cargo test -p draco-cli --test e2e_markdown -- --ignored --nocapture

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::thread;
use std::time::Instant;

use serde_json::Value;

fn draco_bin() -> &'static str {
    env!("CARGO_BIN_EXE_draco")
}

/// The article fixture: site chrome (nav/header/footer) around an `<article>`
/// exercising headings, links (relative + absolute), a list, a code block, and a
/// GFM table.
const ARTICLE_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
  <title>Fixture Article</title>
  <meta name="description" content="A localhost fixture served to draco.">
  <meta property="og:title" content="Fixture Article (OG)">
  <link rel="canonical" href="/articles/fixture">
  <link rel="icon" href="/favicon.ico">
</head>
<body>
  <nav><a href="/">Home</a> <a href="/blog">Blog</a></nav>
  <header><p>Header boilerplate that should be dropped.</p></header>
  <main>
    <article>
      <h1>Fixture Heading</h1>
      <p>An intro paragraph with a <a href="/relative/link">relative link</a>
         and an <a href="https://external.example/x">external link</a>, with enough
         prose that readability treats this region as the page's main content.</p>
      <h2>Details Section</h2>
      <ul><li>First point</li><li>Second point</li></ul>
      <pre><code>let answer = 42;</code></pre>
      <table>
        <thead><tr><th>Key</th><th>Value</th></tr></thead>
        <tbody><tr><td>alpha</td><td>1</td></tr><tr><td>beta</td><td>2</td></tr></tbody>
      </table>
      <p>A closing paragraph with still more words to keep the article comfortably
         above readability's length threshold, padding padding padding padding.</p>
    </article>
  </main>
  <footer><p>Footer boilerplate copyright 2026.</p></footer>
</body>
</html>"##;

/// A one-request-at-a-time HTTP/1.1 fixture on an ephemeral port: serves the
/// article HTML for any page path and an allow-all `/robots.txt`. Runs on a
/// background thread until the listener is dropped; returns its base URL.
fn spawn_fixture() -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    let addr = listener.local_addr().expect("addr");
    let base = format!("http://{addr}");

    let handle = thread::spawn(move || {
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

    let (content_type, body): (&str, String) = if path == "/robots.txt" {
        ("text/plain", "User-agent: *\nDisallow:\n".to_string())
    } else {
        ("text/html; charset=utf-8", ARTICLE_HTML.to_string())
    };

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

#[test]
#[ignore = "e2e: Tier 0 fetch goes through draco-net's BoringSSL runtime; run on a suitable host with --ignored"]
fn markdown_scrape_end_to_end_returns_clean_markdown() {
    let (base, _fixture) = spawn_fixture();

    // ---- Default (markdown) format: raw Markdown on stdout ----
    let started = Instant::now();
    let output = Command::new(draco_bin())
        .args([
            "extract",
            &format!("{base}/articles/fixture"),
            "--ignore-robots",
        ])
        .output()
        .expect("run draco binary");
    let elapsed = started.elapsed();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Success → exit 0 (spec §12).
    assert_eq!(
        output.status.code(),
        Some(0),
        "expected success exit; stdout={stdout}\nstderr={stderr}"
    );

    // Raw Markdown (not a JSON envelope) with the fixture's headings.
    assert!(
        !stdout.trim_start().starts_with('{'),
        "markdown format must print raw Markdown, not JSON: {stdout}"
    );
    assert!(!stdout.trim().is_empty(), "markdown must be non-empty");
    assert!(
        stdout.contains("Fixture Heading"),
        "H1 text missing from markdown:\n{stdout}"
    );
    assert!(
        stdout.contains("Details Section"),
        "H2 text missing from markdown:\n{stdout}"
    );
    // Relative link absolutized against the fetched URL.
    assert!(
        stdout.contains(&format!("({base}/relative/link)")),
        "relative link not absolutized:\n{stdout}"
    );
    // External link preserved, list, code, and GFM table survive.
    assert!(stdout.contains("(https://external.example/x)"), "{stdout}");
    assert!(stdout.contains("First point"), "list missing:\n{stdout}");
    assert!(stdout.contains("```"), "code fence missing:\n{stdout}");
    assert!(stdout.contains("| Key"), "table header missing:\n{stdout}");
    // Boilerplate dropped.
    assert!(
        !stdout.contains("Footer boilerplate"),
        "footer should be stripped:\n{stdout}"
    );

    // Fast: the static-only path should be well under a second on localhost.
    assert!(
        elapsed.as_secs() < 10,
        "markdown scrape took too long: {elapsed:?}"
    );

    // ---- `--json`: the full envelope, with the metadata keys ----
    let (base2, _fixture2) = spawn_fixture();
    let env_out = Command::new(draco_bin())
        .args([
            "extract",
            &format!("{base2}/articles/fixture"),
            "--ignore-robots",
            "--json",
        ])
        .output()
        .expect("run draco binary");
    assert_eq!(env_out.status.code(), Some(0));
    let json: Value =
        serde_json::from_str(String::from_utf8_lossy(&env_out.stdout).trim()).expect("stdout JSON");

    assert_eq!(json["status"], "success");
    assert_eq!(json["source_tier"], "static");
    assert!(json["markdown"]
        .as_str()
        .unwrap()
        .contains("Fixture Heading"));

    let meta = &json["metadata"];
    assert_eq!(meta["title"], "Fixture Article");
    assert_eq!(meta["description"], "A localhost fixture served to draco.");
    assert_eq!(meta["language"], "en");
    assert_eq!(meta["og:title"], "Fixture Article (OG)");
    assert_eq!(meta["canonical"], format!("{base2}/articles/fixture"));
    assert_eq!(meta["favicon"], format!("{base2}/favicon.ico"));
    assert_eq!(meta["statusCode"], 200);
    assert_eq!(meta["contentType"], "text/html; charset=utf-8");
    assert_eq!(meta["sourceURL"], format!("{base2}/articles/fixture"));

    // The fast path must NOT escalate to Tier 1/Tier 2.
    let actions: Vec<&str> = json["trace"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["action"].as_str().unwrap())
        .collect();
    assert_eq!(
        actions,
        vec!["net.fetch", "static.markdown"],
        "markdown fast path should only fetch + scrape"
    );
}
