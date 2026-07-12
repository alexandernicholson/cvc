use anyhow::{Context, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Model {
    pub alias: String,
    pub display_name: String,
    pub upstream: String,
    pub efforts: Vec<String>,
    pub context_limit: u64,
    pub output_limit: u64,
    #[serde(default)]
    pub structured_output: bool,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub database_url: String,
    pub public_url: Option<String>,
    pub trusted_proxy: bool,
    pub master_key: [u8; 32],
    pub models: HashMap<String, Model>,
    pub default_model: String,
    pub upstream_url: String,
    pub oauth_issuer: String,
    pub oauth_client_id: String,
    pub max_body_bytes: usize,
    pub per_user_concurrency: usize,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let bind: SocketAddr = std::env::var("CVC_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8080".into())
            .parse()?;
        let database_url =
            std::env::var("CVC_DATABASE_URL").unwrap_or_else(|_| "sqlite://cvc.db?mode=rwc".into());
        let public_url = std::env::var("CVC_PUBLIC_URL").ok();
        let trusted_proxy = env_bool("CVC_TRUSTED_PROXY", false);
        if !(is_private(bind.ip()) || trusted_proxy && public_url.is_some()) {
            bail!("public bind requires CVC_TRUSTED_PROXY=true and CVC_PUBLIC_URL");
        }
        let raw = std::env::var("CVC_MASTER_KEY")
            .context("CVC_MASTER_KEY is required (base64, 32 bytes)")?;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(raw)
            .context("invalid CVC_MASTER_KEY base64")?;
        let master_key: [u8; 32] = decoded
            .try_into()
            .map_err(|_| anyhow::anyhow!("CVC_MASTER_KEY must decode to 32 bytes"))?;
        let model_json = std::env::var("CVC_MODELS").context("CVC_MODELS is required")?;
        let entries: Vec<Model> =
            serde_json::from_str(&model_json).context("invalid CVC_MODELS JSON")?;
        let mut models = HashMap::new();
        for model in entries {
            if !model.alias.starts_with("claude-") && !model.alias.starts_with("anthropic-") {
                bail!("model alias must begin with claude- or anthropic-");
            }
            if model
                .efforts
                .iter()
                .any(|v| !matches!(v.as_str(), "low" | "medium" | "high" | "xhigh" | "max"))
            {
                bail!("invalid effort for {}", model.alias);
            }
            models.insert(model.alias.clone(), model);
        }
        let default_model =
            std::env::var("CVC_DEFAULT_MODEL").unwrap_or_else(|_| "claude-codex-default".into());
        if !models.contains_key(&default_model) {
            bail!("CVC_DEFAULT_MODEL is not present in CVC_MODELS");
        }
        Ok(Self {
            bind,
            database_url,
            public_url,
            trusted_proxy,
            master_key,
            models,
            default_model,
            upstream_url: std::env::var("CVC_UPSTREAM_URL")
                .unwrap_or_else(|_| "https://chatgpt.com/backend-api/codex/responses".into()),
            oauth_issuer: std::env::var("CVC_OAUTH_ISSUER")
                .unwrap_or_else(|_| "https://auth.openai.com".into()),
            oauth_client_id: std::env::var("CVC_OAUTH_CLIENT_ID")
                .unwrap_or_else(|_| "app_EMoamEEZ73f0CkXaXp7hrann".into()),
            max_body_bytes: env_num("CVC_MAX_BODY_BYTES", 10 * 1024 * 1024),
            per_user_concurrency: env_num("CVC_PER_USER_CONCURRENCY", 4),
        })
    }
}
fn env_bool(k: &str, d: bool) -> bool {
    std::env::var(k)
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(d)
}
fn env_num<T: std::str::FromStr>(k: &str, d: T) -> T {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}
fn is_private(ip: IpAddr) -> bool {
    ip.is_loopback()
        || match ip {
            IpAddr::V4(v) => v.is_private() || v.is_link_local(),
            IpAddr::V6(v) => v.is_unique_local() || v.is_unicast_link_local(),
        }
}
