//! End-to-end proof that an interact session persists cookies across a
//! navigation — the browser-tab behaviour that makes multi-page / login flows
//! work, and the concrete payoff of the one operation-scoped cookie jar shared
//! by the initial fetch and every navigation refetch (`draco-core`'s
//! `open_interact_session` + `NetPageFetcher`).
//!
//! Self-contained: a loopback axum fixture (no external network) whose `/` sets
//! a session cookie and whose `/page2` reports whether it received it. Boots the
//! real V8 isolate through the public `draco_core::open_interact_session` driver,
//! so it exercises the actual `draco-net` cookie jar, not a mock. Tier-2- and
//! serve-gated (both are needed: the isolate for the session, axum for the
//! fixture) so a lean build simply omits it.

#![cfg(all(feature = "tier2", feature = "serve"))]

use axum::http::{header, HeaderMap};
use axum::response::Html;
use axum::routing::get;
use axum::Router;

/// Page 1 sets `sid`; page 2 echoes `authed:yes` iff the request carried it.
#[tokio::test]
async fn cookie_set_on_page_one_rides_to_page_two_across_navigate() {
    let app = Router::new()
        .route(
            "/",
            get(|| async {
                (
                    [(header::SET_COOKIE, "sid=secret; Path=/")],
                    Html(
                        "<!doctype html><html><head><title>Login</title></head>\
                         <body><div id=\"app\">page-one</div></body></html>",
                    ),
                )
            }),
        )
        .route(
            "/page2",
            get(|headers: HeaderMap| async move {
                let authed = headers
                    .get(header::COOKIE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|c| c.contains("sid=secret"));
                let marker = if authed { "authed:yes" } else { "authed:no" };
                Html(format!(
                    "<!doctype html><html><head><title>Account</title></head>\
                     <body><div id=\"app\">{marker}</div></body></html>"
                ))
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let base = format!("http://127.0.0.1:{port}");
    // robots is not respected for the fixture (no /robots.txt); it is irrelevant
    // to what we're testing and would only add a preflight.
    let config = draco_core::Config {
        respect_robots: false,
        ..draco_core::Config::default()
    };

    // Open on page one: the initial fetch stores `sid` in the session's jar.
    let session = draco_core::open_interact_session(&format!("{base}/"), &config)
        .await
        .expect("session opens on page one");

    // Navigate to page two: the SAME session jar must send `sid`, so the fixture
    // sees an authenticated request and renders `authed:yes`.
    let nav = session
        .navigate(format!("{base}/page2"))
        .await
        .expect("navigate delivered");
    assert!(nav.ok, "navigation should succeed: {:?}", nav.error);

    let html = session
        .serialize()
        .await
        .expect("serialize delivered")
        .expect("some rendered HTML");
    assert!(
        html.contains("authed:yes"),
        "the cookie set on page one must ride to page two across navigate; got: {}",
        html.chars().take(200).collect::<String>()
    );

    session.close().await.expect("close");
}
