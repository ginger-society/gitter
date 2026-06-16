// src/merge_queue_handler.rs
//
// POST /org/merge-queue
//
// Accepts an org-wide merge request. The handler:
//   1. Validates inputs.
//   2. Generates a fresh UUID as the merge_request_id.
//   3. Acquires a Redis server lock (TTL = 60 s) so only one merge runs at a
//      time across the entire git server.
//   4. Stores the merge_request_id so other callers can see who holds the lock.
//   5. Publishes one job to the durable RabbitMQ work queue.
//   6. Returns the generated merge_request_id immediately — the actual work
//      happens asynchronously in the consumer.
//
// Redis keys
// ──────────
//   git:server:lock      → "1"                (SET NX EX 60)
//   git:merge:current    → <merge_request_id> (SET EX 60)
//
// HTTP 409 is returned when the lock is already held, with the ID of the
// blocking request in the response body.

use std::convert::Infallible;

use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;
use warp::http::StatusCode;

use crate::rabbit::{publish_merge_request, RabbitPoolRef};
use crate::requests::GenericResponse;
use crate::state::AppState;

// ── Redis key constants ───────────────────────────────────────────────────────

pub const SERVER_LOCK_KEY:   &str = "git:server:lock";
pub const CURRENT_MERGE_KEY: &str = "git:merge:current";
pub const SERVER_LOCK_TTL:    u64 = 60; // seconds

// ── Request / Response ────────────────────────────────────────────────────────

/// Body for POST /org/merge-queue
#[derive(Debug, Deserialize, ToSchema)]
pub struct MergeQueueRequest {
    /// Organisation prefix — repos must be named `{org_id}-*`
    pub org_id: String,
    /// Feature branch to merge into `main` in every matching repo.
    pub branch: String,
}

/// Successful enqueue response
#[derive(Debug, Serialize, ToSchema)]
pub struct MergeQueueResponse {
    /// Server-generated UUID that uniquely identifies this merge request.
    pub merge_request_id: String,
    pub org_id: String,
    pub branch: String,
    pub status: &'static str,
    /// How long (seconds) the server lock will be held.
    pub lock_ttl_secs: u64,
}

/// Returned with HTTP 409 when the server is already locked.
#[derive(Debug, Serialize, ToSchema)]
pub struct MergeQueueConflict {
    pub error: &'static str,
    /// merge_request_id that currently holds the lock.
    pub locked_by: String,
}

// ── Handler ───────────────────────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/org/merge-queue",
    tag = "default",
    request_body(content = MergeQueueRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "Request enqueued",                          body = MergeQueueResponse),
        (status = 400, description = "Validation error",                          body = GenericResponse),
        (status = 409, description = "Server locked by another merge request",    body = MergeQueueConflict),
        (status = 500, description = "Internal error",                            body = GenericResponse),
    )
)]
pub async fn handle_merge_queue(
    body: MergeQueueRequest,
    state: AppState,
    rabbit: RabbitPoolRef,
) -> Result<impl warp::Reply, Infallible> {
    // ── Validate ──────────────────────────────────────────────────────────────
    if body.org_id.trim().is_empty() {
        return Ok(bad_request("'org_id' must not be empty"));
    }
    if body.org_id.contains('/') || body.org_id.contains("..") {
        return Ok(bad_request("'org_id' contains invalid characters"));
    }
    if body.branch.trim().is_empty() {
        return Ok(bad_request("'branch' must not be empty"));
    }
    if body.branch == "main" {
        return Ok(bad_request("'branch' must not be 'main'"));
    }

    // ── Generate a fresh merge_request_id ─────────────────────────────────────
    let merge_request_id = Uuid::new_v4().to_string();

    tracing::info!(
        "POST /org/merge-queue merge_request_id={} org_id={} branch={}",
        merge_request_id,
        body.org_id,
        body.branch,
    );

    let mut redis = state.0.redis.clone();

    // ── Acquire server lock (SET NX EX) ───────────────────────────────────────
    let acquired: bool = redis::cmd("SET")
        .arg(SERVER_LOCK_KEY)
        .arg("1")
        .arg("NX")
        .arg("EX")
        .arg(SERVER_LOCK_TTL)
        .query_async(&mut redis)
        .await
        .unwrap_or(false);

    if !acquired {
        let locked_by: String = redis
            .get(CURRENT_MERGE_KEY)
            .await
            .unwrap_or_else(|_| "unknown".to_string());

        tracing::warn!(
            "[merge-queue] server locked by '{}' — rejecting new request for org={} branch={}",
            locked_by,
            body.org_id,
            body.branch,
        );

        return Ok(warp::reply::with_status(
            warp::reply::json(&MergeQueueConflict {
                error: "server_locked",
                locked_by,
            }),
            StatusCode::CONFLICT,
        )
        .into_response());
    }

    // ── Record who holds the lock ─────────────────────────────────────────────
    let _: Result<(), _> = redis
        .set_ex(
            CURRENT_MERGE_KEY,
            &merge_request_id,
            SERVER_LOCK_TTL.try_into().unwrap(),
        )
        .await;

    tracing::info!(
        "[merge-queue] lock acquired by '{}' (TTL={}s)",
        merge_request_id,
        SERVER_LOCK_TTL,
    );

    // ── Publish job to RabbitMQ ───────────────────────────────────────────────
    let payload = serde_json::json!({
        "merge_request_id": merge_request_id,
        "org_id":           body.org_id,
        "branch":           body.branch,
    });

    publish_merge_request(&rabbit, &payload.to_string()).await;

    tracing::info!(
        "[merge-queue] job published to RabbitMQ merge_request_id={}",
        merge_request_id,
    );

    Ok(warp::reply::with_status(
        warp::reply::json(&MergeQueueResponse {
            merge_request_id,
            org_id:        body.org_id,
            branch:        body.branch,
            status:        "queued",
            lock_ttl_secs: SERVER_LOCK_TTL,
        }),
        StatusCode::OK,
    )
    .into_response())
}

// ── Reply helpers ─────────────────────────────────────────────────────────────

trait IntoResponse {
    fn into_response(self) -> warp::reply::Response;
}

impl<R: warp::Reply> IntoResponse for warp::reply::WithStatus<R> {
    fn into_response(self) -> warp::reply::Response {
        warp::reply::Response::new(
            warp::reply::Reply::into_response(self).into_body(),
        )
    }
}

fn bad_request(msg: impl Into<String>) -> warp::reply::Response {
    warp::reply::with_status(
        warp::reply::json(&GenericResponse {
            status: "error",
            message: Some(msg.into()),
        }),
        StatusCode::BAD_REQUEST,
    )
    .into_response()
}