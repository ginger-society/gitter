use std::convert::Infallible;

use tracing::{error, info};
use warp::http::StatusCode;

use crate::redis_lock::{mark_dirty, signal_pending};
use crate::requests::{ApiResponse, KubeconfigRequest, PermissionsRequest};
use crate::state::AppState;

// ── /permissions ─────────────────────────────────────────────────────────────

/// Update gitolite permissions
///
/// Accepts a complete `gitolite.conf` file and writes it to the
/// gitolite-admin repository. The push to gitolite is debounced — multiple
/// calls within the debounce window are coalesced into a single commit.
#[utoipa::path(
    post,
    path = "/permissions",
    tag = "Admin",
    request_body(
        content = PermissionsRequest,
        description = "Full gitolite.conf content",
        content_type = "application/json"
    ),
    responses(
        (status = 202, description = "Queued for push", body = ApiResponse),
        (status = 500, description = "Internal error", body = ApiResponse),
    )
)]
pub async fn handle_permissions(
    body: PermissionsRequest,
    state: AppState,
) -> Result<impl warp::Reply, Infallible> {
    info!("POST /permissions ({} bytes)", body.conf.len());

    let repo = state.0.admin_repo.lock().await;

    if let Err(e) = repo.write_gitolite_conf(&body.conf).await {
        error!("Failed to write gitolite.conf: {e:#}");
        return Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "error",
                message: Some(e.to_string()),
            }),
            StatusCode::INTERNAL_SERVER_ERROR,
        ));
    }

    drop(repo);

    let mut redis = state.0.redis.clone();
    if let Err(e) = mark_dirty(&mut redis).await {
        error!("Redis mark_dirty failed: {e:#}");
    }
    if let Err(e) = signal_pending(&mut redis, state.0.config.debounce_secs).await {
        error!("Redis signal_pending failed: {e:#}");
    }

    Ok(warp::reply::with_status(
        warp::reply::json(&ApiResponse {
            status: "accepted",
            message: Some("gitolite.conf queued for push".into()),
        }),
        StatusCode::ACCEPTED,
    ))
}

// ── /kubeconfig ──────────────────────────────────────────────────────────────

/// Write a workspace kubeconfig
///
/// Writes the provided kubeconfig YAML to
/// `kubeconfig/<workspace>.yaml` inside the gitolite-admin repository and
/// schedules a debounced push. There is intentionally no GET endpoint —
/// kubeconfig content is write-only through this service.
#[utoipa::path(
    post,
    path = "/kubeconfig",
    tag = "Admin",
    request_body(
        content = KubeconfigRequest,
        description = "Workspace name and raw kubeconfig YAML",
        content_type = "application/json"
    ),
    responses(
        (status = 202, description = "Queued for push", body = ApiResponse),
        (status = 400, description = "Bad request (empty workspace)", body = ApiResponse),
        (status = 500, description = "Internal error", body = ApiResponse),
    )
)]
pub async fn handle_kubeconfig(
    body: KubeconfigRequest,
    state: AppState,
) -> Result<impl warp::Reply, Infallible> {
    info!(
        "POST /kubeconfig workspace={} environment={} ({} bytes)",
        body.workspace,
        body.environment,
        body.kubeconfig.len()
    );

    if body.workspace.trim().is_empty() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "error",
                message: Some("workspace must not be empty".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }

    if body.environment.trim().is_empty() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "error",
                message: Some("environment must not be empty".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }

    let repo = state.0.admin_repo.lock().await;

    if let Err(e) = repo.write_kubeconfig(&body.workspace, &body.environment, &body.kubeconfig).await {
        error!("Failed to write kubeconfig: {e:#}");
        return Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "error",
                message: Some(e.to_string()),
            }),
            StatusCode::INTERNAL_SERVER_ERROR,
        ));
    }

    drop(repo);

    let mut redis = state.0.redis.clone();
    if let Err(e) = mark_dirty(&mut redis).await {
        error!("Redis mark_dirty failed: {e:#}");
    }
    if let Err(e) = signal_pending(&mut redis, state.0.config.debounce_secs).await {
        error!("Redis signal_pending failed: {e:#}");
    }

    Ok(warp::reply::with_status(
        warp::reply::json(&ApiResponse {
            status: "accepted",
            message: Some(format!(
                "kubeconfig for workspace '{}' env '{}' queued for push",
                body.workspace, body.environment
            )),
        }),
        StatusCode::ACCEPTED,
    ))
}

// ── /healthz ─────────────────────────────────────────────────────────────────

/// Liveness probe
///
/// Returns 200 OK when the service is up. Used by Kubernetes liveness and
/// readiness probes.
#[utoipa::path(
    get,
    path = "/healthz",
    tag = "Internal",
    responses(
        (status = 200, description = "Service is healthy", body = ApiResponse),
    )
)]
pub async fn handle_health() -> Result<impl warp::Reply, Infallible> {
    Ok(warp::reply::with_status(
        warp::reply::json(&ApiResponse {
            status: "ok",
            message: None,
        }),
        StatusCode::OK,
    ))
}