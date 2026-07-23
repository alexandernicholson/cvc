use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};
use cvc::{
    config::{Config, Model},
    crypto::{Tokens, Vault, hash_key},
    db::{Repository, SqliteRepository},
    oauth::OAuthClient,
    openai::{CodexClient, CodexOptions},
    server::{self, AppState},
};
use serde_json::{Value, json};
use tower::ServiceExt;

static UPSTREAM_CALLS: AtomicU64 = AtomicU64::new(0);
static RETRY_UPSTREAM_CALLS: AtomicU64 = AtomicU64::new(0);

fn successful_frames() -> String {
    let completed = json!({
        "type":"response.completed",
        "response":{
            "id":"resp_test",
            "model":"gpt-test",
            "status":"completed",
            "usage":{
                "input_tokens":17,
                "output_tokens":2,
                "input_tokens_details":{"cached_tokens":7,"cache_write_tokens":3}
            },
            "output":[{"type":"message","content":[{"type":"output_text","text":"hello"}]}]
        }
    });
    [
        json!({"type":"response.created","response":{"id":"resp_test","model":"gpt-test"}}),
        json!({"type":"response.output_item.added","item":{"type":"message","id":"item_1"}}),
        json!({"type":"response.output_text.delta","item_id":"item_1","delta":"hel"}),
        json!({"type":"response.output_text.delta","item_id":"item_1","delta":"lo"}),
        json!({"type":"response.output_item.done","item_id":"item_1"}),
        completed,
    ]
    .into_iter()
    .map(|value| format!("data: {value}\n\n"))
    .collect()
}

async fn codex_mock(headers: axum::http::HeaderMap, Json(body): Json<Value>) -> impl IntoResponse {
    assert_eq!(headers.get("chatgpt-account-id").unwrap(), "account-test");
    assert_eq!(headers.get("originator").unwrap(), "cvc");
    assert_eq!(body["model"], "gpt-test");
    assert_eq!(body["store"], false);
    assert_eq!(body["reasoning"]["effort"], "high");
    assert_eq!(body["max_output_tokens"], 32);
    assert_eq!(body["prompt_cache_key"].as_str().map(str::len), Some(64));
    UPSTREAM_CALLS.fetch_add(1, Ordering::SeqCst);
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        successful_frames(),
    )
}
async fn retry_mock(
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    assert_eq!(headers.get("chatgpt-account-id").unwrap(), "account-test");
    assert_eq!(body["model"], "gpt-test");
    let attempt = RETRY_UPSTREAM_CALLS.fetch_add(1, Ordering::SeqCst) + 1;
    if attempt <= 2 {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        successful_frames(),
    )
        .into_response()
}

async fn context_error_mock() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        format!(
            "data: {}\n\n",
            json!({
                "type": "error",
                "error": {
                    "code": "context_length_exceeded",
                    "type": "invalid_request_error",
                    "message": "sensitive upstream detail"
                }
            })
        ),
    )
}

async fn models_mock(headers: axum::http::HeaderMap) -> impl IntoResponse {
    assert_eq!(headers.get("chatgpt-account-id").unwrap(), "account-test");
    assert_eq!(headers.get("originator").unwrap(), "cvc");
    assert_eq!(headers.get("version").unwrap(), "test-client");
    Json(json!({
        "models": [{
            "slug": "gpt-test",
            "display_name": "Codex test",
            "supported_reasoning_levels": [{"effort": "high"}],
            "visibility": "list",
            "supported_in_api": true,
            "context_window": 1000
        }]
    }))
}

async fn fixture_with_upstream(
    path: &str,
    upstream: Router,
) -> (Router, tempfile::TempDir, String) {
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(upstream_listener, upstream).await.unwrap();
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
        upstream_url: format!("http://{upstream_addr}{path}"),
        upstream_models_url: format!("http://{upstream_addr}/models"),
        codex_client_version: "test-client".into(),
        model_cache_ttl_seconds: 300,
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
        CodexOptions {
            responses_url: config.upstream_url.clone(),
            models_url: config.upstream_models_url.clone(),
            client_version: config.codex_client_version.clone(),
            model_ttl: Duration::from_secs(config.model_cache_ttl_seconds),
            seed_models: config.models.clone(),
            concurrency: 2,
        },
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
async fn fixture_with_path(path: &str) -> (Router, tempfile::TempDir, String) {
    fixture_with_upstream(
        path,
        Router::new()
            .route("/responses", post(codex_mock))
            .route("/retry", post(retry_mock))
            .route("/models", get(models_mock))
            .route("/context-error", post(context_error_mock)),
    )
    .await
}
async fn fixture() -> (Router, tempfile::TempDir, String) {
    fixture_with_path("/responses").await
}

fn message(stream: bool, key: &str) -> Request<Body> {
    Request::builder().method("POST").uri("/v1/messages?beta=true")
        .header(header::AUTHORIZATION, format!("Bearer {key}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-claude-code-session-id", "session-test")
        .body(Body::from(json!({"model":"claude-codex-default","max_tokens":32,"stream":stream,"messages":[{"role":"user","content":"say hello"}],"output_config":{"effort":"high"},"future_beta_field":{"enabled":true}}).to_string())).unwrap()
}

fn token_count(key: &str, model: &str, text: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/messages/count_tokens?beta=true")
        .header(header::AUTHORIZATION, format!("Bearer {key}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({
                "model": model,
                "system": "Count the complete translated request.",
                "messages": [{"role": "user", "content": text}]
            })
            .to_string(),
        ))
        .unwrap()
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
    assert_eq!(
        value["usage"],
        json!({"input_tokens":7,"output_tokens":2,"cache_creation_input_tokens":3,"cache_read_input_tokens":7})
    );
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
    assert!(events.contains("\"cache_creation_input_tokens\":3"));
    assert!(events.contains("\"cache_read_input_tokens\":7"));
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
    let metric = |name: &str| {
        metrics
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name} ")))
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap()
    };
    assert!(metric("cvc_inference_requests_total") >= 2);
    assert!(metric("cvc_inference_responses_total") >= 2);
    assert!(metric("cvc_inference_in_flight") <= metric("cvc_inference_requests_total"));
    assert!(metric("cvc_input_tokens_total") >= 34);
    assert!(metric("cvc_uncached_input_tokens_total") >= 14);
    assert!(metric("cvc_output_tokens_total") >= 4);
    assert!(metric("cvc_cache_read_input_tokens_total") >= 14);
    assert!(metric("cvc_cache_creation_input_tokens_total") >= 6);
    assert!(metric("cvc_cache_hit_responses_total") >= 2);
    assert!(metrics.contains("# TYPE cvc_upstream_response_latency_seconds histogram"));
    assert!(metrics.contains("# TYPE cvc_inference_duration_seconds histogram"));
}

#[tokio::test]
async fn counts_translated_message_tokens_for_context_management() {
    let (app, _directory, key) = fixture().await;

    let unauthorized = app
        .clone()
        .oneshot(token_count("wrong", "claude-codex-default", "hello"))
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let short = app
        .clone()
        .oneshot(token_count(&key, "claude-codex-default", "hello"))
        .await
        .unwrap();
    assert_eq!(short.status(), StatusCode::OK);
    let short: Value =
        serde_json::from_slice(&to_bytes(short.into_body(), usize::MAX).await.unwrap()).unwrap();
    let short = short["input_tokens"].as_u64().unwrap();
    assert!(short > 0);

    let long_text = "context ".repeat(2_000);
    let long = app
        .clone()
        .oneshot(token_count(&key, "claude-codex-default", &long_text))
        .await
        .unwrap();
    assert_eq!(long.status(), StatusCode::OK);
    let long: Value =
        serde_json::from_slice(&to_bytes(long.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(long["input_tokens"].as_u64().unwrap() > short + 1_000);

    let unknown = app
        .oneshot(token_count(&key, "claude-unknown", "hello"))
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn retries_transient_upstream_failures_before_streaming() {
    RETRY_UPSTREAM_CALLS.store(0, Ordering::SeqCst);
    let (app, _directory, key) = fixture_with_path("/retry").await;

    let response = app.oneshot(message(false, &key)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(value["content"][0]["text"], "hello");
    assert_eq!(RETRY_UPSTREAM_CALLS.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn maps_context_window_failures_without_reporting_a_gateway_outage() {
    let (app, _directory, key) = fixture_with_path("/context-error").await;

    let nonstream = app.clone().oneshot(message(false, &key)).await.unwrap();
    assert_eq!(nonstream.status(), StatusCode::BAD_REQUEST);
    let error: Value =
        serde_json::from_slice(&to_bytes(nonstream.into_body(), usize::MAX).await.unwrap())
            .unwrap();
    assert_eq!(error["error"]["type"], "invalid_request_error");
    assert_eq!(
        error["error"]["message"],
        "request exceeds the upstream model context window; shorten or compact the conversation"
    );

    let streaming = app.oneshot(message(true, &key)).await.unwrap();
    assert_eq!(streaming.status(), StatusCode::OK);
    let events = String::from_utf8(
        to_bytes(streaming.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(events.contains("event: error"));
    assert!(events.contains("\"type\":\"invalid_request_error\""));
    assert!(events.contains("shorten or compact the conversation"));
    assert!(!events.contains("event: message_start"));
    assert!(!events.contains("sensitive upstream detail"));
}

#[tokio::test]
async fn caches_discovered_models_and_refreshes_after_model_rejection() {
    let model_calls = Arc::new(AtomicU64::new(0));
    let handler_calls = model_calls.clone();
    let upstream = Router::new()
        .route("/missing", post(|| async { StatusCode::NOT_FOUND }))
        .route(
            "/models",
            get(move |headers: axum::http::HeaderMap| {
                let calls = handler_calls.clone();
                async move {
                    assert_eq!(headers.get("chatgpt-account-id").unwrap(), "account-test");
                    assert_eq!(headers.get("originator").unwrap(), "cvc");
                    assert_eq!(headers.get("version").unwrap(), "test-client");
                    let call = calls.fetch_add(1, Ordering::SeqCst);
                    let mut models = vec![json!({
                        "slug": "gpt-new",
                        "display_name": "GPT New",
                        "supported_reasoning_levels": [{"effort": "low"}, {"effort": "high"}],
                        "visibility": "list",
                        "supported_in_api": true,
                        "context_window": 2000
                    })];
                    if call == 0 {
                        models.push(json!({
                            "slug": "gpt-test",
                            "display_name": "Codex test",
                            "supported_reasoning_levels": [{"effort": "high"}],
                            "visibility": "list",
                            "supported_in_api": true,
                            "context_window": 1000
                        }));
                    }
                    Json(json!({"models": models}))
                }
            }),
        );
    let (app, _directory, key) = fixture_with_upstream("/missing", upstream).await;
    let request = || {
        Request::get("/v1/models?limit=1000")
            .header(header::AUTHORIZATION, format!("Bearer {key}"))
            .body(Body::empty())
            .unwrap()
    };

    let first = app.clone().oneshot(request()).await.unwrap();
    let first: Value =
        serde_json::from_slice(&to_bytes(first.into_body(), usize::MAX).await.unwrap()).unwrap();
    let first_ids = first["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(first_ids, vec!["claude-codex-default", "claude-gpt-new"]);
    assert_eq!(first["data"][0]["context_window"], 1000);
    assert_eq!(first["data"][0]["max_output_tokens"], 100);
    assert_eq!(first["data"][1]["context_window"], 2000);
    assert_eq!(first["data"][1]["max_output_tokens"], 32000);

    let second = app.clone().oneshot(request()).await.unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(model_calls.load(Ordering::SeqCst), 1);

    let rejected = app.clone().oneshot(message(false, &key)).await.unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert_eq!(model_calls.load(Ordering::SeqCst), 2);

    let refreshed = app.oneshot(request()).await.unwrap();
    let refreshed: Value =
        serde_json::from_slice(&to_bytes(refreshed.into_body(), usize::MAX).await.unwrap())
            .unwrap();
    let refreshed_ids = refreshed["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(refreshed_ids, vec!["claude-gpt-new"]);
    assert_eq!(model_calls.load(Ordering::SeqCst), 2);
}
