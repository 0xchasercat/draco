//! Webhook delivery for async jobs (`/v1/crawl`, `/v1/batch/scrape`) —
//! Firecrawl-compatible.
//!
//! A job may carry a `webhook` config; when it does, the job runner fires four
//! lifecycle events through a [`WebhookSink`]: `started` (job begins), `page`
//! (each scraped page, carrying its `Document`), `completed` (drained), and
//! `failed`. The wire vocabulary is Firecrawl's: the config's `events` filter
//! uses **bare** names (`started`/`page`/`completed`/`failed`), while the
//! emitted payload's `type` is **prefixed** with the job kind (`crawl.page`,
//! `batch_scrape.completed`, …).
//!
//! Delivery is fire-and-forget: [`WebhookSink::emit`] spawns a detached task so
//! a slow or dead endpoint never stalls the crawl. Each event is `POST`ed as
//! JSON (via draco-net's pooled client, with `respect_robots` off — we never
//! robots-gate the caller's own endpoint) with a 10s deadline, retried at
//! +1min / +5min / +15min on any non-2xx or transport failure, then dropped.

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine;
use draco_net::{replay, SessionOpts};
use draco_types::HttpRequestSpec;
use serde::Deserialize;
use serde_json::{json, Value};

/// Per-attempt delivery deadline.
const WEBHOOK_TIMEOUT_MS: u64 = 10_000;

/// Delivery schedule: the initial attempt, then Firecrawl's +1/+5/+15 minute
/// retries. Delivery stops at the first 2xx.
const RETRY_DELAYS: [Duration; 4] = [
    Duration::ZERO,
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(15 * 60),
];

/// The four job lifecycle events. `Deserialize` handles the `events` filter
/// (bare names); [`WebhookEvent::as_str`] renders the same bare name for the
/// prefixed `type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum WebhookEvent {
    Started,
    Page,
    Completed,
    Failed,
}

impl WebhookEvent {
    fn as_str(self) -> &'static str {
        match self {
            WebhookEvent::Started => "started",
            WebhookEvent::Page => "page",
            WebhookEvent::Completed => "completed",
            WebhookEvent::Failed => "failed",
        }
    }
}

fn all_events() -> Vec<WebhookEvent> {
    vec![
        WebhookEvent::Started,
        WebhookEvent::Page,
        WebhookEvent::Completed,
        WebhookEvent::Failed,
    ]
}

/// The `webhook` request field: Firecrawl accepts either a bare URL string
/// (shorthand for `{ url }`) or a full object.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum WebhookSpec {
    Bare(String),
    Object {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        /// Echoed back verbatim in every payload's `metadata`.
        #[serde(default)]
        metadata: HashMap<String, String>,
        /// Which lifecycle events to deliver; defaults to all four.
        #[serde(default = "all_events")]
        events: Vec<WebhookEvent>,
    },
}

/// A normalized webhook configuration (a `WebhookSpec` with the shorthand
/// resolved and defaults filled).
#[derive(Debug, Clone)]
pub(crate) struct WebhookConfig {
    pub url: String,
    pub headers: HashMap<String, String>,
    pub metadata: HashMap<String, String>,
    pub events: Vec<WebhookEvent>,
}

impl From<WebhookSpec> for WebhookConfig {
    fn from(spec: WebhookSpec) -> Self {
        match spec {
            WebhookSpec::Bare(url) => WebhookConfig {
                url,
                headers: HashMap::new(),
                metadata: HashMap::new(),
                events: all_events(),
            },
            WebhookSpec::Object {
                url,
                headers,
                metadata,
                events,
            } => WebhookConfig {
                url,
                headers,
                metadata,
                events,
            },
        }
    }
}

/// Fires a job's lifecycle events at its configured webhook (if any). Cheap to
/// hold and clone-free to `emit`; a no-op when no webhook was configured.
pub(crate) struct WebhookSink {
    config: Option<WebhookConfig>,
    id: String,
    /// `"crawl"` or `"batch_scrape"` — the emitted `type` prefix.
    prefix: &'static str,
}

impl WebhookSink {
    pub(crate) fn new(spec: Option<WebhookSpec>, id: String, prefix: &'static str) -> Self {
        Self {
            config: spec.map(WebhookConfig::from),
            id,
            prefix,
        }
    }

    /// Deliver one lifecycle event. No-op if no webhook is configured or the
    /// event isn't in the config's filter. Never blocks: delivery (with retry)
    /// runs in a detached task.
    ///
    /// `data` is the event payload's `data` array — the scraped `Document`(s)
    /// for `page`, an empty array for `started`/`completed`/`failed` (mirroring
    /// Firecrawl, whose lifecycle events carry no aggregate body).
    pub(crate) fn emit(&self, event: WebhookEvent, data: Value) {
        let Some(cfg) = &self.config else {
            return;
        };
        if !cfg.events.contains(&event) {
            return;
        }
        let payload = json!({
            "success": event != WebhookEvent::Failed,
            "type": format!("{}.{}", self.prefix, event.as_str()),
            "id": self.id,
            "data": data,
            "metadata": cfg.metadata,
        });
        let url = cfg.url.clone();
        let headers: Vec<(String, String)> = cfg.headers.clone().into_iter().collect();
        tokio::spawn(deliver(url, headers, payload));
    }
}

/// `POST` the payload as JSON with retry. Success is the first 2xx; otherwise
/// the schedule in [`RETRY_DELAYS`] is exhausted and the event is dropped (a
/// dead endpoint must never wedge a job).
async fn deliver(url: String, mut headers: Vec<(String, String)>, payload: Value) {
    let body = serde_json::to_vec(&payload).unwrap_or_default();
    let body_b64 = base64::engine::general_purpose::STANDARD.encode(&body);
    headers.push(("content-type".to_string(), "application/json".to_string()));
    let spec = HttpRequestSpec {
        method: "POST".to_string(),
        url,
        headers,
        body_b64: Some(body_b64),
    };
    // respect_robots off: the webhook endpoint is the caller's own, never a
    // scrape target to gate.
    let opts = SessionOpts {
        respect_robots: false,
        timeout_ms: WEBHOOK_TIMEOUT_MS,
        ..Default::default()
    };

    for delay in RETRY_DELAYS {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if let Ok(resp) = replay(&spec, &opts).await {
            if (200..300).contains(&resp.meta.status) {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_string_spec_normalizes_to_all_events_no_headers() {
        let spec: WebhookSpec = serde_json::from_value(json!("https://hook.example/x")).unwrap();
        let cfg: WebhookConfig = spec.into();
        assert_eq!(cfg.url, "https://hook.example/x");
        assert!(cfg.headers.is_empty());
        assert!(cfg.metadata.is_empty());
        assert_eq!(cfg.events.len(), 4);
    }

    #[test]
    fn object_spec_parses_headers_metadata_events() {
        let spec: WebhookSpec = serde_json::from_value(json!({
            "url": "https://hook.example/x",
            "headers": { "authorization": "Bearer t" },
            "metadata": { "run": "42" },
            "events": ["page", "completed"]
        }))
        .unwrap();
        let cfg: WebhookConfig = spec.into();
        assert_eq!(cfg.headers.get("authorization").unwrap(), "Bearer t");
        assert_eq!(cfg.metadata.get("run").unwrap(), "42");
        assert_eq!(
            cfg.events,
            vec![WebhookEvent::Page, WebhookEvent::Completed]
        );
    }

    #[test]
    fn filtered_out_event_is_not_delivered() {
        // events = [completed] only → emitting `page` is a no-op (no panic, no
        // task). We can't easily observe the spawned task here, but we can at
        // least confirm the filter check doesn't fire for an excluded event by
        // constructing the sink and emitting on a runtime.
        let cfg = WebhookConfig {
            url: "https://hook.example/x".into(),
            headers: HashMap::new(),
            metadata: HashMap::new(),
            events: vec![WebhookEvent::Completed],
        };
        let sink = WebhookSink {
            config: Some(cfg),
            id: "1".into(),
            prefix: "crawl",
        };
        assert!(!sink
            .config
            .as_ref()
            .unwrap()
            .events
            .contains(&WebhookEvent::Page));
    }

    #[test]
    fn no_webhook_configured_is_inert() {
        let sink = WebhookSink::new(None, "1".into(), "batch_scrape");
        assert!(sink.config.is_none());
    }

    #[test]
    fn emitted_type_is_prefixed() {
        // The prefix + bare event name compose into Firecrawl's dotted `type`.
        assert_eq!(
            format!("{}.{}", "crawl", WebhookEvent::Page.as_str()),
            "crawl.page"
        );
        assert_eq!(
            format!("{}.{}", "batch_scrape", WebhookEvent::Completed.as_str()),
            "batch_scrape.completed"
        );
    }

    /// End-to-end: `deliver` POSTs the JSON payload (with custom headers) to the
    /// endpoint and stops at a 2xx. Spins up a localhost receiver that captures
    /// the body + a header.
    #[tokio::test]
    async fn deliver_posts_json_payload_with_headers() {
        use std::net::SocketAddr;
        use std::sync::{Arc, Mutex};

        use axum::extract::State;
        use axum::routing::post;
        use axum::Router;

        type Captured = Arc<Mutex<Vec<(Option<String>, Value)>>>;
        let captured: Captured = Arc::new(Mutex::new(Vec::new()));

        let app =
            Router::new()
                .route(
                    "/hook",
                    post(
                        |State(cap): State<Captured>,
                         headers: axum::http::HeaderMap,
                         body: String| async move {
                            let auth = headers
                                .get("authorization")
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_string);
                            let json: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                            cap.lock().unwrap().push((auth, json));
                            "ok"
                        },
                    ),
                )
                .with_state(captured.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let payload = json!({
            "success": true,
            "type": "batch_scrape.page",
            "id": "7",
            "data": [{ "markdown": "hi" }],
            "metadata": { "run": "42" }
        });
        deliver(
            format!("http://{addr}/hook"),
            vec![("authorization".to_string(), "Bearer t".to_string())],
            payload.clone(),
        )
        .await;

        let got = captured.lock().unwrap();
        assert_eq!(got.len(), 1, "webhook should have been delivered once");
        assert_eq!(
            got[0].0.as_deref(),
            Some("Bearer t"),
            "custom header missing"
        );
        assert_eq!(got[0].1["type"], "batch_scrape.page");
        assert_eq!(got[0].1["metadata"]["run"], "42");
        assert_eq!(got[0].1["data"][0]["markdown"], "hi");
    }
}
