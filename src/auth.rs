use crate::{
    db::{Repository, User},
    error::ApiError,
};
use axum::{
    extract::{FromRequestParts, Request},
    http::{header, request::Parts},
    middleware::Next,
    response::Response,
};
use std::sync::Arc;

#[derive(Clone)]
pub struct AuthState {
    pub repo: Arc<dyn Repository>,
}
#[derive(Clone, Debug)]
pub struct Caller(pub User);

pub async fn authenticate(mut req: Request, next: Next) -> Result<Response, ApiError> {
    let state = req
        .extensions()
        .get::<AuthState>()
        .cloned()
        .ok_or_else(|| ApiError::server("authentication unavailable"))?;
    let value = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::auth("missing bearer token"))?;
    let users = state
        .repo
        .users()
        .await
        .map_err(|_| ApiError::server("authentication unavailable"))?;
    let caller = tokio::task::spawn_blocking({
        let key = value.to_owned();
        move || {
            users
                .into_iter()
                .find(|u| !u.revoked && crate::crypto::verify_key(&u.key_hash, &key))
        }
    })
    .await
    .map_err(|_| ApiError::server("authentication unavailable"))?
    .ok_or_else(|| ApiError::auth("invalid bearer token"))?;
    req.extensions_mut().insert(Caller(caller));
    Ok(next.run(req).await)
}
impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Caller>()
            .cloned()
            .ok_or_else(|| ApiError::auth("authentication required"))
    }
}
