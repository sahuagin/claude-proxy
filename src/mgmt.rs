use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::config::{load_credentials, FaultRule};
use crate::proxy::{AppState, BackendAuth};

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/metrics", get(metrics))
        .route("/fault", post(set_fault))
        .route("/reload", post(reload))
        .with_state(state)
}

async fn status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    #[derive(Serialize)]
    struct BackendStatus {
        name: String,
        base_url: String,
        auth: &'static str,
        /// OAuth-only fields; `None` for api_key backends.
        #[serde(skip_serializing_if = "Option::is_none")]
        token_expires_at_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        token_expired: Option<bool>,
    }

    let mut backends = vec![];
    for b in &state.config.backends {
        if let Some(bs) = state.backends.get(&b.name) {
            let (auth, expires, expired) = match &bs.auth {
                BackendAuth::Oauth { token, .. } => {
                    let cred = token.read().await;
                    ("oauth", Some(cred.expires_at), Some(cred.is_expired()))
                }
                BackendAuth::ApiKey { .. } => ("api_key", None, None),
            };
            backends.push(BackendStatus {
                name: b.name.clone(),
                base_url: b.base_url.clone(),
                auth,
                token_expires_at_ms: expires,
                token_expired: expired,
            });
        }
    }

    let faults: Vec<_> = state.faults.read().await.values().cloned().collect();

    #[derive(Serialize)]
    struct StatusResponse {
        backends: Vec<BackendStatus>,
        failover_order: Vec<String>,
        active_faults: Vec<FaultRule>,
    }

    Json(StatusResponse {
        backends,
        failover_order: state.config.failover.order.clone(),
        active_faults: faults,
    })
}

async fn metrics(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.metrics.snapshot())
}

#[derive(Deserialize)]
struct FaultRequest {
    backend: String,
    /// Omit or set to true to clear the fault for this backend
    #[serde(default)]
    clear: bool,
    status: Option<u16>,
    rate: Option<f64>,
}

async fn set_fault(
    State(state): State<Arc<AppState>>,
    Json(req): Json<FaultRequest>,
) -> impl IntoResponse {
    let mut faults = state.faults.write().await;
    if req.clear {
        faults.remove(&req.backend);
        tracing::info!(backend = %req.backend, "fault cleared via mgmt API");
        return (StatusCode::OK, Json(serde_json::json!({"cleared": req.backend})));
    }
    let status = match req.status {
        Some(s) => s,
        None => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "status required"}))),
    };
    let rate = req.rate.unwrap_or(1.0).clamp(0.0, 1.0);
    faults.insert(req.backend.clone(), FaultRule {
        backend: req.backend.clone(),
        status,
        rate,
    });
    tracing::warn!(backend = %req.backend, %status, %rate, "fault set via mgmt API");
    (StatusCode::OK, Json(serde_json::json!({"set": {"backend": req.backend, "status": status, "rate": rate}})))
}

#[derive(Deserialize)]
struct ReloadRequest {
    backend: String,
}

async fn reload(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ReloadRequest>,
) -> impl IntoResponse {
    let Some(backend) = state.backends.get(&req.backend) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("backend '{}' not found", req.backend)})),
        );
    };
    match &backend.auth {
        BackendAuth::Oauth {
            credentials_file,
            token,
        } => match load_credentials(credentials_file) {
            Ok(cred) => {
                let expires_at = cred.expires_at;
                *token.write().await = cred;
                tracing::info!(backend = %req.backend, "OAuth credentials reloaded via mgmt API");
                (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "reloaded": req.backend,
                        "auth": "oauth",
                        "expires_at_ms": expires_at,
                    })),
                )
            }
            Err(e) => {
                tracing::error!(backend = %req.backend, "OAuth reload failed: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e.to_string()})),
                )
            }
        },
        BackendAuth::ApiKey { key } => match state.config.resolve_backend_key(&req.backend) {
            Ok(new_key) => {
                *key.write().await = new_key.trim().to_string();
                tracing::info!(backend = %req.backend, "api_key reloaded via mgmt API");
                (
                    StatusCode::OK,
                    Json(serde_json::json!({"reloaded": req.backend, "auth": "api_key"})),
                )
            }
            Err(e) => {
                tracing::error!(backend = %req.backend, "api_key reload failed: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e.to_string()})),
                )
            }
        },
    }
}
