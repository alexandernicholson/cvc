use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    response::IntoResponse,
    routing::post,
};
use cvc::{
    config::{Config, Model},
    crypto::{Tokens, Vault, hash_key},
    db::{Repository, SqliteRepository},
    oauth::OAuthClient,
    openai::CodexClient,
    server::{self, AppState},
};
use serde_json::{Value, json};
use tower::ServiceExt;

static UPSTREAM_CALLS: AtomicU64 = AtomicU64::new(0);

async fn codex_mock(headers: axum::http::HeaderMap, Json(body): Json<Value>) -> impl IntoResponse {
    assert_eq!(headers.get("chatgpt-account-id").unwrap(), "account-test");
    assert_eq!(headers.get("originator").unwrap(), "cvc");
    assert_eq!(body["model"], "gpt-test");
    assert_eq!(body["store"], false);
    assert_eq!(body["reasoning"]["effort"], "high");
    UPSTREAM_CALLS.fetch_add(1, Ordering::SeqCst);
    let completed = json!({
        "type":"response.completed",
        "response":{
            "id":"resp_test",
            "model":"gpt-test",
            "status":"completed",
            "usage":{"input_tokens":7,"output_tokens":2},
            "output":[{"type":"message","content":[{"type":"output_text","text":"hello"}]}]
        }
    });
    let frames = [
        json!({"type":"response.created","response":{"id":"resp_test","model":"gpt-test"}}),
        json!({"type":"response.output_item.added","item":{"type":"message","id":"item_1"}}),
        json!({"type":"response.output_text.delta","item_id":"item_1","delta":"hel"}),
        json!({"type":"response.output_text.delta","item_id":"item_1","delta":"lo"}),
        json!({"type":"response.output_item.done","item_id":"item_1"}),
        completed,
    ]
    .into_iter()
    .map(|value| format!("data: {value}\n\n"))
    .collect::<String>();
    ([(header::CONTENT_TYPE, "text/event-stream")], frames)
}

async fn fixture() -> (Router, tempfile::TempDir, String) {
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            upstream_listener,
            Router::new().route("/responses", post(codex_mock)),
        )
        .await
        .unwrap();
    });

    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("cvc.db").display()
    );
    let repository = Arc::new(SqliteRepository::connect(&database_url).await.unwrap());
    let gateway_key = "cvc_local_machine_test".to_owned();
    let user = repository
        .create_user("local-test", &hash_key(&gateway_key).unwrap())
        .await
        .unwrap();
    let vault = Vault::new([42; 32]);
    let tokens = Tokens {
        access_token: "access-test".into(),
        refresh_token: "refresh-test".into(),
        id_token: "id-test".into(),
        expires_at: i64::MAX,
        account_id: "account-test".into(),
    };
    repository
        .put_credential(&user.id, 1, &vault.encrypt(&user.id, 1, &tokens).unwrap())
        .await
        .unwrap();

    let model = Model {
        alias: "claude-codex-default".into(),
        display_name: "Codex test".into(),
        upstream: "gpt-test".into(),
        efforts: vec!["high".into()],
        context_limit: 1000,
        output_limit: 100,
        structured_output: true,
    };
    let config = Arc::new(Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        database_url,
        public_url: None,
        trusted_proxy: false,
        master_key: [42; 32],
        models: HashMap::from([(model.alias.clone(), model)]),
        default_model: "claude-codex-default".into(),
        upstream_url: format!("http://{upstream_addr}/responses"),
        oauth_issuer: "http://127.0.0.1:1".into(),
        oauth_client_id: "test".into(),
        max_body_bytes: 1024 * 1024,
        per_user_concurrency: 2,
    });
    let repo: Arc<dyn Repository> = repository;
    let oauth = OAuthClient::new(config.oauth_issuer.clone(), config.oauth_client_id.clone());
    let codex = CodexClient::new(
        repo.clone(),
        vault.clone(),
        oauth.clone(),
        config.upstream_url.clone(),
        2,
    );
    (
        server::router(AppState {
            config,
            repo,
            oauth,
            codex,
            vault,
        }),
        directory,
        gateway_key,
    )
}

fn message(stream: bool, key: &str) -> Request<Body> {
    Request::builder().method("POST").uri("/v1/messages?beta=true")
        .header(header::AUTHORIZATION, format!("Bearer {key}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"model":"claude-codex-default","max_tokens":32,"stream":stream,"messages":[{"role":"user","content":"say hello"}],"output_config":{"effort":"high"},"future_beta_field":{"enabled":true}}).to_string())).unwrap()
}

#[tokio::test]
async fn live_endpoints_and_mock_inference_work_end_to_end() {
    UPSTREAM_CALLS.store(0, Ordering::SeqCst);
    let (app, _directory, key) = fixture().await;

    let health = app
        .clone()
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    let ready = app
        .clone()
        .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(ready.status(), StatusCode::OK);
    let unauthorized = app
        .clone()
        .oneshot(
            Request::get("/v1/models?limit=1000")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let nonstream = app.clone().oneshot(message(false, &key)).await.unwrap();
    assert_eq!(nonstream.status(), StatusCode::OK);
    let value: Value =
        serde_json::from_slice(&to_bytes(nonstream.into_body(), usize::MAX).await.unwrap())
            .unwrap();
    assert_eq!(value["content"][0]["text"], "hello");
    assert_eq!(value["usage"], json!({"input_tokens":7,"output_tokens":2}));
    assert_eq!(value["stop_reason"], "end_turn");

    let streaming = app.clone().oneshot(message(true, &key)).await.unwrap();
    assert_eq!(streaming.status(), StatusCode::OK);
    assert_eq!(streaming.headers().get("x-accel-buffering").unwrap(), "no");
    let events = String::from_utf8(
        to_bytes(streaming.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    for expected in [
        "event: message_start",
        "event: content_block_start",
        "event: content_block_delta",
        "event: content_block_stop",
        "event: message_delta",
        "event: message_stop",
    ] {
        assert!(events.contains(expected), "missing {expected} in {events}");
    }
    assert!(events.contains("hel"));
    assert!(events.contains("lo"));
    assert_eq!(UPSTREAM_CALLS.load(Ordering::SeqCst), 2);

    let metrics = app
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let metrics = String::from_utf8(
        to_bytes(metrics.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(metrics.contains("cvc_inference_requests_total 2"));
}
