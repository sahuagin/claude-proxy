use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub proxy: ProxyConfig,
    pub backends: Vec<Backend>,
    pub failover: FailoverConfig,
    #[serde(default)]
    pub fault_injection: FaultInjectionConfig,
    /// Optional pointer to a central TOML containing secrets/paths. Fields
    /// elsewhere in this config that start with `$REF:dotted.path` are
    /// resolved against this file at load time.
    #[serde(default)]
    pub secrets: Option<SecretsConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SecretsConfig {
    pub source: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ProxyConfig {
    pub listen: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Backend {
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub auth: AuthConfig,
    /// Path to the OAuth credentials.json. Required for `auth = "oauth"`.
    /// May use `$REF:dotted.path` to resolve from `[secrets].source`.
    #[serde(default)]
    pub credentials_file: Option<String>,
    /// Static API key value. Required for `auth = "api_key"`.
    /// May use `$REF:dotted.path` to resolve from `[secrets].source`.
    #[serde(default)]
    pub key: Option<String>,
    /// Map of client-side model id → upstream model id. Applied to the
    /// `model` field of JSON request bodies before forwarding. Used to
    /// translate e.g. `claude-opus-4-7` → `anthropic/claude-opus-4.7` for
    /// OpenRouter.
    #[serde(default)]
    pub model_map: HashMap<String, String>,
    /// Path-prefix allowlist. If non-empty, this backend is only tried for
    /// requests whose path starts with one of the listed prefixes. Prevents
    /// e.g. OAuth-refresh traffic (`/oauth/token`) from being offered to
    /// non-Anthropic upstreams. Defaults are supplied per auth kind — see
    /// `effective_allowed_path_prefixes`.
    #[serde(default)]
    pub allowed_path_prefixes: Option<Vec<String>>,
}

impl Backend {
    /// Resolve the effective allowlist: explicit config wins, else a sensible
    /// per-auth-kind default. For `api_key` we restrict to inference endpoints
    /// by default so nothing auth-related leaks off-platform.
    pub fn effective_allowed_path_prefixes(&self) -> Vec<String> {
        if let Some(list) = &self.allowed_path_prefixes {
            return list.clone();
        }
        match self.auth {
            AuthConfig::Oauth => vec![],
            AuthConfig::ApiKey => vec!["/v1/messages".to_string()],
        }
    }
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthConfig {
    #[default]
    Oauth,
    ApiKey,
}

#[derive(Debug, Deserialize, Clone)]
pub struct FailoverConfig {
    pub order: Vec<String>,
    #[serde(default = "default_triggers")]
    pub triggers: Vec<u16>,
}

fn default_triggers() -> Vec<u16> {
    vec![500, 502, 503, 504, 429, 529]
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
        let mut cfg: Self = toml::from_str(&content).context("parsing config TOML")?;
        cfg.resolve_refs()?;
        Ok(cfg)
    }

    fn secrets_source_path(&self) -> Option<PathBuf> {
        self.secrets
            .as_ref()
            .map(|s| PathBuf::from(shellexpand::tilde(&s.source).into_owned()))
    }

    fn resolve_refs(&mut self) -> Result<()> {
        let secrets_source = self.secrets_source_path();
        for backend in &mut self.backends {
            if let Some(cf) = backend.credentials_file.take() {
                backend.credentials_file = Some(resolve_ref(&cf, secrets_source.as_deref())?);
            }
            if let Some(k) = backend.key.take() {
                backend.key = Some(resolve_ref(&k, secrets_source.as_deref())?);
            }
        }
        Ok(())
    }

    /// Re-resolve a single backend's `key` from the secrets source. Used by
    /// the mgmt `/reload` endpoint so api_key values can be rotated without
    /// a service restart.
    pub fn resolve_backend_key(&self, backend_name: &str) -> Result<String> {
        let backend = self
            .backends
            .iter()
            .find(|b| b.name == backend_name)
            .with_context(|| format!("backend '{backend_name}' not found"))?;
        // We need the original `$REF:` spec to re-resolve, but resolve_refs
        // overwrites it with the resolved value at load time. Instead, read
        // the secrets source directly using the same field path each backend
        // declares — but we don't store the spec. Workaround: re-read the
        // on-disk config.toml fresh, find this backend's declared key, and
        // resolve it.
        let _ = backend; // suppress unused warning until below path is used
        let cfg_path = std::env::var("CLAUDE_PROXY_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                PathBuf::from(home).join(".config/claude-proxy/config.toml")
            });
        let content = std::fs::read_to_string(&cfg_path)
            .with_context(|| format!("reading config from {}", cfg_path.display()))?;
        let raw: Self = toml::from_str(&content).context("parsing config TOML")?;
        let raw_backend = raw
            .backends
            .iter()
            .find(|b| b.name == backend_name)
            .with_context(|| format!("backend '{backend_name}' not in on-disk config"))?;
        let spec = raw_backend
            .key
            .as_ref()
            .with_context(|| format!("backend '{backend_name}' has no key field"))?;
        let secrets_source = raw.secrets_source_path();
        resolve_ref(spec, secrets_source.as_deref())
    }
}

/// Resolve a single `$REF:dotted.path` indirection against the secrets
/// source TOML. Strings without the `$REF:` prefix are returned unchanged.
fn resolve_ref(value: &str, secrets_source: Option<&Path>) -> Result<String> {
    let Some(field_path) = value.strip_prefix("$REF:") else {
        return Ok(value.to_string());
    };
    let source =
        secrets_source.context("$REF: indirection requires [secrets].source in config")?;
    let content = std::fs::read_to_string(source)
        .with_context(|| format!("reading secrets source {}", source.display()))?;
    let parsed: toml::Value = toml::from_str(&content)
        .with_context(|| format!("parsing secrets source {}", source.display()))?;
    let mut current = &parsed;
    for part in field_path.split('.') {
        current = current.get(part).with_context(|| {
            format!(
                "field path '{}' not found in {} (missing segment '{}')",
                field_path,
                source.display(),
                part
            )
        })?;
    }
    let s = current.as_str().with_context(|| {
        format!(
            "field '{}' in {} is not a string",
            field_path,
            source.display()
        )
    })?;
    Ok(s.to_string())
}

pub fn load_credentials(path: &str) -> Result<OAuthCredential> {
    let expanded = shellexpand::tilde(path).into_owned();
    let content = std::fs::read_to_string(&expanded)
        .with_context(|| format!("reading credentials from {expanded}"))?;
    let creds: Credentials =
        serde_json::from_str(&content).context("parsing credentials JSON")?;
    Ok(creds.claude_ai_oauth)
}
