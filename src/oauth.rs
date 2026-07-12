use crate::crypto::Tokens;
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Clone)]
pub struct OAuthClient {
    client: reqwest::Client,
    issuer: String,
    client_id: String,
}
#[derive(Debug, Deserialize)]
pub struct DeviceCode {
    pub device_auth_id: String,
    #[serde(alias = "usercode")]
    pub user_code: String,
    #[serde(deserialize_with = "interval")]
    pub interval: u64,
}
#[derive(Serialize)]
struct UserCodeReq<'a> {
    client_id: &'a str,
}
#[derive(Debug, Deserialize)]
pub struct PollCode {
    pub authorization_code: String,
    pub code_verifier: String,
    pub code_challenge: String,
}
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    id_token: String,
    expires_in: i64,
}
fn interval<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let v = Value::deserialize(d)?;
    match v {
        Value::String(s) => s.parse().map_err(serde::de::Error::custom),
        Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| serde::de::Error::custom("invalid interval")),
        _ => Err(serde::de::Error::custom("invalid interval")),
    }
}
impl OAuthClient {
    pub fn new(issuer: String, client_id: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            issuer,
            client_id,
        }
    }
    pub async fn start(&self) -> anyhow::Result<DeviceCode> {
        let r = self
            .client
            .post(format!("{}/api/accounts/deviceauth/usercode", self.issuer))
            .json(&UserCodeReq {
                client_id: &self.client_id,
            })
            .send()
            .await?
            .error_for_status()?;
        Ok(r.json().await?)
    }
    pub fn verification_url(&self) -> String {
        format!("{}/codex/device", self.issuer)
    }
    pub async fn poll_once(
        &self,
        device_auth_id: &str,
        user_code: &str,
    ) -> anyhow::Result<Option<PollCode>> {
        let r = self
            .client
            .post(format!("{}/api/accounts/deviceauth/token", self.issuer))
            .json(&serde_json::json!({"device_auth_id":device_auth_id,"user_code":user_code}))
            .send()
            .await?;
        if matches!(r.status().as_u16(), 403 | 404) {
            return Ok(None);
        }
        Ok(Some(r.error_for_status()?.json().await?))
    }
    pub async fn exchange(&self, c: PollCode) -> anyhow::Result<Tokens> {
        let redirect = format!("{}/deviceauth/callback", self.issuer);
        let form = vec![
            ("grant_type", "authorization_code".to_owned()),
            ("client_id", self.client_id.clone()),
            ("code", c.authorization_code),
            ("redirect_uri", redirect),
            ("code_verifier", c.code_verifier),
        ];
        let r = self
            .client
            .post(format!("{}/oauth/token", self.issuer))
            .form(&form)
            .send()
            .await?
            .error_for_status()?
            .json::<TokenResponse>()
            .await?;
        tokens(r)
    }
    pub async fn refresh(&self, refresh: &str) -> anyhow::Result<Tokens> {
        let form = vec![
            ("grant_type", "refresh_token".to_owned()),
            ("client_id", self.client_id.clone()),
            ("refresh_token", refresh.to_owned()),
        ];
        let r = self
            .client
            .post(format!("{}/oauth/token", self.issuer))
            .form(&form)
            .send()
            .await?
            .error_for_status()?
            .json::<TokenResponse>()
            .await?;
        tokens(r)
    }
}
fn tokens(r: TokenResponse) -> anyhow::Result<Tokens> {
    let payload = r
        .id_token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("invalid ID token"))?;
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, payload)?;
    let claims: Value = serde_json::from_slice(&bytes)?;
    let auth = claims
        .get("https://api.openai.com/auth")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("ID token lacks OpenAI workspace claims"))?;
    let account_id = auth
        .get("chatgpt_account_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("ID token lacks ChatGPT account ID"))?
        .to_string();
    Ok(Tokens {
        access_token: r.access_token,
        refresh_token: r.refresh_token,
        id_token: r.id_token,
        expires_at: now() + r.expires_in,
        account_id,
    })
}
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
