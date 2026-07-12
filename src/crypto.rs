use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: String,
    pub expires_at: i64,
    pub account_id: String,
}

#[derive(Clone)]
pub struct Vault([u8; 32]);
impl Vault {
    pub fn new(key: [u8; 32]) -> Self {
        Self(key)
    }
    pub fn encrypt(&self, user_id: &str, version: i64, tokens: &Tokens) -> anyhow::Result<Vec<u8>> {
        let cipher = Aes256Gcm::new_from_slice(&self.0).unwrap();
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let aad = format!("cvc:{user_id}:{version}");
        let mut out = nonce.to_vec();
        out.extend(
            cipher
                .encrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: &serde_json::to_vec(tokens)?,
                        aad: aad.as_bytes(),
                    },
                )
                .map_err(|_| anyhow::anyhow!("credential encryption failed"))?,
        );
        Ok(out)
    }
    pub fn decrypt(&self, user_id: &str, version: i64, data: &[u8]) -> anyhow::Result<Tokens> {
        if data.len() < 13 {
            anyhow::bail!("invalid credential envelope")
        }
        let aad = format!("cvc:{user_id}:{version}");
        let plain = Aes256Gcm::new_from_slice(&self.0)
            .unwrap()
            .decrypt(
                Nonce::from_slice(&data[..12]),
                Payload {
                    msg: &data[12..],
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| anyhow::anyhow!("credential decryption failed"))?;
        Ok(serde_json::from_slice(&plain)?)
    }
}
pub fn issue_gateway_key() -> String {
    let mut b = [0u8; 32];
    OsRng.fill_bytes(&mut b);
    format!(
        "cvc_{}",
        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, b)
    )
}
pub fn hash_key(key: &str) -> anyhow::Result<String> {
    Argon2::default()
        .hash_password(key.as_bytes(), &SaltString::generate(&mut OsRng))
        .map(|v| v.to_string())
        .map_err(|e| anyhow::anyhow!("gateway key hashing failed: {e}"))
}
pub fn verify_key(hash: &str, key: &str) -> bool {
    PasswordHash::new(hash).ok().is_some_and(|h| {
        Argon2::default()
            .verify_password(key.as_bytes(), &h)
            .is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_and_aad() {
        let v = Vault::new([7; 32]);
        let t = Tokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            id_token: "i".into(),
            expires_at: 1,
            account_id: "x".into(),
        };
        let c = v.encrypt("u", 1, &t).unwrap();
        assert_eq!(v.decrypt("u", 1, &c).unwrap().access_token, "a");
        assert!(v.decrypt("other", 1, &c).is_err());
    }
    #[test]
    fn hashes() {
        let k = issue_gateway_key();
        let h = hash_key(&k).unwrap();
        assert!(verify_key(&h, &k));
        assert!(!verify_key(&h, "bad"));
    }
}
