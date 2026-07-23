use crate::{
    auth::{AuthState, Caller, authenticate},
    config::Config,
    crypto::Vault,
    db::{OAuthAttempt, Repository},
    error::{ApiError, from_upstream_event},
    oauth::{OAuthClient, now},
    openai::CodexClient,
    protocol::{MessageRequest, Usage},
    stream::Machine,
    translate,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware,
    response::{IntoResponse, Response, Sse},
    routing::{delete, get, head, post},
};
use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{
        Arc, LazyLock,
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
static INFERENCE_RESPONSES: AtomicU64 = AtomicU64::new(0);
static INFERENCE_ERRORS: AtomicU64 = AtomicU64::new(0);
static INFERENCE_IN_FLIGHT: AtomicU64 = AtomicU64::new(0);
static INPUT_TOKENS: AtomicU64 = AtomicU64::new(0);
static UNCACHED_INPUT_TOKENS: AtomicU64 = AtomicU64::new(0);
static OUTPUT_TOKENS: AtomicU64 = AtomicU64::new(0);
static CACHE_READ_INPUT_TOKENS: AtomicU64 = AtomicU64::new(0);
static CACHE_CREATION_INPUT_TOKENS: AtomicU64 = AtomicU64::new(0);
static CACHE_HIT_RESPONSES: AtomicU64 = AtomicU64::new(0);
static UPSTREAM_LATENCY_BUCKETS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
static UPSTREAM_LATENCY_SUM_MS: AtomicU64 = AtomicU64::new(0);
static INFERENCE_DURATION_BUCKETS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
static INFERENCE_DURATION_SUM_MS: AtomicU64 = AtomicU64::new(0);
const LATENCY_BOUNDS_MS: [u64; 8] = [100, 500, 1_000, 5_000, 15_000, 30_000, 60_000, u64::MAX];
static INPUT_TOKENIZER: LazyLock<tiktoken_rs::CoreBPE> =
    LazyLock::new(|| tiktoken_rs::o200k_base().expect("embedded o200k tokenizer must be valid"));
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
        .route("/v1/messages/count_tokens", post(count_tokens))
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
    let mut body = String::with_capacity(4_096);
    write_counter(
        &mut body,
        "cvc_inference_requests_total",
        "Authenticated inference requests accepted by the gateway.",
        &INFERENCE_REQUESTS,
    );
    write_counter(
        &mut body,
        "cvc_inference_responses_total",
        "Inference responses completed by the upstream.",
        &INFERENCE_RESPONSES,
    );
    write_counter(
        &mut body,
        "cvc_inference_errors_total",
        "Inference requests that failed before or during the upstream response.",
        &INFERENCE_ERRORS,
    );
    writeln!(
        body,
        "# HELP cvc_inference_in_flight Inference requests whose response body is still active.\n# TYPE cvc_inference_in_flight gauge\ncvc_inference_in_flight {}",
        INFERENCE_IN_FLIGHT.load(Ordering::Relaxed)
    )
    .unwrap();
    for (name, help, value) in [
        (
            "cvc_input_tokens_total",
            "Total upstream input tokens, including cache reads and cache writes.",
            &INPUT_TOKENS,
        ),
        (
            "cvc_uncached_input_tokens_total",
            "Input tokens processed without a reported cache read or cache write.",
            &UNCACHED_INPUT_TOKENS,
        ),
        (
            "cvc_output_tokens_total",
            "Output tokens generated by the upstream.",
            &OUTPUT_TOKENS,
        ),
        (
            "cvc_cache_read_input_tokens_total",
            "Input tokens served from the upstream prompt cache.",
            &CACHE_READ_INPUT_TOKENS,
        ),
        (
            "cvc_cache_creation_input_tokens_total",
            "Input tokens reported as written to the upstream prompt cache.",
            &CACHE_CREATION_INPUT_TOKENS,
        ),
        (
            "cvc_cache_hit_responses_total",
            "Completed responses with at least one cached input token.",
            &CACHE_HIT_RESPONSES,
        ),
    ] {
        write_counter(&mut body, name, help, value);
    }
    write_histogram(
        &mut body,
        "cvc_upstream_response_latency_seconds",
        "Time from accepting a request until the upstream response headers arrive.",
        &UPSTREAM_LATENCY_BUCKETS,
        &UPSTREAM_LATENCY_SUM_MS,
    );
    write_histogram(
        &mut body,
        "cvc_inference_duration_seconds",
        "Time from accepting a request until a successful upstream completion event.",
        &INFERENCE_DURATION_BUCKETS,
        &INFERENCE_DURATION_SUM_MS,
    );
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
}

fn write_counter(body: &mut String, name: &str, help: &str, value: &AtomicU64) {
    writeln!(
        body,
        "# HELP {name} {help}\n# TYPE {name} counter\n{name} {}",
        value.load(Ordering::Relaxed)
    )
    .unwrap();
}

fn write_histogram(
    body: &mut String,
    name: &str,
    help: &str,
    buckets: &[AtomicU64; 8],
    sum_ms: &AtomicU64,
) {
    writeln!(body, "# HELP {name} {help}\n# TYPE {name} histogram").unwrap();
    let mut cumulative = 0;
    for (bound, bucket) in LATENCY_BOUNDS_MS.iter().zip(buckets) {
        cumulative += bucket.load(Ordering::Relaxed);
        if *bound == u64::MAX {
            writeln!(body, "{name}_bucket{{le=\"+Inf\"}} {cumulative}").unwrap();
        } else {
            writeln!(
                body,
                "{name}_bucket{{le=\"{}\"}} {cumulative}",
                *bound as f64 / 1_000.0
            )
            .unwrap();
        }
    }
    writeln!(
        body,
        "{name}_sum {}\n{name}_count {cumulative}",
        sum_ms.load(Ordering::Relaxed) as f64 / 1_000.0
    )
    .unwrap();
}
async fn models(State(s): State<AppState>, Caller(user): Caller) -> Json<Value> {
    let catalog = s.codex.models(&user.id, false).await;
    let mut models = catalog.values().collect::<Vec<_>>();
    models.sort_by_key(|model| &model.alias);
    Json(
        json!({"data":models.into_iter().map(|model|json!({"id":model.alias,"type":"model","display_name":model.display_name,"created_at":"2026-01-01T00:00:00Z","context_window":model.context_limit,"max_output_tokens":model.output_limit})).collect::<Vec<_>>(),"has_more":false,"first_id":null,"last_id":null}),
    )
}
async fn count_tokens(
    State(s): State<AppState>,
    Caller(user): Caller,
    Json(mut body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| ApiError::validation("token count request must be a JSON object"))?;
    object.entry("max_tokens").or_insert_with(|| json!(1));
    object.insert("stream".into(), json!(false));
    let request: MessageRequest = serde_json::from_value(body)
        .map_err(|_| ApiError::validation("invalid message token count request"))?;
    let catalog = s.codex.models(&user.id, false).await;
    let model = catalog
        .get(&request.model)
        .ok_or_else(|| ApiError::validation(format!("unknown model '{}'", request.model)))?;
    let translated = translate::request(&request, model)?;
    let encoded = serde_json::to_string(&translated)
        .map_err(|_| ApiError::server("token count serialization failed"))?;
    let input_tokens = INPUT_TOKENIZER.encode_with_special_tokens(&encoded).len();
    Ok(Json(json!({"input_tokens": input_tokens})))
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
fn prompt_cache_key(headers: &HeaderMap, user_id: &str) -> Option<String> {
    let session_id = headers
        .get("x-claude-code-session-id")?
        .to_str()
        .ok()?
        .trim();
    if session_id.is_empty() {
        return None;
    }
    let agent_id = headers
        .get("x-claude-code-agent-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_default();
    let mut hash = Sha256::new();
    hash.update(b"cvc:prompt-cache:v1");
    for value in [user_id, session_id, agent_id] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    Some(format!("{:x}", hash.finalize()))
}

async fn messages(
    State(s): State<AppState>,
    Caller(u): Caller,
    headers: HeaderMap,
    Json(r): Json<MessageRequest>,
) -> Result<Response, ApiError> {
    let guard = InFlightGuard::start();
    let started = Instant::now();
    let catalog = s.codex.models(&u.id, false).await;
    let model = catalog
        .get(&r.model)
        .ok_or_else(|| ApiError::validation(format!("unknown model '{}'", r.model)))?;
    let mut body = translate::request(&r, model)?;
    if let Some(key) = prompt_cache_key(&headers, &u.id) {
        body["prompt_cache_key"] = json!(key);
    }
    let upstream = match s.codex.responses(&u.id, body).await {
        Ok(value) => value,
        Err(error) => {
            INFERENCE_ERRORS.fetch_add(1, Ordering::Relaxed);
            record_histogram(
                &UPSTREAM_LATENCY_BUCKETS,
                &UPSTREAM_LATENCY_SUM_MS,
                started.elapsed(),
            );
            return Err(error);
        }
    };
    record_histogram(
        &UPSTREAM_LATENCY_BUCKETS,
        &UPSTREAM_LATENCY_SUM_MS,
        started.elapsed(),
    );
    if r.stream {
        let stream = anthropic_stream(
            upstream.bytes_stream(),
            model.upstream.clone(),
            started,
            guard,
        );
        let mut response = Sse::new(stream)
            .keep_alive(axum::response::sse::KeepAlive::new().interval(Duration::from_secs(15)))
            .into_response();
        response
            .headers_mut()
            .insert("x-accel-buffering", HeaderValue::from_static("no"));
        Ok(response)
    } else {
        match nonstream(upstream.bytes_stream(), &model.upstream, started).await {
            Ok(response) => Ok(response),
            Err(error) => {
                INFERENCE_ERRORS.fetch_add(1, Ordering::Relaxed);
                Err(error)
            }
        }
    }
}
struct InFlightGuard;

impl InFlightGuard {
    fn start() -> Self {
        INFERENCE_REQUESTS.fetch_add(1, Ordering::Relaxed);
        INFERENCE_IN_FLIGHT.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        INFERENCE_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
    }
}

fn record_histogram(buckets: &[AtomicU64; 8], sum_ms: &AtomicU64, elapsed: Duration) {
    let millis = elapsed.as_millis() as u64;
    let index = LATENCY_BOUNDS_MS
        .iter()
        .position(|bound| millis <= *bound)
        .unwrap_or(LATENCY_BOUNDS_MS.len() - 1);
    buckets[index].fetch_add(1, Ordering::Relaxed);
    sum_ms.fetch_add(millis, Ordering::Relaxed);
}

fn record_completion(elapsed: Duration, usage: &Usage) {
    INFERENCE_RESPONSES.fetch_add(1, Ordering::Relaxed);
    let input_tokens = usage
        .input_tokens
        .saturating_add(usage.cache_read_input_tokens)
        .saturating_add(usage.cache_creation_input_tokens);
    INPUT_TOKENS.fetch_add(input_tokens, Ordering::Relaxed);
    UNCACHED_INPUT_TOKENS.fetch_add(usage.input_tokens, Ordering::Relaxed);
    OUTPUT_TOKENS.fetch_add(usage.output_tokens, Ordering::Relaxed);
    CACHE_READ_INPUT_TOKENS.fetch_add(usage.cache_read_input_tokens, Ordering::Relaxed);
    CACHE_CREATION_INPUT_TOKENS.fetch_add(usage.cache_creation_input_tokens, Ordering::Relaxed);
    if usage.cache_read_input_tokens > 0 {
        CACHE_HIT_RESPONSES.fetch_add(1, Ordering::Relaxed);
    }
    record_histogram(
        &INFERENCE_DURATION_BUCKETS,
        &INFERENCE_DURATION_SUM_MS,
        elapsed,
    );
}
fn anthropic_stream<S>(
    input: S,
    model: String,
    started: Instant,
    guard: InFlightGuard,
) -> impl Stream<Item = Result<axum::response::sse::Event, Infallible>>
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    async_stream::stream! {
        let _guard = guard;
        let mut input = Box::pin(input);
        let mut buffer = Vec::<u8>::new();
        let mut machine = Machine::default();
        let mut finished = false;
        while let Some(chunk) = input.next().await {
            match chunk {
                Ok(bytes) => {
                    buffer.extend_from_slice(&bytes);
                    while let Some(end) = buffer.windows(2).position(|w| w == b"\n\n") {
                        let frame = buffer.drain(..end + 2).collect::<Vec<_>>();
                        let Ok(frame) = std::str::from_utf8(&frame) else {
                            INFERENCE_ERRORS.fetch_add(1, Ordering::Relaxed);
                            yield Ok(stream_error("upstream emitted invalid UTF-8"));
                            return;
                        };
                        let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) else { continue };
                        if data == "[DONE]" { continue }
                        if let Ok(value) = serde_json::from_str::<Value>(data) {
                            let event_type = value
                                .get("type")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            let upstream_failed = matches!(event_type, "response.failed" | "error");
                            if upstream_failed {
                                let upstream_error_code = value
                                    .pointer("/response/error/code")
                                    .or_else(|| value.pointer("/error/code"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown");
                                let upstream_error_type = value
                                    .pointer("/response/error/type")
                                    .or_else(|| value.pointer("/error/type"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown");
                                INFERENCE_ERRORS.fetch_add(1, Ordering::Relaxed);
                                tracing::warn!(
                                    model,
                                    upstream_event = event_type,
                                    upstream_error_code,
                                    upstream_error_type,
                                    "Codex upstream reported an in-stream failure"
                                );
                            }
                            let completed_usage = (event_type == "response.completed")
                                .then(|| Usage::from_openai(&value["response"]["usage"]));
                            match machine.apply(&value) {
                                Ok(events) => {
                                    for event in events { yield Ok(event) }
                                    if let Some(usage) = completed_usage {
                                        record_completion(started.elapsed(), &usage);
                                        finished = true;
                                    }
                                    if upstream_failed { return }
                                },
                                Err(error) => {
                                    INFERENCE_ERRORS.fetch_add(1, Ordering::Relaxed);
                                    yield Ok(axum::response::sse::Event::default().event("error").json_data(json!({"type":"error","error":{"type":error.kind,"message":error.message}})).unwrap());
                                    return;
                                }
                            }
                        }
                    }
                }
                Err(_) => {
                    INFERENCE_ERRORS.fetch_add(1, Ordering::Relaxed);
                    yield Ok(stream_error("upstream stream disconnected"));
                    return;
                }
            }
        }
        if !finished {
            INFERENCE_ERRORS.fetch_add(1, Ordering::Relaxed);
            yield Ok(stream_error("upstream stream ended before completion"));
        }
    }
}
fn stream_error(message: &str) -> axum::response::sse::Event {
    axum::response::sse::Event::default()
        .event("error")
        .json_data(json!({"type":"error","error":{"type":"api_error","message":message}}))
        .unwrap()
}
async fn nonstream<S>(input: S, model: &str, started: Instant) -> Result<Response, ApiError>
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
    let mut usage = Usage::default();
    let mut completed = false;
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
                usage = Usage::from_openai(&event["response"]["usage"]);
                completed = true;
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
                return Err(from_upstream_event(&event));
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
    if completed {
        record_completion(started.elapsed(), &usage);
    }
    Ok(Json(json!({"id":format!("msg_{}",uuid::Uuid::new_v4().simple()),"type":"message","role":"assistant","model":model,"content":content,"stop_reason":stop,"stop_sequence":null,"usage":usage})).into_response())
}

#[cfg(test)]
mod cache_key_tests {
    use super::*;

    fn headers(session: &'static str, agent: Option<&'static str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-claude-code-session-id",
            HeaderValue::from_static(session),
        );
        if let Some(agent) = agent {
            headers.insert("x-claude-code-agent-id", HeaderValue::from_static(agent));
        }
        headers
    }

    #[test]
    fn prompt_cache_keys_are_stable_and_isolated() {
        let base = prompt_cache_key(&headers("session-a", None), "user-a").unwrap();
        assert_eq!(base.len(), 64);
        assert_eq!(
            base,
            prompt_cache_key(&headers("session-a", None), "user-a").unwrap()
        );
        assert_ne!(
            base,
            prompt_cache_key(&headers("session-b", None), "user-a").unwrap()
        );
        assert_ne!(
            base,
            prompt_cache_key(&headers("session-a", None), "user-b").unwrap()
        );
        assert_ne!(
            base,
            prompt_cache_key(&headers("session-a", Some("agent-a")), "user-a").unwrap()
        );
    }

    #[test]
    fn prompt_cache_key_requires_a_nonempty_session() {
        assert!(prompt_cache_key(&HeaderMap::new(), "user-a").is_none());
        assert!(prompt_cache_key(&headers("", None), "user-a").is_none());
    }
}
