use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, HeaderName, Request, Response, StatusCode},
    response::IntoResponse,
};
use rand::Rng;
use tokio::sync::RwLock;

use crate::config::{load_credentials, Config, FaultRule, OAuthCredential};
use crate::metrics::Metrics;

/// Anthropic OAuth beta flag required when presenting an OAuth bearer to api.anthropic.com.
/// Without this header, the upstream rejects with 401 even on a valid, in-expiry token.
const OAUTH_BETA: &str = "oauth-2025-04-20";

pub enum BackendAuth {
    Oauth {
        credentials_file: String,
        token: RwLock<OAuthCredential>,
    },
    ApiKey {
        key: RwLock<String>,
    },
}

pub struct BackendState {
    pub auth: BackendAuth,
}

pub struct AppState {
    pub config: Config,
    pub backends: HashMap<String, Arc<BackendState>>,
    /// Fault rules keyed by backend name; RwLock so mgmt API can update at runtime.
    pub faults: RwLock<HashMap<String, FaultRule>>,
    pub metrics: Arc<Metrics>,
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

        // Path-prefix allowlist. Skip backends whose upstream isn't qualified
        // to answer this path (e.g. don't offer /oauth/token to OpenRouter).
        let allowed = backend_cfg.effective_allowed_path_prefixes();
        if !allowed.is_empty() && !allowed.iter().any(|p| path_and_query.starts_with(p)) {
            tracing::debug!(%backend_name, path = %path_and_query, "path not in allowlist, skipping");
            continue;
        }

        state.metrics.inc_requests(backend_name);

        // Fault injection — check before touching real backend
        {
            let faults = state.faults.read().await;
            if let Some(rule) = faults.get(backend_name) {
                if rand::rng().random::<f64>() < rule.rate {
                    tracing::warn!(%backend_name, status = rule.status, "fault injected");
                    state.metrics.inc_faults();
                    state.metrics.set_last_status(backend_name, rule.status);
                    if state.config.failover.triggers.contains(&rule.status) {
                        state.metrics.inc_failovers();
                        continue;
                    }
                    return StatusCode::from_u16(rule.status)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
                        .into_response();
                }
            }
        }

        let url = format!(
            "{}{}",
            backend_cfg.base_url.trim_end_matches('/'),
            path_and_query
        );
        tracing::debug!(%backend_name, %url, "forwarding request");

        let forward_body = if backend_cfg.model_map.is_empty() {
            body_bytes.clone()
        } else {
            rewrite_model(&body_bytes, &backend_cfg.model_map)
        };

        let send_once = |auth_header: String, add_oauth_beta: bool, body: Bytes| {
            let mut b = state
                .client
                .request(parts.method.clone(), &url)
                .headers(forward_headers.clone())
                .header("Authorization", auth_header)
                .body(body);
            if add_oauth_beta {
                b = b.header("anthropic-beta", OAUTH_BETA);
            }
            b.send()
        };

        let resp = match &backend.auth {
            BackendAuth::Oauth { .. } => {
                let token = current_oauth_token(backend).await;
                let first = send_once(format!("Bearer {token}"), true, forward_body.clone()).await;
                match first {
                    Ok(r) if r.status().as_u16() == 401 => {
                        tracing::warn!(%backend_name, "got 401, reloading credentials and retrying");
                        let fresh = reload_token(backend).await;
                        match send_once(format!("Bearer {fresh}"), true, forward_body.clone()).await
                        {
                            Ok(r) if r.status().as_u16() != 401 => r,
                            Ok(_) | Err(_) => {
                                tracing::warn!(%backend_name, "still failing after token refresh, trying next");
                                continue;
                            }
                        }
                    }
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(%backend_name, "request error: {e}");
                        continue;
                    }
                }
            }
            BackendAuth::ApiKey { key } => {
                let key_value = key.read().await.clone();
                match send_once(format!("Bearer {key_value}"), false, forward_body.clone()).await
                {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(%backend_name, "request error: {e}");
                        continue;
                    }
                }
            }
        };

        let status = resp.status().as_u16();
        if state.config.failover.triggers.contains(&status) {
            tracing::warn!(%backend_name, %status, "trigger status, trying next backend");
            state.metrics.set_last_status(backend_name, status);
            state.metrics.inc_failovers();
            continue;
        }
        tracing::info!(%backend_name, %status, "response");
        state.metrics.set_last_status(backend_name, status);
        return stream_response(resp).await;
    }

    tracing::error!("all backends exhausted");
    StatusCode::BAD_GATEWAY.into_response()
}

/// Return the current OAuth access token, reloading from disk if expired.
/// Panics if called on a non-OAuth backend.
async fn current_oauth_token(backend: &BackendState) -> String {
    let BackendAuth::Oauth { token, .. } = &backend.auth else {
        unreachable!("current_oauth_token called on non-oauth backend");
    };
    let cred = token.read().await;
    if cred.is_expired() {
        drop(cred);
        tracing::info!("token expired, reloading credentials");
        reload_token(backend).await
    } else {
        cred.access_token.clone()
    }
}

/// Re-read the credentials file and update the stored token. Returns the fresh access token.
/// Panics if called on a non-OAuth backend.
async fn reload_token(backend: &BackendState) -> String {
    let BackendAuth::Oauth { credentials_file, token } = &backend.auth else {
        unreachable!("reload_token called on non-oauth backend");
    };
    match load_credentials(credentials_file) {
        Ok(cred) => {
            let t = cred.access_token.clone();
            *token.write().await = cred;
            t
        }
        Err(e) => {
            tracing::error!("failed to reload credentials from {credentials_file}: {e}");
            token.read().await.access_token.clone()
        }
    }
}

/// Rewrite the `model` field of a JSON request body. Silently returns the
/// original bytes if the body is not JSON or does not contain a `model` key —
/// non-Messages endpoints (e.g. `/v1/models`) shouldn't be mangled.
fn rewrite_model(body: &Bytes, map: &HashMap<String, String>) -> Bytes {
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return body.clone();
    };
    let Some(model) = v.get("model").and_then(|m| m.as_str()) else {
        return body.clone();
    };
    let Some(replacement) = map.get(model) else {
        return body.clone();
    };
    tracing::debug!(from = %model, to = %replacement, "rewriting model id");
    v["model"] = serde_json::Value::String(replacement.clone());
    match serde_json::to_vec(&v) {
        Ok(b) => Bytes::from(b),
        Err(e) => {
            tracing::warn!("model rewrite reserialize failed: {e}");
            body.clone()
        }
    }
}

fn strip_auth_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        match name.as_str().to_ascii_lowercase().as_str() {
            // content-length is stripped because we may rewrite the body (model-id
            // substitution for api_key backends); reqwest recomputes it from the
            // actual bytes we pass to .body().
            "authorization" | "x-api-key" | "host" | "transfer-encoding" | "content-length" => {
                continue
            }
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
