use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;

use crate::config::Config;
use crate::discovery::{RenderMode, ResolvedHostConfig};
use crate::slots::SlotRegistry;
use crate::wire::{ErrorResponse, MintRequest};

#[derive(Debug)]
pub struct AppState {
    host: ResolvedHostConfig,
    slots: Arc<SlotRegistry>,
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
}

pub async fn serve(config: Config, host: ResolvedHostConfig) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|error| format!("bind {}: {error}", config.bind))?;
    let local = listener.local_addr().unwrap_or(config.bind);
    let app = router(Arc::new(AppState {
        host,
        slots: SlotRegistry::new(config.slots),
    }));
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
        },
        host_config_cached: state.host.cache_present,
        discovery_cache_hit: state.host.cache_hit,
    })
}

async fn mint(
    State(state): State<Arc<AppState>>,
    Json(request): Json<MintRequest>,
) -> (StatusCode, Json<ErrorResponse>) {
    let lease = match state.slots.try_acquire() {
        Some(lease) => lease,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new("browser tier saturated")),
            )
        }
    };

    // Exercise the real lifecycle now: a slot is reserved until this handler
    // returns, and Drop makes it available again. Later slices replace only the
    // body below with relay swap, leak probe, worker mint, and Double Tap.
    let _slot_id = lease.slot().id;
    let _render_mode = state.host.config.render_mode;
    let _future_job = (
        request.url,
        request.proxy,
        request.render_opts,
        request.wait_strategy,
    );

    (
        StatusCode::NOT_IMPLEMENTED,
        Json(ErrorResponse::new("browser tier not yet implemented")),
    )
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
        })
    }

    async fn json_body(response: axum::response::Response) -> Value {
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn health_reports_render_and_slot_state() {
        let response = router(test_state(2))
            .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["slots"], json!({"total": 2, "busy": 0, "free": 2}));
        assert!(matches!(body["renderMode"].as_str(), Some("gpu" | "swiftshader")));
        assert!(body["hostConfigCached"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn mint_returns_frozen_stub_failure() {
        let response = router(test_state(1))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mint")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"url":"https://example.com","proxy":"socks5h://proxy"})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        assert_eq!(json_body(response).await, json!({
            "success": false,
            "error": "browser tier not yet implemented"
        }));
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
                        json!({"url":"https://example.com","proxy":"socks5h://proxy"})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
