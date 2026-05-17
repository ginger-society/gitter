use thiserror::Error;
impl warp::reject::Reject for SidecarError {}


use warp::Filter;

#[derive(Debug, Error)]
pub enum SidecarError {
    #[error("Git operation failed: {0}")]
    Git(String),

    #[error("Redis error: {0}")]
    Redis(#[from] redis::RedisError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Lock acquisition failed")]
    LockFailed,

    #[error("Internal error: {0}")]
    Internal(#[from] anyhow::Error),
}

use std::convert::Infallible;

use warp::http::StatusCode;

/// JWT missing, expired, or invalid signature.
#[derive(Debug)]
pub struct JWTError;
impl warp::reject::Reject for JWTError {}

/// Authorization header present but token string is empty/malformed.
#[derive(Debug)]
pub struct InvalidTokenError;
impl warp::reject::Reject for InvalidTokenError {}

/// Central rejection handler — converts custom rejects into JSON responses.
pub async fn handle_rejection(
    err: warp::Rejection,
) -> Result<impl warp::Reply, Infallible> {
    let (code, message) = if err.find::<JWTError>().is_some() {
        (StatusCode::UNAUTHORIZED, "unauthorized: invalid or missing token")
    } else if err.find::<InvalidTokenError>().is_some() {
        (StatusCode::UNAUTHORIZED, "unauthorized: token format invalid")
    } else if err.is_not_found() {
        (StatusCode::NOT_FOUND, "not found")
    } else {
        (StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
    };

    let json = warp::reply::json(&serde_json::json!({
        "status": "error",
        "message": message,
    }));

    Ok(warp::reply::with_status(json, code))
}