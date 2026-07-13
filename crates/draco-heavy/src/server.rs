use std::net::IpAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{json, Value};

use crate::config::Config;
use crate::discovery::{RenderMode, ResolvedHostConfig};
use crate::local::{mint_local_with, LocalMintConfig};
use crate::pipe::browser::NamespaceBrowserDriver;
use crate::pipe::PipeConfig;
use crate::slots::SlotRegistry;
use crate::wire::{ErrorResponse, MintRequest, MintSuccess};

#[derive(Debug)]
pub struct AppState {
    host: ResolvedHostConfig,
    slots: Arc<SlotRegistry>,
    pipe: Arc<PipeConfig>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    render_mode: RenderMode,
    slots: HealthSlots,
    host_config_cached: bool,
    discovery_cache_hit: bool,
}

#[derive(Debug, Serialize)]
struct HealthSlots {
    total: usize,
    busy: usize,
    free: usize,
    quarantined: usize,
}

pub async fn serve(config: Config, host: ResolvedHostConfig) -> Result<(), String> {
    let pipe = Arc::new(config.pipe_config()?);
    let slots = SlotRegistry::provision(config.slots, &pipe).await?;
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|error| format!("bind {}: {error}", config.bind))?;
    let local = listener.local_addr().unwrap_or(config.bind);
    let app = router(Arc::new(AppState { host, slots, pipe }));
    eprintln!("draco-heavy: listening on http://{local}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|error| format!("server error: {error}"))
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/mint", post(mint))
        .with_state(state)
}

async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    let counts = state.slots.counts();
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        render_mode: state.host.config.render_mode,
        slots: HealthSlots {
            total: counts.total,
            busy: counts.busy,
            free: counts.free,
            quarantined: counts.quarantined,
        },
        host_config_cached: state.host.cache_present,
        discovery_cache_hit: state.host.cache_hit,
    })
}

async fn mint(
    State(state): State<Arc<AppState>>,
    Json(request): Json<MintRequest>,
) -> (StatusCode, Json<Value>) {
    let lease = match state.slots.try_acquire() {
        Some(lease) => lease,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(error_value("browser tier saturated")),
            )
        }
    };
    let Some(pipe) = lease.slot().pipe.as_ref() else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(error_value("pipe slot is not provisioned")),
        );
    };
    let expected_exit_ip = match expected_exit_ip(&request) {
        Ok(ip) => ip,
        Err(error) => return (StatusCode::BAD_REQUEST, Json(error_value(&error))),
    };
    let decision = match pipe
        .assign_box(&request.proxy, expected_exit_ip, &state.pipe)
        .await
    {
        Ok(decision) => decision,
        Err(error) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(error_value(&format!("pipe leak probe failed: {error}"))),
            )
        }
    };

    let driver = NamespaceBrowserDriver {
        namespace: pipe.namespace_name().unwrap_or_default().to_string(),
        quic_enabled: decision.quic_enabled,
    };
    let result = match mint_local_with(
        &driver,
        &state.host.config,
        &LocalMintConfig::default(),
        &request.url,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            return (
                StatusCode::NOT_IMPLEMENTED,
                Json(error_value(&format!("browser worker seam: {error}"))),
            )
        }
    };

    let response = MintSuccess {
        success: true,
        final_url: result.url,
        cookies: std::collections::HashMap::new(),
        html: result.html.unwrap_or_default(),
        markdown: result.markdown.unwrap_or_default(),
        render_mode: state.host.config.render_mode,
        ms: result.timing.total_ms,
    };
    (
        StatusCode::OK,
        Json(serde_json::to_value(response).unwrap_or_else(|_| {
            json!({
                "success": false,
                "error": "serialize mint response"
            })
        })),
    )
}

fn expected_exit_ip(request: &MintRequest) -> Result<Option<IpAddr>, String> {
    request
        .render_opts
        .as_ref()
        .and_then(|options| options.get("expectedExitIp"))
        .and_then(Value::as_str)
        .map(|value| {
            value
                .parse()
                .map_err(|error| format!("invalid renderOpts.expectedExitIp: {error}"))
        })
        .transpose()
}

fn error_value(error: &str) -> Value {
    serde_json::to_value(ErrorResponse::new(error))
        .unwrap_or_else(|_| json!({ "success": false, "error": "serialize error response" }))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use serde_json::{json, Value};
    use std::time::Duration;
    use tower::ServiceExt;

    fn test_state(slots: usize) -> Arc<AppState> {
        let temp = tempfile::tempdir().unwrap();
        let resolved = crate::discovery::resolve(
            &temp.path().join("host.json"),
            Duration::from_secs(60),
            true,
        );
        Arc::new(AppState {
            host: resolved,
            slots: SlotRegistry::new(slots),
            pipe: Arc::new(PipeConfig::default()),
        })
    }

    async fn json_body(response: axum::response::Response) -> Value {
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn health_reports_render_and_slot_state() {
        let response = router(test_state(2))
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(
            body["slots"],
            json!({"total": 2, "busy": 0, "free": 2, "quarantined": 0})
        );
        assert!(matches!(
            body["renderMode"].as_str(),
            Some("gpu" | "swiftshader")
        ));
        assert!(body["hostConfigCached"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn unprovisioned_test_slot_fails_closed() {
        let response = router(test_state(1))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mint")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"url":"https://example.com","proxy":"socks5h://proxy"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        assert_eq!(
            json_body(response).await,
            json!({
                "success": false,
                "error": "pipe slot is not provisioned"
            })
        );
    }

    #[tokio::test]
    async fn saturation_fails_fast_with_503() {
        let state = test_state(1);
        let _held = state.slots.try_acquire().unwrap();
        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mint")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"url":"https://example.com","proxy":"socks5h://proxy"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
