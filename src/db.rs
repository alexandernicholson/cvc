use async_trait::async_trait;
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{str::FromStr, time::Duration};

#[derive(Clone, Debug)]
pub struct User {
    pub id: String,
    pub name: String,
    pub key_hash: String,
    pub revoked: bool,
}
#[derive(Clone, Debug)]
pub struct StoredCredential {
    pub version: i64,
    pub ciphertext: Vec<u8>,
}
#[derive(Clone, Debug)]
pub struct OAuthAttempt {
    pub id: String,
    pub user_id: String,
    pub device_auth_id: String,
    pub user_code: String,
    pub verification_url: String,
    pub interval_seconds: i64,
    pub expires_at: i64,
    pub status: String,
}

#[async_trait]
pub trait Repository: Send + Sync {
    async fn users(&self) -> anyhow::Result<Vec<User>>;
    async fn user_by_id(&self, id: &str) -> anyhow::Result<Option<User>>;
    async fn create_user(&self, name: &str, hash: &str) -> anyhow::Result<User>;
    async fn set_key(&self, id: &str, hash: &str) -> anyhow::Result<()>;
    async fn revoke(&self, id: &str) -> anyhow::Result<()>;
    async fn credential(&self, id: &str) -> anyhow::Result<Option<StoredCredential>>;
    async fn put_credential(&self, id: &str, version: i64, ciphertext: &[u8])
    -> anyhow::Result<()>;
    async fn delete_credential(&self, id: &str) -> anyhow::Result<()>;
    async fn put_attempt(&self, a: &OAuthAttempt) -> anyhow::Result<()>;
    async fn attempt(&self, id: &str, user_id: &str) -> anyhow::Result<Option<OAuthAttempt>>;
    async fn finish_attempt(&self, id: &str, status: &str) -> anyhow::Result<bool>;
    async fn ready(&self) -> bool;
}

#[derive(Clone)]
pub struct SqliteRepository {
    pool: SqlitePool,
}
impl SqliteRepository {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let opts = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(opts)
            .await?;
        sqlx::migrate!().run(&pool).await?;
        Ok(Self { pool })
    }
}
fn user(r: sqlx::sqlite::SqliteRow) -> User {
    User {
        id: r.get("id"),
        name: r.get("name"),
        key_hash: r.get("key_hash"),
        revoked: r.get::<Option<i64>, _>("revoked_at").is_some(),
    }
}
#[async_trait]
impl Repository for SqliteRepository {
    async fn users(&self) -> anyhow::Result<Vec<User>> {
        Ok(
            sqlx::query("SELECT id,name,key_hash,revoked_at FROM users ORDER BY name")
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .map(user)
                .collect(),
        )
    }
    async fn user_by_id(&self, id: &str) -> anyhow::Result<Option<User>> {
        Ok(
            sqlx::query("SELECT id,name,key_hash,revoked_at FROM users WHERE id=?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?
                .map(user),
        )
    }
    async fn create_user(&self, name: &str, hash: &str) -> anyhow::Result<User> {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO users(id,name,key_hash) VALUES(?,?,?)")
            .bind(&id)
            .bind(name)
            .bind(hash)
            .execute(&self.pool)
            .await?;
        Ok(User {
            id,
            name: name.into(),
            key_hash: hash.into(),
            revoked: false,
        })
    }
    async fn set_key(&self, id: &str, hash: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE users SET key_hash=?,revoked_at=NULL WHERE id=?")
            .bind(hash)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    async fn revoke(&self, id: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE users SET revoked_at=unixepoch() WHERE id=?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    async fn credential(&self, id: &str) -> anyhow::Result<Option<StoredCredential>> {
        Ok(
            sqlx::query("SELECT version,ciphertext FROM openai_credentials WHERE user_id=?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?
                .map(|r| StoredCredential {
                    version: r.get(0),
                    ciphertext: r.get(1),
                }),
        )
    }
    async fn put_credential(&self, id: &str, v: i64, c: &[u8]) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO openai_credentials(user_id,version,ciphertext) VALUES(?,?,?) ON CONFLICT(user_id) DO UPDATE SET version=excluded.version,ciphertext=excluded.ciphertext,updated_at=unixepoch()").bind(id).bind(v).bind(c).execute(&self.pool).await?;
        Ok(())
    }
    async fn delete_credential(&self, id: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM openai_credentials WHERE user_id=?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    async fn put_attempt(&self, a: &OAuthAttempt) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE oauth_attempts SET status='cancelled' WHERE user_id=? AND status='pending'",
        )
        .bind(&a.user_id)
        .execute(&self.pool)
        .await?;
        sqlx::query("INSERT INTO oauth_attempts(id,user_id,device_auth_id,user_code,verification_url,interval_seconds,expires_at,status) VALUES(?,?,?,?,?,?,?,'pending')").bind(&a.id).bind(&a.user_id).bind(&a.device_auth_id).bind(&a.user_code).bind(&a.verification_url).bind(a.interval_seconds).bind(a.expires_at).execute(&self.pool).await?;
        Ok(())
    }
    async fn attempt(&self, id: &str, uid: &str) -> anyhow::Result<Option<OAuthAttempt>> {
        Ok(
            sqlx::query("SELECT * FROM oauth_attempts WHERE id=? AND user_id=?")
                .bind(id)
                .bind(uid)
                .fetch_optional(&self.pool)
                .await?
                .map(|r| OAuthAttempt {
                    id: r.get("id"),
                    user_id: r.get("user_id"),
                    device_auth_id: r.get("device_auth_id"),
                    user_code: r.get("user_code"),
                    verification_url: r.get("verification_url"),
                    interval_seconds: r.get("interval_seconds"),
                    expires_at: r.get("expires_at"),
                    status: r.get("status"),
                }),
        )
    }
    async fn finish_attempt(&self, id: &str, status: &str) -> anyhow::Result<bool> {
        Ok(
            sqlx::query("UPDATE oauth_attempts SET status=? WHERE id=? AND status='pending'")
                .bind(status)
                .bind(id)
                .execute(&self.pool)
                .await?
                .rows_affected()
                == 1,
        )
    }
    async fn ready(&self) -> bool {
        sqlx::query("SELECT 1").execute(&self.pool).await.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn migration_user_credential_and_attempt_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}?mode=rwc", dir.path().join("test.db").display());
        let repo = SqliteRepository::connect(&url).await.unwrap();
        assert!(repo.ready().await);
        let user = repo.create_user("alice", "hash").await.unwrap();
        repo.put_credential(&user.id, 2, b"ciphertext")
            .await
            .unwrap();
        let credential = repo.credential(&user.id).await.unwrap().unwrap();
        assert_eq!(credential.version, 2);
        assert_eq!(credential.ciphertext, b"ciphertext");
        let attempt = OAuthAttempt {
            id: "attempt".into(),
            user_id: user.id.clone(),
            device_auth_id: "device".into(),
            user_code: "CODE".into(),
            verification_url: "https://example.test".into(),
            interval_seconds: 5,
            expires_at: 100,
            status: "pending".into(),
        };
        repo.put_attempt(&attempt).await.unwrap();
        assert_eq!(
            repo.attempt("attempt", &user.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "pending"
        );
        assert!(repo.finish_attempt("attempt", "complete").await.unwrap());
        assert!(!repo.finish_attempt("attempt", "complete").await.unwrap());
        repo.revoke(&user.id).await.unwrap();
        assert!(repo.user_by_id(&user.id).await.unwrap().unwrap().revoked);
    }
}
