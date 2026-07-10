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

use std::rc::Rc;
use std::time::Duration;

use common::null_fetcher;
use draco_runtime::session::{Session, SessionConfig};
use draco_runtime::{ApiFetcher, CaptureConfig, ScriptFetcher};

/// Observe-mode fetchers (no live data): the null script fetcher + no
/// `ApiFetcher`. Spelling the return type explicitly lets `None` infer, and a fn
/// item is `Send`, so it coerces straight into the `FetcherFactory`.
fn observe_fetchers() -> (Rc<dyn ScriptFetcher>, Option<Rc<dyn ApiFetcher>>) {
    (null_fetcher(), None)
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
            true,
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
            false,
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
