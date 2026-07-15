use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{Query, Request};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use axum::Json;
use draco_types::{ExtractionResult, Status};
use serde::Deserialize;
use serde_json::{json, Value};

const MAX_LOG_ENTRIES: usize = 500;
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static LOGS: OnceLock<Mutex<VecDeque<Value>>> = OnceLock::new();

#[derive(Debug, Clone, Copy)]
pub(crate) struct RequestId(pub(crate) u64);

#[derive(Debug, Deserialize)]
pub(crate) struct LogsQuery {
    limit: Option<usize>,
    after: Option<u64>,
}

fn logs() -> &'static Mutex<VecDeque<Value>> {
    LOGS.get_or_init(|| Mutex::new(VecDeque::with_capacity(MAX_LOG_ENTRIES)))
}

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

fn timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn push(entry: Value) {
    let rendered = serde_json::to_string(&entry).unwrap_or_else(|_| "{}".into());
    eprintln!("draco serve: {rendered}");
    let mut entries = logs()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if entries.len() == MAX_LOG_ENTRIES {
        entries.pop_front();
    }
    entries.push_back(entry);
}

pub(crate) async fn access_log(mut request: Request, next: Next) -> Response {
    let id = next_id();
    request.extensions_mut().insert(RequestId(id));
    let method = request.method().to_string();
    let path = request.uri().path().to_owned();
    let started = Instant::now();
    let mut response = next.run(request).await;
    if let Ok(value) = HeaderValue::from_str(&id.to_string()) {
        response.headers_mut().insert("x-draco-request-id", value);
    }

    if path != "/health" && path != "/admin/logs" {
        push(json!({
            "id": id,
            "timestampMs": timestamp_ms(),
            "kind": "access",
            "method": method,
            "path": path,
            "status": response.status().as_u16(),
            "durationMs": started.elapsed().as_millis(),
        }));
    }
    response
}

pub(crate) fn record_scrape(
    request_id: RequestId,
    url: &str,
    result: &ExtractionResult,
    status: StatusCode,
    duration_ms: u128,
) {
    let target_host = url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned));
    let trace: Vec<Value> = result
        .trace
        .iter()
        .map(|step| {
            json!({
                "tier": step.tier,
                "action": step.action,
                "outcome": step.outcome,
                "elapsedMs": step.elapsed_ms,
                "detail": step.detail.as_deref().map(redact_credentials),
            })
        })
        .collect();
    push(json!({
        "id": request_id.0,
        "timestampMs": timestamp_ms(),
        "kind": "scrape",
        "method": "POST",
        "path": "/v1/scrape",
        "status": status.as_u16(),
        "durationMs": duration_ms,
        "targetHost": target_host,
        "outcome": result.status,
        "tier": result.source_tier,
        "error": (result.status != Status::Success)
            .then(|| redact_credentials(&super::error_summary(result))),
        "trace": trace,
    }));
}

pub(crate) async fn logs_handler(Query(query): Query<LogsQuery>) -> Json<Value> {
    let limit = query.limit.unwrap_or(200).clamp(1, MAX_LOG_ENTRIES);
    let after = query.after.unwrap_or(0);
    let entries = logs()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let selected: Vec<Value> = entries
        .iter()
        .rev()
        .filter(|entry| {
            entry
                .get("id")
                .and_then(Value::as_u64)
                .is_some_and(|id| id > after)
        })
        .take(limit)
        .cloned()
        .collect();
    Json(json!({ "success": true, "logs": selected }))
}

fn redact_credentials(input: &str) -> String {
    let mut redacted = input.to_owned();
    for scheme in ["http://", "https://", "socks5://", "socks5h://"] {
        let mut search_from = 0;
        while let Some(relative_start) = redacted[search_from..].find(scheme) {
            let start = search_from + relative_start + scheme.len();
            let tail = &redacted[start..];
            let authority_end = tail
                .find(|character: char| character.is_whitespace() || character == '/')
                .unwrap_or(tail.len());
            let authority = &tail[..authority_end];
            let Some(at) = authority.rfind('@') else {
                search_from = start + authority_end;
                continue;
            };
            redacted.replace_range(start..start + at + 1, "***@");
            search_from = start + 4;
        }
    }
    redacted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_credentials_are_redacted_without_hiding_host() {
        assert_eq!(
            redact_credentials("connect socks5h://user:p%40ss@proxy.example:1080 failed"),
            "connect socks5h://***@proxy.example:1080 failed",
        );
    }
}
