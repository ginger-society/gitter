use thiserror::Error;

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

impl warp::reject::Reject for SidecarError {}