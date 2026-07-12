use crate::{
    crypto::{Tokens, Vault},
    db::Repository,
    error::ApiError,
    oauth::{OAuthClient, now},
};
use dashmap::DashMap;
use futures::StreamExt;
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
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
}
impl CodexClient {
    pub fn new(
        repo: Arc<dyn Repository>,
        vault: Vault,
        oauth: OAuthClient,
        url: String,
        concurrency: usize,
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
            url,
            locks: Default::default(),
            limits: Default::default(),
            concurrency,
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
    pub async fn responses(&self, id: &str, body: Value) -> Result<Upstream, ApiError> {
        let permit = self.permit(id).await?;
        for force in [false, true] {
            let t = self.tokens(id, force).await?;
            let r = self
                .http
                .post(&self.url)
                .bearer_auth(&t.access_token)
                .header("chatgpt-account-id", &t.account_id)
                .header("originator", "cvc")
                .header("openai-beta", "responses=experimental")
                .header("accept", "text/event-stream")
                .json(&body)
                .send()
                .await
                .map_err(|_| {
                    ApiError::new(
                        http::StatusCode::BAD_GATEWAY,
                        "api_error",
                        "upstream connection failed",
                    )
                })?;
            if r.status() == http::StatusCode::UNAUTHORIZED && !force {
                continue;
            }
            if !r.status().is_success() {
                let status = r.status();
                let retry = r
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                let (out, kind) = match status.as_u16() {
                    400..=422 => (http::StatusCode::BAD_REQUEST, "invalid_request_error"),
                    429 => (http::StatusCode::TOO_MANY_REQUESTS, "rate_limit_error"),
                    503 => (http::StatusCode::SERVICE_UNAVAILABLE, "overloaded_error"),
                    _ => (http::StatusCode::BAD_GATEWAY, "api_error"),
                };
                let mut e = ApiError::new(out, kind, format!("Codex upstream returned {status}"));
                e.retry_after = retry;
                return Err(e);
            }
            return Ok(Upstream {
                response: r,
                _permit: permit,
            });
        }
        Err(ApiError::auth("OpenAI authentication failed"))
    }
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
