//! Integration tests for the shared connection pool + per-call cookie jar.
//!
//! These drive the public [`draco_net::fetch_target`] surface against a
//! localhost axum fixture, exercising the two guarantees the pooling rework
//! must uphold:
//!
//! 1. **Per-call cookie isolation** — a cookie set during one `fetch_target`
//!    call must not be visible to a later, unrelated call, even though both
//!    calls share the same pooled client.
//! 2. **Within-call cookie flow** — the per-call jar still carries cookies
//!    across a redirect chain inside a single call (the behavior the old
//!    per-call client provided).
//!
//! Both run over plain HTTP on loopback (no TLS/robots), so they are
//! sandbox-safe and deterministic.

use std::net::SocketAddr;

use axum::extract::Path;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use draco_net::{fetch_target, SessionOpts};

/// Start the fixture server on an ephemeral loopback port; return its base URL.
async fn spawn_fixture() -> String {
    let app = Router::new()
        // Sets a cookie and returns a plain body (no redirect).
        .route(
            "/set-cookie",
            get(|| async {
                ([(header::SET_COOKIE, "sid=SECRET; Path=/")], "cookie set").into_response()
            }),
        )
        // Echoes back whatever Cookie header the client sent (or "none").
        .route(
            "/echo-cookie",
            get(|headers: HeaderMap| async move {
                let cookie = headers
                    .get(header::COOKIE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("none")
                    .to_string();
                cookie
            }),
        )
        // Sets a cookie AND redirects to /echo-cookie: exercises the per-call
        // jar carrying the cookie across a redirect within one call.
        .route(
            "/set-and-redirect/{target}",
            get(|Path(target): Path<String>| async move {
                Response::builder()
                    .status(StatusCode::FOUND)
                    .header(header::SET_COOKIE, "sid=SECRET; Path=/")
                    .header(header::LOCATION, format!("/{target}"))
                    .body(axum::body::Body::empty())
                    .unwrap()
            }),
        )
        // Echoes back the custom `X-Custom` request header (or "none").
        .route(
            "/echo-header",
            get(|headers: HeaderMap| async move {
                headers
                    .get("x-custom")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("none")
                    .to_string()
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn opts() -> SessionOpts {
    SessionOpts {
        respect_robots: false,
        timeout_ms: 5_000,
        ..Default::default()
    }
}

/// Caller-supplied `SessionOpts::headers` must be sent on the outbound request.
#[tokio::test]
async fn custom_headers_are_sent_on_the_request() {
    let base = spawn_fixture().await;
    let opts = SessionOpts {
        respect_robots: false,
        timeout_ms: 5_000,
        headers: vec![("X-Custom".to_string(), "hello-draco".to_string())],
        ..Default::default()
    };
    let resp = fetch_target(&format!("{base}/echo-header"), &opts)
        .await
        .expect("echo-header fetch");
    let body = String::from_utf8_lossy(&resp.body);
    assert_eq!(
        body, "hello-draco",
        "custom X-Custom header should reach the server"
    );
}

/// A cookie set in one call must NOT leak into a later, unrelated call — even
/// though both calls share the pooled client. This is the core isolation
/// guarantee the per-call jar provides.
#[tokio::test]
async fn cookies_do_not_leak_between_calls() {
    let base = spawn_fixture().await;

    // Call 1: receive a Set-Cookie (into this call's jar, then discarded).
    let set = fetch_target(&format!("{base}/set-cookie"), &opts())
        .await
        .expect("set-cookie fetch");
    assert_eq!(set.meta.status, 200);

    // Call 2: a fresh call → fresh jar → must send NO cookie.
    let echo = fetch_target(&format!("{base}/echo-cookie"), &opts())
        .await
        .expect("echo-cookie fetch");
    let body = String::from_utf8_lossy(&echo.body);
    assert_eq!(
        body, "none",
        "cookie from a prior call leaked into an unrelated call: {body:?}"
    );
}

/// With an operation-scoped [`SharedCookieJar`] on the opts, a cookie set on one
/// call MUST be replayed on a later call in the same operation — the browser
/// behavior that lets Cloudflare's `__cf_bm` from the page fetch ride along on
/// every subresource request.
#[tokio::test]
async fn cookies_persist_across_calls_with_a_shared_jar() {
    use draco_net::SharedCookieJar;
    let base = spawn_fixture().await;
    let shared = SharedCookieJar::new();
    let job_opts = SessionOpts {
        respect_robots: false,
        timeout_ms: 5_000,
        cookie_jar: Some(shared.clone()),
        ..Default::default()
    };

    // Call 1: receive Set-Cookie into the shared jar.
    let set = fetch_target(&format!("{base}/set-cookie"), &job_opts)
        .await
        .expect("set-cookie fetch");
    assert_eq!(set.meta.status, 200);

    // Call 2: same jar → the cookie must be sent.
    let echo = fetch_target(&format!("{base}/echo-cookie"), &job_opts)
        .await
        .expect("echo-cookie fetch");
    let body = String::from_utf8_lossy(&echo.body);
    assert!(
        body.contains("sid=SECRET"),
        "shared jar did not persist the cookie across calls: {body:?}"
    );

    // A DIFFERENT operation (fresh jar) must still be isolated from this one.
    let other = SessionOpts {
        respect_robots: false,
        timeout_ms: 5_000,
        cookie_jar: Some(SharedCookieJar::new()),
        ..Default::default()
    };
    let echo2 = fetch_target(&format!("{base}/echo-cookie"), &other)
        .await
        .expect("echo-cookie fetch (other op)");
    assert_eq!(
        String::from_utf8_lossy(&echo2.body),
        "none",
        "cookie leaked across operations (separate jars must isolate)"
    );
}

/// Within a single call, the per-call jar must carry a cookie set by a redirect
/// hop through to the redirect target — the behavior the old per-call client
/// provided, preserved on the shared pool.
#[tokio::test]
async fn cookie_flows_across_redirect_within_one_call() {
    let base = spawn_fixture().await;

    let resp = fetch_target(&format!("{base}/set-and-redirect/echo-cookie"), &opts())
        .await
        .expect("redirect fetch");
    assert_eq!(resp.meta.status, 200, "should have followed the redirect");
    let body = String::from_utf8_lossy(&resp.body);
    assert!(
        body.contains("sid=SECRET"),
        "per-call jar did not carry the cookie across the redirect: {body:?}"
    );
}

/// Many sequential calls through the shared pooled client all succeed — a
/// smoke test that reusing one client across calls is functionally sound
/// (connection keep-alive/H2 reuse happens transparently underneath).
#[tokio::test]
async fn many_calls_reuse_the_pooled_client() {
    let base = spawn_fixture().await;
    for _ in 0..10 {
        let resp = fetch_target(&format!("{base}/echo-cookie"), &opts())
            .await
            .expect("pooled fetch");
        assert_eq!(resp.meta.status, 200);
    }
}
