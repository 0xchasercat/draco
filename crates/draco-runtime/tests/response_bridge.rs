use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use draco_runtime::{run_capture_render, ApiFetcher, ApiRequest, ApiResponse, CaptureConfig};

mod common;
use common::null_fetcher;

type ApiFuture<'a> = Pin<Box<dyn Future<Output = Option<ApiResponse>> + 'a>>;

struct FixedApiFetcher(HashMap<String, ApiResponse>);

impl ApiFetcher for FixedApiFetcher {
    fn fetch<'a>(&'a self, request: &'a ApiRequest) -> ApiFuture<'a> {
        let response = self.0.get(&request.url).cloned();
        Box::pin(async move { response })
    }
}

fn response(status: u16, headers: &[(&str, &str)], body: impl Into<Vec<u8>>) -> ApiResponse {
    ApiResponse {
        status,
        headers: headers
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect(),
        body: body.into(),
    }
}

fn capture(html: &str, routes: HashMap<String, ApiResponse>) -> draco_runtime::CaptureReport {
    run_capture_render(
        "https://bridge.example/",
        html,
        &CaptureConfig {
            capture_window_ms: 1_000,
            quiesce_ms: 30,
            max_intercepts: 32,
            stub_response_json: r#"{"stub":true}"#.to_string(),
        },
        null_fetcher(),
        Rc::new(FixedApiFetcher(routes)),
    )
}

fn rendered(report: &draco_runtime::CaptureReport) -> &str {
    report
        .rendered_html
        .as_deref()
        .expect("capture should serialize the hydrated DOM")
}

#[test]
fn fetch_live_responses_preserve_json_text_empty_error_and_lossy_utf8_bodies() {
    let html = r#"<html><body><output id="result"></output><script>
    (async function () {
      const json = await fetch("/json");
      const jsonBody = await json.json();
      const text = await (await fetch("/text")).text();
      const empty = await (await fetch("/empty")).text();
      const error = await fetch("/error");
      const errorBody = await error.text();
      const invalid = await (await fetch("/invalid-utf8")).text();
      const bytes = new Uint8Array(await (await fetch("/bytes")).arrayBuffer());
      document.getElementById("result").textContent = [
        json.status, json.ok, jsonBody.answer,
        text, empty.length,
        error.status, error.ok, errorBody,
        invalid === "f\ufffdo",
        Array.from(bytes).join(",")
      ].join("|");
    })().catch(function (error) {
      document.getElementById("result").textContent = "ERROR:" + error;
    });
    </script></body></html>"#;
    let routes = HashMap::from([
        (
            "https://bridge.example/json".to_string(),
            response(
                201,
                &[("content-type", "application/json")],
                br#"{"answer":42}"#.to_vec(),
            ),
        ),
        (
            "https://bridge.example/text".to_string(),
            response(200, &[("content-type", "text/plain")], "plain café"),
        ),
        (
            "https://bridge.example/empty".to_string(),
            response(204, &[], Vec::<u8>::new()),
        ),
        (
            "https://bridge.example/error".to_string(),
            response(422, &[("content-type", "text/plain")], "bad input"),
        ),
        (
            "https://bridge.example/invalid-utf8".to_string(),
            response(200, &[], vec![b'f', 0x80, b'o']),
        ),
        (
            "https://bridge.example/bytes".to_string(),
            response(200, &[], "AZ"),
        ),
    ]);

    let report = capture(html, routes);
    assert!(
        rendered(&report).contains("201|true|42|plain café|0|422|false|bad input|true|65,90"),
        "live Fetch bodies/statuses were not preserved; dom={:?}, logs={:?}",
        report.rendered_html,
        report.logs
    );
    assert!(
        report.logs.iter().any(|line| line.contains("/invalid-utf8")
            && line.contains("raw=3b")
            && line.contains("decoded_utf8=5b")),
        "fetch telemetry should compare raw and decoded UTF-8 sizes: {:?}",
        report.logs
    );
}

#[test]
fn fetch_body_is_one_shot_and_clone_has_independent_state() {
    let html = r#"<html><body><output id="result"></output><script>
    (async function () {
      const response = await fetch("/clone");
      const clone = response.clone();
      const before = !response.bodyUsed && !clone.bodyUsed;
      const original = await response.json();
      const originalUsed = response.bodyUsed && !clone.bodyUsed;
      const copied = await clone.text();
      const cloneUsed = clone.bodyUsed;
      let secondReadTypeError = false;
      let cloneAfterReadTypeError = false;
      try { await response.text(); } catch (error) { secondReadTypeError = error && error.name === "TypeError"; }
      try { response.clone(); } catch (error) { cloneAfterReadTypeError = error && error.name === "TypeError"; }
      document.getElementById("result").textContent = [
        before, original.value, originalUsed, copied, cloneUsed,
        secondReadTypeError, cloneAfterReadTypeError
      ].join("|");
    })().catch(function (error) {
      document.getElementById("result").textContent = "ERROR:" + error;
    });
    </script></body></html>"#;
    let routes = HashMap::from([(
        "https://bridge.example/clone".to_string(),
        response(
            200,
            &[("content-type", "application/json")],
            br#"{"value":"ok"}"#.to_vec(),
        ),
    )]);

    let report = capture(html, routes);
    assert!(
        rendered(&report).contains(r#"true|ok|true|{"value":"ok"}|true|true|true"#),
        "Fetch body/clone state was not browser-like; dom={:?}, logs={:?}",
        report.rendered_html,
        report.logs
    );
}

#[test]
fn fetch_body_methods_apply_to_the_receiver_and_reject_unbranded_receivers() {
    let html = r#"<html><body><output id="result"></output><script>
    (async function () {
      const a = await fetch("/receiver-a");
      const b = await fetch("/receiver-b");
      const borrowedText = await a.text.call(b);
      const borrowedTextUsedB = !a.bodyUsed && b.bodyUsed;

      const c = await fetch("/receiver-c");
      const d = await fetch("/receiver-d");
      const borrowedClone = c.clone.call(d);
      const borrowedCloneStartedClean = !c.bodyUsed && !d.bodyUsed && !borrowedClone.bodyUsed;
      const cloneText = await borrowedClone.text();
      const dText = await d.text();
      const borrowedCloneMetadata = borrowedClone.status === 202
        && borrowedClone.headers.get("X-Receiver") === "D"
        && borrowedClone.url.endsWith("/receiver-d");

      const incompatible = {};
      const methodNames = ["text", "json", "arrayBuffer", "blob"];
      const methodTypeErrors = [];
      for (const name of methodNames) {
        try {
          await a[name].call(incompatible);
          methodTypeErrors.push(false);
        } catch (error) {
          methodTypeErrors.push(error && error.name === "TypeError");
        }
      }
      let cloneTypeError = false;
      try { a.clone.call(incompatible); } catch (error) { cloneTypeError = error && error.name === "TypeError"; }
      let getterTypeError = false;
      try {
        Object.getOwnPropertyDescriptor(a, "bodyUsed").get.call(incompatible);
      } catch (error) {
        getterTypeError = error && error.name === "TypeError";
      }

      document.getElementById("result").textContent = [
        borrowedText, borrowedTextUsedB,
        borrowedCloneStartedClean, cloneText, dText, borrowedCloneMetadata,
        methodTypeErrors.every(Boolean), cloneTypeError, getterTypeError,
        !a.bodyUsed, !c.bodyUsed
      ].join("|");
    })().catch(function (error) {
      document.getElementById("result").textContent = "ERROR:" + error;
    });
    </script></body></html>"#;
    let routes = HashMap::from([
        (
            "https://bridge.example/receiver-a".to_string(),
            response(200, &[], "A"),
        ),
        (
            "https://bridge.example/receiver-b".to_string(),
            response(200, &[], "B"),
        ),
        (
            "https://bridge.example/receiver-c".to_string(),
            response(201, &[("X-Receiver", "C")], "C"),
        ),
        (
            "https://bridge.example/receiver-d".to_string(),
            response(202, &[("X-Receiver", "D")], "D"),
        ),
    ]);

    let report = capture(html, routes);
    assert!(
        rendered(&report).contains("B|true|true|D|D|true|true|true|true|true|true"),
        "borrowed Fetch methods did not brand/use their actual receiver; dom={:?}, logs={:?}",
        report.rendered_html,
        report.logs
    );
}

#[test]
fn fetch_headers_combine_duplicates_and_keep_first_seen_order() {
    let html = r#"<html><body><output id="result"></output><script>
    (async function () {
      const response = await fetch("/headers");
      const entries = [];
      response.headers.forEach(function (value, name) { entries.push(name + "=" + value); });
      document.getElementById("result").textContent = entries.join("|");
    })();
    </script></body></html>"#;
    let routes = HashMap::from([(
        "https://bridge.example/headers".to_string(),
        response(
            200,
            &[
                ("X-First", "one"),
                ("X-Dupe", "a"),
                ("x-dupe", "b"),
                ("X-Last", "three"),
            ],
            "{}",
        ),
    )]);

    let report = capture(html, routes);
    assert!(
        rendered(&report).contains("X-First=one|X-Dupe=a, b|X-Last=three"),
        "Fetch response header values/order changed; dom={:?}, logs={:?}",
        report.rendered_html,
        report.logs
    );
}

#[test]
fn xhr_live_responses_preserve_json_empty_arraybuffer_status_and_headers() {
    let html = r#"<html><body><output id="result"></output><script>
    function xhr(url, type) {
      return new Promise(function (resolve, reject) {
        const request = new XMLHttpRequest();
        request.open("GET", url);
        request.responseType = type;
        request.onload = function () { resolve(request); };
        request.onerror = reject;
        request.send();
      });
    }
    (async function () {
      const json = await xhr("/xhr-json", "json");
      const empty = await xhr("/xhr-empty", "text");
      const bytes = await xhr("/xhr-bytes", "arraybuffer");
      document.getElementById("result").textContent = [
        json.status, json.response.message,
        json.getResponseHeader("x-dupe"),
        json.getAllResponseHeaders() === "X-First: one\r\nX-Dupe: a\r\nx-dupe: b\r\nX-Last: three\r\n",
        empty.status, empty.responseText.length,
        bytes.response instanceof ArrayBuffer,
        Array.from(new Uint8Array(bytes.response)).join(",")
      ].join("|");
    })().catch(function (error) {
      document.getElementById("result").textContent = "ERROR:" + error;
    });
    </script></body></html>"#;
    let routes = HashMap::from([
        (
            "https://bridge.example/xhr-json".to_string(),
            response(
                418,
                &[
                    ("X-First", "one"),
                    ("X-Dupe", "a"),
                    ("x-dupe", "b"),
                    ("X-Last", "three"),
                ],
                br#"{"message":"teapot"}"#.to_vec(),
            ),
        ),
        (
            "https://bridge.example/xhr-empty".to_string(),
            response(204, &[], Vec::<u8>::new()),
        ),
        (
            "https://bridge.example/xhr-bytes".to_string(),
            response(200, &[], "AZ"),
        ),
    ]);

    let report = capture(html, routes);
    assert!(
        rendered(&report).contains("418|teapot|a, b|true|204|0|true|65,90"),
        "live XHR response fidelity changed; dom={:?}, logs={:?}",
        report.rendered_html,
        report.logs
    );
}

#[test]
fn happy_dom_internal_broker_preserves_live_stub_and_status_zero_responses() {
    let html = r#"<html><body><output id="result"></output><script>
    function internalFetch(url) {
      const internalWindow = document.defaultView;
      let owner = Object.getPrototypeOf(internalWindow);
      while (owner && !Object.prototype.hasOwnProperty.call(owner, "fetch")) {
        owner = Object.getPrototypeOf(owner);
      }
      if (!owner) throw new Error("happy-dom fetch prototype not found");
      return owner.fetch.call(internalWindow, url);
    }
    (async function () {
      const live = await internalFetch("/internal-live");
      const liveBody = await live.text();
      const zero = await internalFetch("/internal-zero");
      const zeroBody = await zero.text();
      const stub = await internalFetch("/internal-stub");
      const stubBody = await stub.text();
      const orderedHeaders = [];
      live.headers.forEach(function (value, name) { orderedHeaders.push(name + "=" + value); });
      document.getElementById("result").textContent = [
        live.status, liveBody,
        orderedHeaders.join(","),
        zero.status, zeroBody, zero.headers.get("X-Zero"),
        stub.status, stubBody, stub.headers.get("content-type")
      ].join("|");
    })().catch(function (error) {
      document.getElementById("result").textContent = "ERROR:" + error;
    });
    </script></body></html>"#;
    let routes = HashMap::from([
        (
            "https://bridge.example/internal-live".to_string(),
            response(
                418,
                &[
                    ("X-First", "one"),
                    ("X-Dupe", "a"),
                    ("x-dupe", "b"),
                    ("X-Last", "three"),
                ],
                "live-body",
            ),
        ),
        (
            "https://bridge.example/internal-zero".to_string(),
            response(0, &[("X-Zero", "kept")], "zero-body"),
        ),
    ]);

    let report = capture(html, routes);
    assert!(
        rendered(&report).contains(
            r#"418|live-body|X-First=one,X-Dupe=a, b,X-Last=three,Content-Type=text/plain;charset=UTF-8|0|zero-body|kept|200|{"stub":true}|application/json"#
        ),
        "happy-dom internal broker lost response fidelity; dom={:?}, logs={:?}",
        report.rendered_html,
        report.logs
    );
}
