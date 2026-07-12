use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use cvc::{
    config::Config,
    crypto::{Vault, hash_key, issue_gateway_key},
    db::{Repository, SqliteRepository},
    oauth::OAuthClient,
    openai::CodexClient,
    server::{self, AppState},
};
use std::sync::Arc;
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Serve,
    Login {
        #[arg(long)]
        server: String,
        #[arg(long)]
        token: String,
    },
    Admin {
        #[command(subcommand)]
        command: Admin,
    },
}
#[derive(Subcommand)]
enum Admin {
    User {
        #[command(subcommand)]
        command: UserCommand,
    },
    Openai {
        #[command(subcommand)]
        command: OpenAiCommand,
    },
}
#[derive(Subcommand)]
enum UserCommand {
    Create { name: String },
    Revoke { user: String },
    Rotate { user: String },
    List,
}
#[derive(Subcommand)]
enum OpenAiCommand {
    Disconnect { user: String },
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    match Cli::parse().command {
        Command::Login { server, token } => login(&server, &token).await,
        cmd => {
            let cfg = Arc::new(Config::from_env()?);
            let repo: Arc<dyn Repository> =
                Arc::new(SqliteRepository::connect(&cfg.database_url).await?);
            match cmd {
                Command::Serve => serve(cfg, repo).await,
                Command::Admin { command } => admin(repo, command).await,
                _ => unreachable!(),
            }
        }
    }
}
async fn serve(cfg: Arc<Config>, repo: Arc<dyn Repository>) -> anyhow::Result<()> {
    let oauth = OAuthClient::new(cfg.oauth_issuer.clone(), cfg.oauth_client_id.clone());
    let vault = Vault::new(cfg.master_key);
    let codex = CodexClient::new(
        repo.clone(),
        vault.clone(),
        oauth.clone(),
        cfg.upstream_url.clone(),
        cfg.per_user_concurrency,
    );
    let app = server::router(AppState {
        config: cfg.clone(),
        repo,
        oauth,
        codex,
        vault,
    });
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    tracing::info!(address=%cfg.bind,"server listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
async fn admin(repo: Arc<dyn Repository>, cmd: Admin) -> anyhow::Result<()> {
    match cmd {
        Admin::User { command } => match command {
            UserCommand::Create { name } => {
                let key = issue_gateway_key();
                let hash = tokio::task::spawn_blocking({
                    let k = key.clone();
                    move || hash_key(&k)
                })
                .await??;
                let u = repo.create_user(&name, &hash).await?;
                println!(
                    "user: {}\nid: {}\ngateway key (shown once): {}",
                    u.name, u.id, key
                )
            }
            UserCommand::List => {
                for u in repo.users().await? {
                    println!(
                        "{}\t{}\t{}",
                        u.id,
                        u.name,
                        if u.revoked { "revoked" } else { "active" }
                    )
                }
            }
            UserCommand::Revoke { user } => {
                let u = find_user(&*repo, &user).await?;
                repo.revoke(&u.id).await?
            }
            UserCommand::Rotate { user } => {
                let u = find_user(&*repo, &user).await?;
                let key = issue_gateway_key();
                let hash = tokio::task::spawn_blocking({
                    let k = key.clone();
                    move || hash_key(&k)
                })
                .await??;
                repo.set_key(&u.id, &hash).await?;
                println!("gateway key (shown once): {key}")
            }
        },
        Admin::Openai {
            command: OpenAiCommand::Disconnect { user },
        } => {
            let u = find_user(&*repo, &user).await?;
            repo.delete_credential(&u.id).await?
        }
    }
    Ok(())
}
async fn find_user(repo: &dyn Repository, s: &str) -> anyhow::Result<cvc::db::User> {
    repo.users()
        .await?
        .into_iter()
        .find(|u| u.id == s || u.name == s)
        .with_context(|| format!("user '{s}' not found"))
}
async fn login(server: &str, token: &str) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let base = server.trim_end_matches('/');
    let start = client
        .post(format!("{base}/auth/device/start"))
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?
        .json::<serde_json::Value>()
        .await?;
    println!(
        "Open {} and enter {}",
        start["verification_url"].as_str().unwrap_or(""),
        start["user_code"].as_str().unwrap_or("")
    );
    let id = start["id"]
        .as_str()
        .context("server omitted device attempt ID")?;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let v = client
            .get(format!("{base}/auth/device/{id}"))
            .bearer_auth(token)
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;
        match v["status"].as_str() {
            Some("complete") => {
                println!("OpenAI account connected");
                return Ok(());
            }
            Some("pending") => {}
            Some(s) => bail!("login {s}"),
            None => bail!("invalid login response"),
        }
    }
}
