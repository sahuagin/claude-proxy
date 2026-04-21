use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub proxy: ProxyConfig,
    pub backends: Vec<Backend>,
    pub failover: FailoverConfig,
    #[serde(default)]
    pub fault_injection: FaultInjectionConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ProxyConfig {
    pub listen: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Backend {
    pub name: String,
    pub base_url: String,
    pub credentials_file: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct FailoverConfig {
    pub order: Vec<String>,
    #[serde(default = "default_triggers")]
    pub triggers: Vec<u16>,
}

fn default_triggers() -> Vec<u16> {
    vec![500, 502, 503, 504, 429]
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct FaultInjectionConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub rules: Vec<FaultRule>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct FaultRule {
    pub backend: String,
    /// HTTP status code to return instead of forwarding (e.g. 500, 429)
    pub status: u16,
    /// Probability 0.0–1.0 that this rule fires on a given request
    #[serde(default = "default_rate")]
    pub rate: f64,
}

fn default_rate() -> f64 {
    1.0
}

impl FaultInjectionConfig {
    pub fn as_map(&self) -> HashMap<String, FaultRule> {
        self.rules.iter().map(|r| (r.backend.clone(), r.clone())).collect()
    }
}

/// The relevant subset of ~/.claude*/credentials.json
#[derive(Debug, Deserialize)]
pub struct Credentials {
    #[serde(rename = "claudeAiOauth")]
    pub claude_ai_oauth: OAuthCredential,
}

#[derive(Debug, Deserialize, Clone)]
pub struct OAuthCredential {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(rename = "refreshToken")]
    pub refresh_token: String,
    #[serde(rename = "expiresAt")]
    pub expires_at: u64,
}

impl OAuthCredential {
    pub fn is_expired(&self) -> bool {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        now_ms >= self.expires_at
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading config from {}", path.display()))?;
        toml::from_str(&content).context("parsing config TOML")
    }
}

pub fn load_credentials(path: &str) -> Result<OAuthCredential> {
    let expanded = shellexpand::tilde(path).into_owned();
    let content = std::fs::read_to_string(&expanded)
        .with_context(|| format!("reading credentials from {expanded}"))?;
    let creds: Credentials =
        serde_json::from_str(&content).context("parsing credentials JSON")?;
    Ok(creds.claude_ai_oauth)
}
