//! End-to-end proof of selector-schema structured extraction: a real fetch
//! through `draco_core::extract` with `Config.extract_schema` set, against a
//! loopback axum fixture — covering the full path (fetch → content scrape →
//! extract hook → `ExtractionResult.extract`) rather than the pure extractor
//! (draco-static's unit tests already cover that).
//!
//! Serve-gated for the axum fixture only; extraction itself is
//! tier-independent (this page is static, no V8 involved).

#![cfg(feature = "serve")]

use axum::response::Html;
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};

const PAGE: &str = "<!doctype html><html><head><title>Deals</title></head><body>\
     <main>\
     <h1>Deals of the day</h1>\
     <div class=\"card\"><span class=\"name\">Widget</span>\
       <span class=\"price\">$9.99</span><a href=\"/buy/widget\">Buy</a></div>\
     <div class=\"card\"><span class=\"name\">Gadget</span>\
       <span class=\"price\">$19.99</span><a href=\"/buy/gadget\">Buy</a></div>\
     </main></body></html>";

async fn serve_fixture() -> String {
    let app = Router::new().route("/", get(|| async { Html(PAGE) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://127.0.0.1:{port}/")
}

#[tokio::test]
async fn schema_extraction_rides_the_extraction_result() {
    let url = serve_fixture().await;
    let config = draco_core::Config {
        respect_robots: false,
        extract_schema: Some(json!({
            "title": "h1",
            "items": { "selector": ".card", "all": true, "fields": {
                "name": ".name",
                "price": ".price",
                "url": { "selector": "a", "attr": "href" }
            } }
        })),
        ..draco_core::Config::default()
    };

    let result = draco_core::extract(&url, &config).await;
    let extract = result
        .extract
        .as_ref()
        .expect("extract_schema set => result.extract populated");

    assert_eq!(extract["title"], "Deals of the day");
    let items = extract["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["name"], "Widget");
    assert_eq!(items[0]["price"], "$9.99");
    assert_eq!(items[1]["name"], "Gadget");
    // URL attributes are absolutized against the (loopback) page URL.
    let widget_url = items[0]["url"].as_str().expect("absolutized href");
    assert!(
        widget_url.starts_with("http://127.0.0.1:") && widget_url.ends_with("/buy/widget"),
        "got: {widget_url}"
    );
}

#[tokio::test]
async fn invalid_selector_warns_but_does_not_fail_the_scrape() {
    let url = serve_fixture().await;
    let config = draco_core::Config {
        respect_robots: false,
        extract_schema: Some(json!({
            "title": "h1",
            "bad": ":::nope"
        })),
        ..draco_core::Config::default()
    };

    let result = draco_core::extract(&url, &config).await;
    let extract = result.extract.as_ref().expect("extract still populated");

    // The good field extracts; the bad one nulls out instead of erroring.
    assert_eq!(extract["title"], "Deals of the day");
    assert_eq!(extract["bad"], Value::Null);
    // The warning is surfaced through the trace (action "extract.warning"),
    // which the REST envelope exposes as `extractWarnings`.
    assert!(
        result
            .trace
            .iter()
            .any(|step| step.action == "extract.warning"),
        "expected an extract.warning trace step"
    );
}
