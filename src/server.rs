use crate::{
    auth::{AuthState, Caller, authenticate},
    config::Config,
    crypto::Vault,
    db::{OAuthAttempt, Repository},
    error::ApiError,
    oauth::{OAuthClient, now},
    openai::CodexClient,
    protocol::MessageRequest,
    stream::Machine,
    translate,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderValue, StatusCode},
    middleware,
    response::{IntoResponse, Response, Sse},
    routing::{delete, get, head, post},
};
use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tower_http::{
    limit::RequestBodyLimitLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};
static INFERENCE_REQUESTS: AtomicU64 = AtomicU64::new(0);
static INFERENCE_ERRORS: AtomicU64 = AtomicU64::new(0);
static LATENCY_BUCKETS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
const LATENCY_BOUNDS_MS: [u64; 5] = [100, 500, 1_000, 5_000, u64::MAX];
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub repo: Arc<dyn Repository>,
    pub oauth: OAuthClient,
    pub codex: CodexClient,
    pub vault: Vault,
}
pub fn router(s: AppState) -> Router {
    let protected = Router::new()
        .route("/auth/device/start", post(device_start))
        .route(
            "/auth/device/{id}",
            get(device_status).delete(device_cancel),
        )
        .route("/auth/openai", delete(disconnect))
        .route("/v1/models", get(models))
        .route("/v1/messages", post(messages))
        .layer(middleware::from_fn(authenticate));
    Router::new()
        .route("/", head(|| async { StatusCode::OK }))
        .route("/healthz", get(|| async { Json(json!({"status":"ok"})) }))
        .route("/readyz", get(ready))
        .route("/metrics", get(metrics))
        .merge(protected)
        .layer(TraceLayer::new_for_http())
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(RequestBodyLimitLayer::new(s.config.max_body_bytes))
        .layer(axum::Extension(AuthState {
            repo: s.repo.clone(),
        }))
        .with_state(s)
}
async fn ready(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    if s.repo.ready().await {
        Ok(Json(json!({"status":"ready"})))
    } else {
        Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            "database unavailable",
        ))
    }
}
async fn metrics() -> impl IntoResponse {
    let counts = LATENCY_BUCKETS
        .iter()
        .map(|v| v.load(Ordering::Relaxed))
        .collect::<Vec<_>>();
    let cumulative = counts
        .iter()
        .scan(0u64, |sum, value| {
            *sum += value;
            Some(*sum)
        })
        .collect::<Vec<_>>();
    let body = format!(
        "# TYPE cvc_inference_requests_total counter\ncvc_inference_requests_total {}\n# TYPE cvc_inference_errors_total counter\ncvc_inference_errors_total {}\n# TYPE cvc_inference_latency_seconds histogram\ncvc_inference_latency_seconds_bucket{{le=\"0.1\"}} {}\ncvc_inference_latency_seconds_bucket{{le=\"0.5\"}} {}\ncvc_inference_latency_seconds_bucket{{le=\"1\"}} {}\ncvc_inference_latency_seconds_bucket{{le=\"5\"}} {}\ncvc_inference_latency_seconds_bucket{{le=\"+Inf\"}} {}\ncvc_inference_latency_seconds_count {}\n",
        INFERENCE_REQUESTS.load(Ordering::Relaxed),
        INFERENCE_ERRORS.load(Ordering::Relaxed),
        cumulative[0],
        cumulative[1],
        cumulative[2],
        cumulative[3],
        cumulative[4],
        cumulative[4],
    );
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
}
async fn models(State(s): State<AppState>) -> Json<Value> {
    let mut ms = s.config.models.values().collect::<Vec<_>>();
    ms.sort_by_key(|m| &m.alias);
    Json(
        json!({"data":ms.into_iter().map(|m|json!({"id":m.alias,"type":"model","display_name":m.display_name,"created_at":"2026-01-01T00:00:00Z"})).collect::<Vec<_>>(),"has_more":false,"first_id":null,"last_id":null}),
    )
}
async fn disconnect(State(s): State<AppState>, Caller(u): Caller) -> Result<StatusCode, ApiError> {
    s.repo
        .delete_credential(&u.id)
        .await
        .map_err(|_| ApiError::server("credential store unavailable"))?;
    Ok(StatusCode::NO_CONTENT)
}
async fn device_start(
    State(s): State<AppState>,
    Caller(u): Caller,
) -> Result<Json<Value>, ApiError> {
    let d = s.oauth.start().await.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "OpenAI device authorization unavailable",
        )
    })?;
    let a = OAuthAttempt {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: u.id.clone(),
        device_auth_id: d.device_auth_id,
        user_code: d.user_code.clone(),
        verification_url: s.oauth.verification_url(),
        interval_seconds: d.interval as i64,
        expires_at: now() + 900,
        status: "pending".into(),
    };
    s.repo
        .put_attempt(&a)
        .await
        .map_err(|_| ApiError::server("could not store device authorization"))?;
    let state = s.clone();
    let attempt = a.clone();
    tokio::spawn(async move {
        poll_device(state, attempt).await;
    });
    Ok(Json(
        json!({"id":a.id,"verification_url":a.verification_url,"user_code":a.user_code,"expires_in":900,"interval":a.interval_seconds}),
    ))
}
async fn poll_device(s: AppState, a: OAuthAttempt) {
    loop {
        if now() >= a.expires_at {
            let _ = s.repo.finish_attempt(&a.id, "failed").await;
            return;
        }
        tokio::time::sleep(Duration::from_secs(a.interval_seconds.max(1) as u64)).await;
        let Ok(Some(current)) = s.repo.attempt(&a.id, &a.user_id).await else {
            return;
        };
        if current.status != "pending" {
            return;
        }
        match s.oauth.poll_once(&a.device_auth_id, &a.user_code).await {
            Ok(Some(c)) => {
                if let Ok(t) = s.oauth.exchange(c).await {
                    let version = 1;
                    if let Ok(encrypted) = s.vault.encrypt(&a.user_id, version, &t)
                        && s.repo
                            .put_credential(&a.user_id, version, &encrypted)
                            .await
                            .is_ok()
                    {
                        let _ = s.repo.finish_attempt(&a.id, "complete").await;
                        return;
                    }
                }
                let _ = s.repo.finish_attempt(&a.id, "failed").await;
                return;
            }
            Ok(None) => {}
            Err(_) => {
                let _ = s.repo.finish_attempt(&a.id, "failed").await;
                return;
            }
        }
    }
}
async fn device_status(
    State(s): State<AppState>,
    Caller(u): Caller,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let a = s
        .repo
        .attempt(&id, &u.id)
        .await
        .map_err(|_| ApiError::server("device authorization unavailable"))?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "not_found_error",
                "device authorization not found",
            )
        })?;
    Ok(Json(
        json!({"id":a.id,"status":a.status,"expires_at":a.expires_at}),
    ))
}
async fn device_cancel(
    State(s): State<AppState>,
    Caller(u): Caller,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if s.repo
        .attempt(&id, &u.id)
        .await
        .map_err(|_| ApiError::server("device authorization unavailable"))?
        .is_none()
    {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "device authorization not found",
        ));
    }
    s.repo
        .finish_attempt(&id, "cancelled")
        .await
        .map_err(|_| ApiError::server("device authorization unavailable"))?;
    Ok(StatusCode::NO_CONTENT)
}
async fn messages(
    State(s): State<AppState>,
    Caller(u): Caller,
    Json(r): Json<MessageRequest>,
) -> Result<Response, ApiError> {
    INFERENCE_REQUESTS.fetch_add(1, Ordering::Relaxed);
    let started = Instant::now();
    let model = s
        .config
        .models
        .get(&r.model)
        .ok_or_else(|| ApiError::validation(format!("unknown model '{}'", r.model)))?;
    let body = translate::request(&r, model)?;
    let upstream = match s.codex.responses(&u.id, body).await {
        Ok(value) => value,
        Err(error) => {
            INFERENCE_ERRORS.fetch_add(1, Ordering::Relaxed);
            record_latency(started.elapsed());
            return Err(error);
        }
    };
    record_latency(started.elapsed());
    if r.stream {
        let stream = anthropic_stream(upstream.bytes_stream());
        let mut response = Sse::new(stream)
            .keep_alive(axum::response::sse::KeepAlive::new().interval(Duration::from_secs(15)))
            .into_response();
        response
            .headers_mut()
            .insert("x-accel-buffering", HeaderValue::from_static("no"));
        Ok(response)
    } else {
        match nonstream(upstream.bytes_stream(), &r.model).await {
            Ok(response) => Ok(response),
            Err(error) => {
                INFERENCE_ERRORS.fetch_add(1, Ordering::Relaxed);
                Err(error)
            }
        }
    }
}
fn record_latency(elapsed: Duration) {
    let millis = elapsed.as_millis() as u64;
    let index = LATENCY_BOUNDS_MS
        .iter()
        .position(|bound| millis <= *bound)
        .unwrap_or(4);
    LATENCY_BUCKETS[index].fetch_add(1, Ordering::Relaxed);
}
fn anthropic_stream<S>(
    input: S,
) -> impl Stream<Item = Result<axum::response::sse::Event, Infallible>>
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    async_stream::stream! {
        let mut input = Box::pin(input);
        let mut buffer = Vec::<u8>::new();
        let mut machine = Machine::default();
        while let Some(chunk) = input.next().await {
            match chunk {
                Ok(bytes) => {
                    buffer.extend_from_slice(&bytes);
                    while let Some(end) = buffer.windows(2).position(|w| w == b"\n\n") {
                        let frame = buffer.drain(..end + 2).collect::<Vec<_>>();
                        let Ok(frame) = std::str::from_utf8(&frame) else { yield Ok(stream_error("upstream emitted invalid UTF-8")); return; };
                        let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) else { continue };
                        if data == "[DONE]" { continue }
                        if let Ok(value) = serde_json::from_str::<Value>(data) {
                            match machine.apply(&value) {
                                Ok(events) => for event in events { yield Ok(event) },
                                Err(error) => { yield Ok(axum::response::sse::Event::default().event("error").json_data(json!({"type":"error","error":{"type":error.kind,"message":error.message}})).unwrap()); return; }
                            }
                        }
                    }
                }
                Err(_) => { yield Ok(stream_error("upstream stream disconnected")); return; }
            }
        }
    }
}
fn stream_error(message: &str) -> axum::response::sse::Event {
    axum::response::sse::Event::default()
        .event("error")
        .json_data(json!({"type":"error","error":{"type":"api_error","message":message}}))
        .unwrap()
}
async fn nonstream<S>(input: S, model: &str) -> Result<Response, ApiError>
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>>,
{
    let mut input = Box::pin(input);
    let mut buf = Vec::new();
    while let Some(c) = input.next().await {
        buf.extend(c.map_err(|_| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "upstream stream disconnected",
            )
        })?)
    }
    let text = String::from_utf8_lossy(&buf);
    let mut content = Vec::new();
    let mut text_output = String::new();
    let mut thinking_output = String::new();
    let mut thinking_signature = None;
    let mut calls: HashMap<String, (String, String)> = HashMap::new();
    let mut call_order = Vec::new();
    let mut usage = json!({"input_tokens":0,"output_tokens":0});
    let mut stop = "end_turn";
    for data in text
        .split("\n\n")
        .filter_map(|f| f.lines().find_map(|l| l.strip_prefix("data: ")))
    {
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") => text_output.push_str(
                event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
            Some("response.reasoning_text.delta")
            | Some("response.reasoning_summary_text.delta") => thinking_output.push_str(
                event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
            Some("response.output_item.added")
                if event.pointer("/item/type").and_then(Value::as_str) == Some("function_call") =>
            {
                let item = &event["item"];
                let id = item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("call")
                    .to_owned();
                call_order.push(id.clone());
                calls.insert(
                    id,
                    (
                        item.get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        String::new(),
                    ),
                );
            }
            Some("response.function_call_arguments.delta") => {
                if let Some((_, arguments)) = calls.get_mut(
                    event
                        .get("item_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                ) {
                    arguments.push_str(
                        event
                            .get("delta")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    );
                }
            }
            Some("response.output_item.done")
                if event.pointer("/item/type").and_then(Value::as_str) == Some("reasoning") =>
            {
                thinking_signature = serde_json::to_string(&event["item"]).ok();
            }
            Some("response.output_item.done")
                if event.pointer("/item/type").and_then(Value::as_str) == Some("function_call") =>
            {
                let item = &event["item"];
                let id = item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some((name, arguments)) = calls.get_mut(id) {
                    if let Some(final_name) = item.get("name").and_then(Value::as_str) {
                        *name = final_name.to_owned();
                    }
                    if let Some(final_arguments) = item.get("arguments").and_then(Value::as_str) {
                        *arguments = final_arguments.to_owned();
                    }
                }
            }
            Some("response.completed") => {
                usage = json!({"input_tokens":event.pointer("/response/usage/input_tokens").and_then(Value::as_u64).unwrap_or(0),"output_tokens":event.pointer("/response/usage/output_tokens").and_then(Value::as_u64).unwrap_or(0)});
            }
            Some(event_type @ ("response.failed" | "error")) => {
                let upstream_error_code = event
                    .pointer("/response/error/code")
                    .or_else(|| event.pointer("/error/code"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let upstream_error_type = event
                    .pointer("/response/error/type")
                    .or_else(|| event.pointer("/error/type"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                tracing::warn!(
                    model,
                    upstream_event = event_type,
                    upstream_error_code,
                    upstream_error_type,
                    "Codex upstream reported an in-stream failure"
                );
                return Err(ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "Codex upstream stream failed",
                ));
            }
            _ => {}
        }
    }
    if !thinking_output.is_empty() || thinking_signature.is_some() {
        content.push(json!({"type":"thinking","thinking":thinking_output,"signature":thinking_signature.unwrap_or_default()}));
    }
    if !text_output.is_empty() {
        content.push(json!({"type":"text","text":text_output}));
    }
    for id in call_order {
        if let Some((name, arguments)) = calls.remove(&id) {
            stop = "tool_use";
            let input = serde_json::from_str::<Value>(&arguments)
                .map_err(|_| ApiError::server("upstream tool arguments are not valid JSON"))?;
            content.push(json!({"type":"tool_use","id":id,"name":name,"input":input}));
        }
    }
    Ok(Json(json!({"id":format!("msg_{}",uuid::Uuid::new_v4().simple()),"type":"message","role":"assistant","model":model,"content":content,"stop_reason":stop,"stop_sequence":null,"usage":usage})).into_response())
}
