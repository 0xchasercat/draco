//! Integration tests for the interact session actor (v0.17.0 slice 2).
//!
//! These boot the real V8 isolate (restored from the build-time snapshot) and
//! drive it through the public [`Session`] API, proving the slice-2 risk gate:
//! the isolate stays alive across turns, `exec` runs JS in page global scope with
//! effects visible via `serialize`, the event loop keeps pumping *between*
//! commands (a timer armed in one turn fires before the next), and teardown is
//! clean. Offline: the [`ScriptFetcher`] is the shared `null_fetcher` double, and
//! the fixture pages carry only inline script, so no network is touched.

mod common;

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use common::null_fetcher;
use draco_runtime::session::{ExecOptions, PageFetcher, Session, SessionConfig, SessionFetchers};
use draco_runtime::CaptureConfig;

/// Observe-mode fetchers (no live data, no navigation): the null script fetcher.
/// A fn item is `Send`, so it coerces straight into the `FetcherFactory`.
fn observe_fetchers() -> SessionFetchers {
    SessionFetchers {
        scripts: null_fetcher(),
        api: None,
        page: None,
    }
}

/// Underlying type of the `PageFetcher::fetch_page` return (spelled without a
/// `futures` dev-dep, matching `tests/common`).
type BoxedPage<'a> = Pin<Box<dyn Future<Output = Option<(String, String)>> + 'a>>;

/// A page fetcher serving two fixed documents, standing in for the cookie-aware
/// `draco-net` navigator. Proves `navigate` swaps the loaded page.
struct TwoPages;

impl PageFetcher for TwoPages {
    fn fetch_page<'a>(&'a self, url: &'a str) -> BoxedPage<'a> {
        let doc = match url {
            "https://example.test/page2" => Some((
                url.to_string(),
                "<!doctype html><html><head><title>Page Two</title></head>\
                 <body><div id=\"app\">page-two-content</div></body></html>"
                    .to_string(),
            )),
            _ => None,
        };
        Box::pin(async move { doc })
    }
}

/// Fetchers with navigation enabled (the `TwoPages` stand-in).
fn navigating_fetchers() -> SessionFetchers {
    SessionFetchers {
        scripts: null_fetcher(),
        api: None,
        page: Some(Rc::new(TwoPages)),
    }
}

/// A snappy capture config so the initial hydrate settle and each `exec` settle
/// quiesce quickly in tests.
fn test_config(html: &str) -> SessionConfig {
    SessionConfig {
        url: "https://example.test/".to_string(),
        html: html.to_string(),
        capture: CaptureConfig {
            capture_window_ms: 1500,
            quiesce_ms: 50,
            max_intercepts: 64,
            stub_response_json: "{}".to_string(),
        },
    }
}

const SMOKE_HTML: &str = "<!doctype html><html><head><title>Interact Smoke</title></head>\
     <body><div id=\"app\">hi</div></body></html>";

/// Open → exec (a page-scope DOM mutation) → serialize → close. Proves the
/// isolate hydrates, holds, runs `exec` in page global scope with the effect
/// visible in the serialized DOM, and tears down cleanly.
#[tokio::test]
async fn open_exec_serialize_close() {
    let session = Session::open(test_config(SMOKE_HTML), Box::new(observe_fetchers))
        .await
        .expect("session opens");

    let report = session
        .exec(
            "document.getElementById('app').textContent = 'exec-ran';".to_string(),
            ExecOptions::default(),
        )
        .await
        .expect("exec delivered");
    assert!(report.ok, "exec should not throw: {:?}", report.error);

    let html = session
        .serialize()
        .await
        .expect("serialize delivered")
        .expect("some rendered HTML");
    assert!(
        html.contains("exec-ran"),
        "exec mutation must be visible in the serialized DOM"
    );
    assert!(
        html.contains("Interact Smoke"),
        "serialized DOM carries the original head"
    );

    session.close().await.expect("close");
}

/// The slice-2 core proof: a timer armed in turn 1 (without settling) fires
/// *between* commands — driven only by the actor's idle event-loop pump — so its
/// DOM mutation is present by the time we serialize, with no command in flight to
/// drive it. If the isolate were one-shot (or the loop didn't pump between turns)
/// the timer would never fire.
#[tokio::test]
async fn inter_turn_pump_fires_timer_between_commands() {
    let session = Session::open(test_config(SMOKE_HTML), Box::new(observe_fetchers))
        .await
        .expect("session opens");

    // Turn 1: arm a 5ms timer WITHOUT settling, so it has not fired on return.
    let t1 = session
        .exec(
            "globalThis.__x = 0; \
             setTimeout(() => { \
                 globalThis.__x = 42; \
                 document.getElementById('app').textContent = 'val:' + globalThis.__x; \
             }, 5);"
                .to_string(),
            ExecOptions {
                settle: false,
                ..ExecOptions::default()
            },
        )
        .await
        .expect("exec t1 delivered");
    assert!(t1.ok, "arming the timer should not throw: {:?}", t1.error);

    // No command in flight: the actor's idle pump alone must advance the isolate's
    // event loop enough for the 5ms timer to elapse and its callback to run.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let html = session
        .serialize()
        .await
        .expect("serialize delivered")
        .expect("some rendered HTML");
    assert!(
        html.contains("val:42"),
        "the between-turns timer must have fired and mutated the DOM; got: {}",
        html.chars().take(400).collect::<String>()
    );

    session.close().await.expect("close");
}

/// The devtools-console return value: a turn that `return`s a value gets it back,
/// serialized to JSON (slice 3).
#[tokio::test]
async fn exec_returns_serialized_value() {
    let session = Session::open(test_config(SMOKE_HTML), Box::new(observe_fetchers))
        .await
        .expect("session opens");

    let rep = session
        .exec("return 1 + 2;".to_string(), ExecOptions::default())
        .await
        .expect("exec delivered");
    assert!(rep.ok, "should not throw: {:?}", rep.error);
    assert_eq!(
        rep.result,
        Some(serde_json::json!(3)),
        "return value captured"
    );

    // A DOM node returned is *described*, never dropped.
    let rep = session
        .exec(
            "return document.getElementById('app');".to_string(),
            ExecOptions::default(),
        )
        .await
        .expect("exec delivered");
    let node = rep.result.expect("a described node");
    assert_eq!(
        node.get("__node").and_then(|v| v.as_str()),
        Some("div"),
        "node described with its tag: {node}"
    );
    assert_eq!(node.get("id").and_then(|v| v.as_str()), Some("app"));

    session.close().await.expect("close");
}

/// The size budget + the `full` lever: an over-budget value becomes a truncation
/// descriptor by default, and `full: true` returns it whole (slice 3, decision 6).
#[tokio::test]
async fn exec_result_truncation_and_full_override() {
    let session = Session::open(test_config(SMOKE_HTML), Box::new(observe_fetchers))
        .await
        .expect("session opens");

    // ~1000-char string, JSON ~1002 bytes; a 100-byte budget must truncate.
    let bounded = session
        .exec(
            "return 'x'.repeat(1000);".to_string(),
            ExecOptions {
                max_bytes: 100,
                ..ExecOptions::default()
            },
        )
        .await
        .expect("exec delivered");
    let d = bounded.result.expect("a truncation descriptor");
    assert_eq!(
        d.get("__truncated").and_then(|v| v.as_bool()),
        Some(true),
        "over-budget value is a truncation descriptor: {d}"
    );

    // The same value with `full` returns whole.
    let whole = session
        .exec(
            "return 'x'.repeat(1000);".to_string(),
            ExecOptions {
                full: true,
                ..ExecOptions::default()
            },
        )
        .await
        .expect("exec delivered");
    assert_eq!(
        whole.result.and_then(|v| v.as_str().map(str::len)),
        Some(1000),
        "full override returns the untruncated value"
    );

    session.close().await.expect("close");
}

/// Navigation (slice 4): `navigate` fetches the next document through the page
/// fetcher, tears down the current isolate, and re-hydrates in place — the new
/// page's content is present and the old page's is gone.
#[tokio::test]
async fn navigate_swaps_the_loaded_page() {
    let session = Session::open(test_config(SMOKE_HTML), Box::new(navigating_fetchers))
        .await
        .expect("session opens");

    // Page one is loaded.
    let before = session.serialize().await.expect("serialize").expect("html");
    assert!(before.contains("Interact Smoke"), "page one loaded");

    // Navigate to page two.
    let nav = session
        .navigate("https://example.test/page2".to_string())
        .await
        .expect("navigate delivered");
    assert!(nav.ok, "navigation succeeded: {:?}", nav.error);
    assert_eq!(nav.url.as_deref(), Some("https://example.test/page2"));

    // Page two is now loaded; page one is gone.
    let after = session.serialize().await.expect("serialize").expect("html");
    assert!(
        after.contains("page-two-content"),
        "page two rendered: {after:.160}"
    );
    assert!(
        !after.contains("Interact Smoke"),
        "the previous page was torn down"
    );

    session.close().await.expect("close");
}

/// Navigation is unavailable when no page fetcher was supplied (Observe-only
/// session): `navigate` reports failure and the session stays usable.
#[tokio::test]
async fn navigate_without_page_fetcher_reports_unavailable() {
    let session = Session::open(test_config(SMOKE_HTML), Box::new(observe_fetchers))
        .await
        .expect("session opens");

    let nav = session
        .navigate("https://example.test/page2".to_string())
        .await
        .expect("navigate delivered");
    assert!(!nav.ok, "navigation should be unavailable");
    assert!(nav.error.is_some(), "a reason is reported");

    // Session still works afterward.
    let html = session.serialize().await.expect("serialize").expect("html");
    assert!(
        html.contains("Interact Smoke"),
        "original page still loaded"
    );

    session.close().await.expect("close");
}
