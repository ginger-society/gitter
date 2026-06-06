// src/merge_queue_handler.rs
//
// POST /repo/merge-queue
//
// Accepts a merge request, acquires a Redis-backed server lock (TTL = 60 s),
// stores the merge-request ID for reference, then publishes the job to the
// durable RabbitMQ work queue.
//
// Redis keys
// ──────────
//   git:server:lock                 → "1"  (SET NX EX 60)
//                                     Present while a merge is in flight.
//   git:merge:current               → <merge_request_id>
//                                     Identifies which request holds the lock.
//
// Conflict response
// ─────────────────
//   HTTP 409 — body contains the ID of the request currently holding the lock.

use std::convert::Infallible;

use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use warp::http::StatusCode;

use crate::rabbit::{publish_merge_request, RabbitPoolRef};
use crate::requests::ApiResponse;
use crate::state::AppState;

// ── Redis key constants ───────────────────────────────────────────────────────

pub const SERVER_LOCK_KEY:    &str = "git:server:lock";
pub const CURRENT_MERGE_KEY:  &str = "git:merge:current";
pub const SERVER_LOCK_TTL:     u64 = 60; // seconds

// ── Request / Response ────────────────────────────────────────────────────────

/// Body for POST /repo/merge-queue
#[derive(Debug, Deserialize, ToSchema)]
pub struct MergeQueueRequest {
    /// Unique identifier for this merge request (e.g. a UUID or PR number).
    pub merge_request_id: String,
    /// Repository name (same format as the `repo` field in other endpoints).
    pub repo: String,
    /// The feature branch to merge into `main`.
    pub branch: String,
    /// Optional human-readable description / commit message.
    #[serde(default)]
    pub message: Option<String>,
}

/// Successful enqueue response
#[derive(Debug, Serialize, ToSchema)]
pub struct MergeQueueResponse {
    pub merge_request_id: String,
    pub repo: String,
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
    path = "/repo/merge-queue",
    tag = "default",
    request_body(content = MergeQueueRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "Request enqueued",           body = MergeQueueResponse),
        (status = 400, description = "Validation error",           body = ApiResponse),
        (status = 409, description = "Server locked by another merge request", body = MergeQueueConflict),
        (status = 500, description = "Internal error",             body = ApiResponse),
    )
)]
pub async fn handle_merge_queue(
    body: MergeQueueRequest,
    state: AppState,
    rabbit: RabbitPoolRef,
) -> Result<impl warp::Reply, Infallible> {
    // ── Validate ──────────────────────────────────────────────────────────────
    if body.merge_request_id.trim().is_empty() {
        return Ok(bad_request("'merge_request_id' must not be empty"));
    }
    if body.repo.trim().is_empty() {
        return Ok(bad_request("'repo' must not be empty"));
    }
    if body.branch.trim().is_empty() {
        return Ok(bad_request("'branch' must not be empty"));
    }
    if body.branch == "main" {
        return Ok(bad_request("'branch' must not be 'main'"));
    }

    tracing::info!(
        "POST /repo/merge-queue merge_request_id={} repo={} branch={}",
        body.merge_request_id,
        body.repo,
        body.branch,
    );

    let mut redis = state.0.redis.clone();

    // ── Acquire server lock (SET NX EX) ───────────────────────────────────────
    // SET git:server:lock "1" NX EX 60
    // Returns true only if the key did NOT exist (we got the lock).
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
        // Read who currently holds the lock for a helpful 409 body.
        let locked_by: String = redis
            .get(CURRENT_MERGE_KEY)
            .await
            .unwrap_or_else(|_| "unknown".to_string());

        tracing::warn!(
            "[merge-queue] server locked by '{}' — rejecting '{}'",
            locked_by,
            body.merge_request_id
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

    // ── Store the merge_request_id that holds the lock ────────────────────────
    // Use the same TTL so this key doesn't outlive the lock.
    let _: Result<(), _> = redis
        .set_ex(
            CURRENT_MERGE_KEY,
            &body.merge_request_id,
            SERVER_LOCK_TTL.try_into().unwrap(),
        )
        .await;

    tracing::info!(
        "[merge-queue] lock acquired by '{}' (TTL={}s)",
        body.merge_request_id,
        SERVER_LOCK_TTL,
    );

    // ── Enqueue the job ───────────────────────────────────────────────────────
    let payload = serde_json::json!({
        "merge_request_id": body.merge_request_id,
        "repo":             body.repo,
        "branch":           body.branch,
        "message":          body.message,
    });

    publish_merge_request(&rabbit, &payload.to_string()).await;

    tracing::info!(
        "[merge-queue] job published to RabbitMQ merge_request_id={}",
        body.merge_request_id,
    );

    Ok(warp::reply::with_status(
        warp::reply::json(&MergeQueueResponse {
            merge_request_id: body.merge_request_id,
            repo:             body.repo,
            branch:           body.branch,
            status:           "queued",
            lock_ttl_secs:    SERVER_LOCK_TTL,
        }),
        StatusCode::OK,
    )
    .into_response())
}

// ── Reply helpers ─────────────────────────────────────────────────────────────

// We need a unified return type so both branches compile.
// warp::reply::Response is Box<dyn warp::Reply>.
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

type JsonReply = warp::reply::WithStatus<warp::reply::Json>;

fn bad_request(msg: impl Into<String>) -> warp::reply::Response {
    warp::reply::with_status(
        warp::reply::json(&ApiResponse {
            status: "error",
            message: Some(msg.into()),
        }),
        StatusCode::BAD_REQUEST,
    )
    .into_response()
}