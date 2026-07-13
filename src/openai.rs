use crate::{
    config::Model,
    crypto::{Tokens, Vault},
    db::Repository,
    error::ApiError,
    oauth::{OAuthClient, now},
};
use anyhow::{Context, anyhow, bail};
use dashmap::DashMap;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
#[derive(Clone)]
struct CachedModelCatalog {
    fetched_at: Instant,
    etag: Option<String>,
    models: HashMap<String, Model>,
}

#[derive(Deserialize)]
struct UpstreamModelsResponse {
    models: Vec<UpstreamModel>,
}

#[derive(Deserialize)]
struct UpstreamModel {
    slug: String,
    display_name: String,
    #[serde(default)]
    supported_reasoning_levels: Vec<UpstreamReasoningLevel>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default = "default_true")]
    supported_in_api: bool,
    context_window: Option<u64>,
}

#[derive(Deserialize)]
struct UpstreamReasoningLevel {
    effort: String,
}

fn default_true() -> bool {
    true
}

pub struct CodexOptions {
    pub responses_url: String,
    pub models_url: String,
    pub client_version: String,
    pub model_ttl: Duration,
    pub seed_models: HashMap<String, Model>,
    pub concurrency: usize,
}

#[derive(Clone)]
pub struct CodexClient {
    http: reqwest::Client,
    repo: Arc<dyn Repository>,
    vault: Vault,
    oauth: OAuthClient,
    url: String,
    locks: Arc<DashMap<String, Arc<Mutex<()>>>>,
    limits: Arc<DashMap<String, Arc<Semaphore>>>,
    concurrency: usize,
    models_url: String,
    client_version: String,
    model_ttl: Duration,
    seed_models: Arc<HashMap<String, Model>>,
    model_cache: Arc<DashMap<String, CachedModelCatalog>>,
    model_locks: Arc<DashMap<String, Arc<Mutex<()>>>>,
}
impl CodexClient {
    pub fn new(
        repo: Arc<dyn Repository>,
        vault: Vault,
        oauth: OAuthClient,
        options: CodexOptions,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(300))
                .build()
                .unwrap(),
            repo,
            vault,
            oauth,
            url: options.responses_url,
            models_url: options.models_url,
            client_version: options.client_version,
            model_ttl: options.model_ttl,
            seed_models: Arc::new(options.seed_models),
            model_cache: Default::default(),
            model_locks: Default::default(),
            locks: Default::default(),
            limits: Default::default(),
            concurrency: options.concurrency,
        }
    }
    async fn permit(&self, id: &str) -> Result<OwnedSemaphorePermit, ApiError> {
        let s = self
            .limits
            .entry(id.into())
            .or_insert_with(|| Arc::new(Semaphore::new(self.concurrency)))
            .clone();
        s.try_acquire_owned().map_err(|_| {
            let mut e = ApiError::new(
                http::StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "per-user concurrency limit exceeded",
            );
            e.retry_after = Some("1".into());
            e
        })
    }
    pub async fn tokens(&self, id: &str, force: bool) -> Result<Tokens, ApiError> {
        let lock = self
            .locks
            .entry(id.into())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _g = lock.lock().await;
        let stored = self
            .repo
            .credential(id)
            .await
            .map_err(|_| ApiError::server("credential store unavailable"))?
            .ok_or_else(|| ApiError::auth("OpenAI account is not connected; run cvc login"))?;
        let mut t = self
            .vault
            .decrypt(id, stored.version, &stored.ciphertext)
            .map_err(|_| ApiError::server("credential cannot be decrypted"))?;
        if force || t.expires_at <= now() + 60 {
            t = self.oauth.refresh(&t.refresh_token).await.map_err(|_| {
                ApiError::auth("OpenAI credential refresh failed; reconnect account")
            })?;
            let v = stored.version + 1;
            let c = self
                .vault
                .encrypt(id, v, &t)
                .map_err(|_| ApiError::server("credential encryption failed"))?;
            self.repo
                .put_credential(id, v, &c)
                .await
                .map_err(|_| ApiError::server("credential store unavailable"))?;
        }
        Ok(t)
    }
    pub async fn models(&self, id: &str, force: bool) -> HashMap<String, Model> {
        if !force
            && let Some(cached) = self.model_cache.get(id)
            && cached.fetched_at.elapsed() < self.model_ttl
        {
            return cached.models.clone();
        }

        let lock = self
            .model_locks
            .entry(id.into())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;

        if !force
            && let Some(cached) = self.model_cache.get(id)
            && cached.fetched_at.elapsed() < self.model_ttl
        {
            return cached.models.clone();
        }

        let previous = self.model_cache.get(id).map(|cached| cached.clone());
        let etag = previous.as_ref().and_then(|cached| cached.etag.as_deref());
        let refresh = async {
            let mut tokens = self
                .tokens(id, false)
                .await
                .map_err(|_| anyhow!("credential unavailable"))?;
            let mut response = self.fetch_models(&tokens, etag).await;
            if response
                .as_ref()
                .is_ok_and(|response| response.status() == http::StatusCode::UNAUTHORIZED)
            {
                tokens = self
                    .tokens(id, true)
                    .await
                    .map_err(|_| anyhow!("credential refresh unavailable"))?;
                response = self.fetch_models(&tokens, etag).await;
            }
            let response = response.context("model discovery request failed")?;
            if response.status() == http::StatusCode::NOT_MODIFIED {
                return Ok::<_, anyhow::Error>(None);
            }
            if !response.status().is_success() {
                bail!("model discovery returned {}", response.status());
            }
            let etag = response
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let response = response
                .json::<UpstreamModelsResponse>()
                .await
                .context("invalid model discovery response")?;
            let models = self.catalog_from_upstream(response.models);
            if models.is_empty() {
                bail!("model discovery returned no usable models");
            }
            Ok(Some((models, etag)))
        }
        .await;

        match refresh {
            Ok(Some((models, etag))) => {
                tracing::info!(
                    user_id = id,
                    model_count = models.len(),
                    "Codex model catalog refreshed"
                );
                self.model_cache.insert(
                    id.into(),
                    CachedModelCatalog {
                        fetched_at: Instant::now(),
                        etag,
                        models: models.clone(),
                    },
                );
                models
            }
            Ok(None) => {
                if let Some(mut cached) = previous {
                    cached.fetched_at = Instant::now();
                    let models = cached.models.clone();
                    self.model_cache.insert(id.into(), cached);
                    models
                } else {
                    tracing::warn!(
                        user_id = id,
                        "Codex model endpoint returned 304 without a cached catalog"
                    );
                    self.seed_models.as_ref().clone()
                }
            }
            Err(error) => {
                tracing::warn!(user_id = id, %error, "Codex model catalog refresh failed; using cached catalog");
                if let Some(mut cached) = previous {
                    cached.fetched_at = Instant::now()
                        .checked_sub(self.model_ttl)
                        .unwrap_or(cached.fetched_at);
                    let models = cached.models.clone();
                    self.model_cache.insert(id.into(), cached);
                    models
                } else {
                    self.seed_models.as_ref().clone()
                }
            }
        }
    }

    fn suppress_rejected_model(&self, id: &str, upstream: &str) {
        let mut cached = self
            .model_cache
            .get(id)
            .map(|cached| cached.clone())
            .unwrap_or_else(|| CachedModelCatalog {
                fetched_at: Instant::now(),
                etag: None,
                models: self.seed_models.as_ref().clone(),
            });
        cached.fetched_at = Instant::now();
        cached.models.retain(|_, model| model.upstream != upstream);
        self.model_cache.insert(id.into(), cached);
    }

    async fn fetch_models(
        &self,
        tokens: &Tokens,
        etag: Option<&str>,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let mut request = self
            .http
            .get(&self.models_url)
            .query(&[("client_version", &self.client_version)])
            .bearer_auth(&tokens.access_token)
            .header("chatgpt-account-id", &tokens.account_id)
            .header("originator", "cvc")
            .header("version", &self.client_version);
        if let Some(etag) = etag {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        request.send().await
    }

    fn catalog_from_upstream(&self, models: Vec<UpstreamModel>) -> HashMap<String, Model> {
        models
            .into_iter()
            .filter(|model| {
                model.supported_in_api && model.visibility.as_deref().unwrap_or("list") == "list"
            })
            .map(|model| {
                let seed = self
                    .seed_models
                    .values()
                    .find(|seed| seed.upstream == model.slug);
                let alias = seed
                    .map(|seed| seed.alias.clone())
                    .unwrap_or_else(|| model_alias(&model.slug));
                let efforts = model
                    .supported_reasoning_levels
                    .into_iter()
                    .map(|level| level.effort)
                    .filter(|effort| {
                        matches!(effort.as_str(), "low" | "medium" | "high" | "xhigh" | "max")
                    })
                    .collect::<Vec<_>>();
                let entry = Model {
                    alias: alias.clone(),
                    display_name: model.display_name,
                    upstream: model.slug,
                    efforts: if efforts.is_empty() {
                        seed.map(|seed| seed.efforts.clone())
                            .unwrap_or_else(|| vec!["medium".into()])
                    } else {
                        efforts
                    },
                    context_limit: model
                        .context_window
                        .or_else(|| seed.map(|seed| seed.context_limit))
                        .unwrap_or(272_000),
                    output_limit: seed.map(|seed| seed.output_limit).unwrap_or(32_000),
                    structured_output: seed.map(|seed| seed.structured_output).unwrap_or(true),
                };
                (alias, entry)
            })
            .collect()
    }

    pub async fn responses(&self, id: &str, body: Value) -> Result<Upstream, ApiError> {
        const MAX_TRANSIENT_RETRIES: usize = 2;
        let permit = self.permit(id).await?;
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let mut transient_retries = 0;
        let mut request_attempt = 0;
        let mut force_refresh = false;
        let mut refreshed = false;

        loop {
            request_attempt += 1;
            let tokens = self.tokens(id, force_refresh).await?;
            force_refresh = false;
            let response = self
                .http
                .post(&self.url)
                .bearer_auth(&tokens.access_token)
                .header("chatgpt-account-id", &tokens.account_id)
                .header("originator", "cvc")
                .header("openai-beta", "responses=experimental")
                .header("accept", "text/event-stream")
                .json(&body)
                .send()
                .await;

            let response = match response {
                Ok(response) => response,
                Err(_) if transient_retries < MAX_TRANSIENT_RETRIES => {
                    transient_retries += 1;
                    tracing::warn!(
                        model,
                        attempt = request_attempt,
                        retry = transient_retries,
                        "Codex upstream connection failed before streaming; retrying"
                    );
                    transient_backoff(transient_retries).await;
                    continue;
                }
                Err(_) => {
                    tracing::warn!(
                        model,
                        attempt = request_attempt,
                        "Codex upstream connection failed before streaming"
                    );
                    return Err(ApiError::new(
                        http::StatusCode::BAD_GATEWAY,
                        "api_error",
                        "upstream connection failed",
                    ));
                }
            };

            let status = response.status();
            if status == http::StatusCode::UNAUTHORIZED && !refreshed {
                refreshed = true;
                force_refresh = true;
                tracing::warn!(
                    model,
                    attempt = request_attempt,
                    upstream_status = status.as_u16(),
                    "Codex upstream rejected credential; refreshing once"
                );
                continue;
            }

            let transient = matches!(status.as_u16(), 502..=504);
            if transient && transient_retries < MAX_TRANSIENT_RETRIES {
                transient_retries += 1;
                tracing::warn!(
                    model,
                    attempt = request_attempt,
                    retry = transient_retries,
                    upstream_status = status.as_u16(),
                    "Codex upstream failed before streaming; retrying"
                );
                transient_backoff(transient_retries).await;
                continue;
            }

            if !status.is_success() {
                let retry_after = response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                tracing::warn!(
                    model,
                    attempt = request_attempt,
                    upstream_status = status.as_u16(),
                    "Codex upstream request failed before streaming"
                );
                if status == http::StatusCode::NOT_FOUND {
                    drop(response);
                    self.models(id, true).await;
                    self.suppress_rejected_model(id, &model);
                    tracing::info!(
                        user_id = id,
                        model,
                        "Codex model catalog refreshed and rejected model temporarily suppressed"
                    );
                }
                let (out, kind) = match status.as_u16() {
                    401 => (http::StatusCode::UNAUTHORIZED, "authentication_error"),
                    400..=422 => (http::StatusCode::BAD_REQUEST, "invalid_request_error"),
                    429 => (http::StatusCode::TOO_MANY_REQUESTS, "rate_limit_error"),
                    503 => (http::StatusCode::SERVICE_UNAVAILABLE, "overloaded_error"),
                    _ => (http::StatusCode::BAD_GATEWAY, "api_error"),
                };
                let mut error =
                    ApiError::new(out, kind, format!("Codex upstream returned {status}"));
                error.retry_after = retry_after;
                return Err(error);
            }

            return Ok(Upstream {
                response,
                _permit: permit,
            });
        }
    }
}
fn model_alias(slug: &str) -> String {
    let normalized = slug
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    format!("claude-{normalized}")
}

async fn transient_backoff(retry: usize) {
    let base_ms = 150_u64.saturating_mul(1_u64 << retry.saturating_sub(1));
    let jitter_ms = rand::random::<u64>() % 101;
    tokio::time::sleep(Duration::from_millis(base_ms + jitter_ms)).await;
}
pub struct Upstream {
    response: reqwest::Response,
    _permit: OwnedSemaphorePermit,
}
impl Upstream {
    pub fn bytes_stream(self) -> impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> {
        let mut inner = Box::pin(self.response.bytes_stream());
        let permit = self._permit;
        async_stream::stream! {let _permit=permit;while let Some(item)=inner.next().await{yield item;}}
    }
}
