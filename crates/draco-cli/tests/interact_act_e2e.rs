//! End-to-end proof that `act` captures a fetch-less reactive render: a click
//! handler mounts a modal DIV with no network. Exercises the real V8 isolate
//! through `draco_core::open_interact_session` + `Session::act`, proving the
//! faithful-event dispatch fires the page's own listener AND the DOM-content-
//! settled pump captures the mount. tier2 + serve gated.
//!
//! The marker text is CONCATENATED in the page script (`'MODAL-' + 'OPENED'`)
//! so the literal never appears in the inline `<script>` source — `serialize()`
//! returns `outerHTML`, which includes script text, so a literal marker would
//! trip the "before" assertion without any click.
#![cfg(all(feature = "tier2", feature = "serve"))]

use axum::response::Html;
use axum::routing::get;
use axum::Router;
use draco_core::Action;

#[tokio::test]
async fn click_captures_a_fetchless_reactive_modal() {
    let app = Router::new().route(
        "/",
        get(|| async {
            Html(
                "<!doctype html><html><head><title>t</title></head><body>\
                 <button id=\"open\">Open</button>\
                 <script>\
                 document.getElementById('open').addEventListener('click', function () {\
                   var d = document.createElement('div');\
                   d.id = 'modal';\
                   d.textContent = 'MODAL-' + 'OPENED';\
                   document.body.appendChild(d);\
                 });\
                 </script></body></html>",
            )
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let base = format!("http://127.0.0.1:{port}/");
    let config = draco_core::Config {
        respect_robots: false,
        ..draco_core::Config::default()
    };

    let session = draco_core::open_interact_session(&base, &config)
        .await
        .expect("session opens");

    let before = session
        .serialize()
        .await
        .expect("serialize delivered")
        .expect("some html");
    assert!(
        !before.contains("MODAL-OPENED"),
        "modal should not exist before the click"
    );

    let report = session
        .act(vec![Action::Click {
            selector: "#open".to_string(),
        }])
        .await
        .expect("act delivered");
    assert!(report.ok, "act should succeed: {:?}", report.steps);

    let after = session
        .serialize()
        .await
        .expect("serialize delivered")
        .expect("some html");
    assert!(
        after.contains("MODAL-OPENED"),
        "the click must fire the page listener and the settle pump must capture \
         the fetch-less modal mount; got: {}",
        after.chars().take(300).collect::<String>()
    );

    session.close().await.expect("close");
}
