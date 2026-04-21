use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use axum::{routing::any, Router};
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;

mod config;
mod mgmt;
mod metrics;
mod proxy;

use proxy::{AppState, BackendState};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("claude_proxy=info")),
        )
        .init();

    let config_path = std::env::var("CLAUDE_PROXY_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_config_dir().join("config.toml"));

    tracing::info!("config: {}", config_path.display());
    let cfg = config::Config::load(&config_path)?;

    let mut backends = HashMap::new();
    for backend in &cfg.backends {
        tracing::info!("loading credentials for backend '{}'", backend.name);
        let cred = config::load_credentials(&backend.credentials_file)
            .with_context(|| format!("loading credentials for backend '{}'", backend.name))?;
        if cred.is_expired() {
            tracing::warn!(
                "token for '{}' is already expired — will reload on first request",
                backend.name
            );
        }
        backends.insert(
            backend.name.clone(),
            Arc::new(BackendState {
                credentials_file: backend.credentials_file.clone(),
                token: RwLock::new(cred),
            }),
        );
    }

    let faults = cfg.fault_injection.as_map();
    if !faults.is_empty() {
        tracing::warn!(
            "fault injection enabled for backends: {:?}",
            faults.keys().collect::<Vec<_>>()
        );
    }

    let backend_names: Vec<String> = cfg.backends.iter().map(|b| b.name.clone()).collect();
    let metrics = metrics::Metrics::new(&backend_names);

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building HTTP client")?;

    let state = Arc::new(AppState {
        config: cfg.clone(),
        backends,
        faults: RwLock::new(faults),
        metrics,
        client,
    });

    // Start mgmt server if CLAUDE_PROXY_MGMT is set
    if let Ok(mgmt_addr) = std::env::var("CLAUDE_PROXY_MGMT") {
        let mgmt_state = Arc::clone(&state);
        let mgmt_listener = tokio::net::TcpListener::bind(&mgmt_addr)
            .await
            .with_context(|| format!("binding mgmt server to {mgmt_addr}"))?;
        tracing::info!("mgmt API listening on {mgmt_addr}");
        tokio::spawn(async move {
            if let Err(e) = axum::serve(mgmt_listener, mgmt::router(mgmt_state)).await {
                tracing::error!("mgmt server error: {e}");
            }
        });
    }

    let app = Router::new()
        .route("/{*path}", any(proxy::proxy_handler))
        .with_state(state);

    let listen = cfg.proxy.listen.clone();
    tracing::info!("listening on {listen}");
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding to {listen}"))?;

    axum::serve(listener, app).await.context("server error")
}

fn default_config_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config/claude-proxy")
}
