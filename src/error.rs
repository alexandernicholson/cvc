use axum::{
    Json,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use uuid::Uuid;
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub kind: &'static str,
    pub message: String,
    pub retry_after: Option<String>,
}
impl ApiError {
    pub fn auth(m: &str) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "authentication_error", m)
    }
    pub fn validation(m: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request_error", m)
    }
    pub fn server(m: &str) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "api_error", m)
    }
    pub fn new(s: StatusCode, k: &'static str, m: impl Into<String>) -> Self {
        Self {
            status: s,
            kind: k,
            message: m.into(),
            retry_after: None,
        }
    }
}

pub fn from_upstream_event(event: &Value) -> ApiError {
    let code = event
        .pointer("/response/error/code")
        .or_else(|| event.pointer("/error/code"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let kind = event
        .pointer("/response/error/type")
        .or_else(|| event.pointer("/error/type"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    match (code, kind) {
        ("context_length_exceeded", _) => ApiError::validation(
            "request exceeds the upstream model context window; shorten or compact the conversation",
        ),
        ("rate_limit_exceeded", _) | (_, "rate_limit_error") => ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            "Codex upstream rate limit exceeded",
        ),
        (_, "invalid_request_error") => ApiError::validation("Codex upstream rejected the request"),
        (_, "authentication_error") => {
            ApiError::auth("Codex upstream rejected the OpenAI credential")
        }
        (_, "permission_error") => ApiError::new(
            StatusCode::FORBIDDEN,
            "permission_error",
            "Codex upstream denied access to the requested model",
        ),
        (_, "overloaded_error") => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded_error",
            "Codex upstream is overloaded",
        ),
        _ => ApiError::new(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "Codex upstream stream failed",
        ),
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let id = format!("req_{}", Uuid::new_v4().simple());
        let mut h = HeaderMap::new();
        h.insert("request-id", HeaderValue::from_str(&id).unwrap());
        if let Some(v) = self
            .retry_after
            .and_then(|v| HeaderValue::from_str(&v).ok())
        {
            h.insert("retry-after", v);
        }
        (self.status,h,Json(json!({"type":"error","error":{"type":self.kind,"message":self.message},"request_id":id}))).into_response()
    }
}
