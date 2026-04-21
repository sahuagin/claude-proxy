use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderName, Request, Response, StatusCode},
    response::IntoResponse,
};
use rand::Rng;
use tokio::sync::RwLock;

use crate::config::{load_credentials, Config, FaultRule, OAuthCredential};

pub struct BackendState {
    pub credentials_file: String,
    pub token: RwLock<OAuthCredential>,
}

pub struct AppState {
    pub config: Config,
    pub backends: HashMap<String, Arc<BackendState>>,
    /// Fault rules keyed by backend name; RwLock so mgmt API can update at runtime.
    pub faults: RwLock<HashMap<String, FaultRule>>,
    pub client: reqwest::Client,
}

pub async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> impl IntoResponse {
    let (parts, body) = req.into_parts();

    let body_bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("failed to read request body: {e}");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");

    let forward_headers = strip_auth_headers(&parts.headers);

    for backend_name in &state.config.failover.order {
        let Some(backend_cfg) = state.config.backends.iter().find(|b| &b.name == backend_name)
        else {
            tracing::warn!("backend '{backend_name}' in failover.order not defined");
            continue;
        };
        let Some(backend) = state.backends.get(backend_name) else {
            continue;
        };

        // Fault injection — check before touching real backend
        {
            let faults = state.faults.read().await;
            if let Some(rule) = faults.get(backend_name) {
                if rand::rng().random::<f64>() < rule.rate {
                    tracing::warn!(%backend_name, status = rule.status, "fault injected");
                    if state.config.failover.triggers.contains(&rule.status) {
                        continue; // counts as a trigger, try next backend
                    }
                    return StatusCode::from_u16(rule.status)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
                        .into_response();
                }
            }
        }

        let token = {
            let cred = backend.token.read().await;
            if cred.is_expired() {
                drop(cred);
                tracing::info!(%backend_name, "token expired, reloading credentials");
                reload_token(backend).await
            } else {
                cred.access_token.clone()
            }
        };

        let url = format!(
            "{}{}",
            backend_cfg.base_url.trim_end_matches('/'),
            path_and_query
        );
        tracing::debug!(%backend_name, %url, "forwarding request");

        let builder = state
            .client
            .request(parts.method.clone(), &url)
            .headers(forward_headers.clone())
            .header("Authorization", format!("Bearer {token}"))
            .body(body_bytes.clone());

        match builder.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status == 401 {
                    tracing::warn!(%backend_name, "got 401, reloading credentials and retrying");
                    let fresh_token = reload_token(backend).await;
                    let retry = state
                        .client
                        .request(parts.method.clone(), &url)
                        .headers(forward_headers.clone())
                        .header("Authorization", format!("Bearer {fresh_token}"))
                        .body(body_bytes.clone())
                        .send()
                        .await;
                    match retry {
                        Ok(r) if r.status().as_u16() != 401 => {
                            let s = r.status().as_u16();
                            if state.config.failover.triggers.contains(&s) {
                                tracing::warn!(%backend_name, %s, "trigger after refresh, trying next");
                                continue;
                            }
                            tracing::info!(%backend_name, %s, "response (after token refresh)");
                            return stream_response(r).await;
                        }
                        _ => {
                            tracing::warn!(%backend_name, "still failing after token refresh, trying next");
                            continue;
                        }
                    }
                }
                if state.config.failover.triggers.contains(&status) {
                    tracing::warn!(%backend_name, %status, "trigger status, trying next backend");
                    continue;
                }
                tracing::info!(%backend_name, %status, "response");
                return stream_response(resp).await;
            }
            Err(e) => {
                tracing::warn!(%backend_name, "request error: {e}");
                continue;
            }
        }
    }

    tracing::error!("all backends exhausted");
    StatusCode::BAD_GATEWAY.into_response()
}

/// Re-read the credentials file and update the stored token. Returns the fresh access token.
async fn reload_token(backend: &BackendState) -> String {
    match load_credentials(&backend.credentials_file) {
        Ok(cred) => {
            let token = cred.access_token.clone();
            *backend.token.write().await = cred;
            token
        }
        Err(e) => {
            tracing::error!("failed to reload credentials from {}: {e}", backend.credentials_file);
            backend.token.read().await.access_token.clone()
        }
    }
}

fn strip_auth_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        match name.as_str().to_ascii_lowercase().as_str() {
            "authorization" | "x-api-key" | "host" | "transfer-encoding" => continue,
            _ => {
                out.insert(name.clone(), value.clone());
            }
        }
    }
    out
}

async fn stream_response(resp: reqwest::Response) -> axum::response::Response {
    let status =
        StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    let mut headers = HeaderMap::new();
    for (name, value) in resp.headers() {
        if name.as_str().eq_ignore_ascii_case("transfer-encoding") {
            continue;
        }
        if let Ok(n) = HeaderName::from_bytes(name.as_ref()) {
            headers.insert(n, value.clone());
        }
    }

    let body = Body::from_stream(resp.bytes_stream());
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}
