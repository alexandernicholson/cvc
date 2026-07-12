use axum::{
    Json,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::json;
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
