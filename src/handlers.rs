use std::convert::Infallible;

use serde::{Deserialize, Serialize};
use tracing::{error, info};
use warp::http::StatusCode;
use warp::reply::Json;

use crate::redis_lock::{mark_dirty, signal_pending};
use crate::state::AppState;

// ── Request / response types ─────────────────────────────────────────────────

/// POST /permissions
#[derive(Deserialize)]
pub struct PermissionsRequest {
    /// Full contents of gitolite.conf — no validation, just write it.
    pub conf: String,
}

/// POST /kubeconfig
#[derive(Deserialize)]
pub struct KubeconfigRequest {
    /// Workspace name — used as the filename: kubeconfig/<workspace>.yaml
    pub workspace: String,
    /// Raw kubeconfig YAML content.
    pub kubeconfig: String,
}

#[derive(Serialize)]
struct ApiResponse {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// POST /permissions
///
/// Body: `{ "conf": "<full gitolite.conf content>" }`
///
/// Writes the content to the local gitolite-admin checkout and schedules a
/// debounced push. Returns 202 Accepted immediately.
pub async fn handle_permissions(
    body: PermissionsRequest,
    state: AppState,
) -> Result<impl warp::Reply, Infallible> {
    info!("POST /permissions ({} bytes)", body.conf.len());

    let repo = state.0.admin_repo.lock().await;

    // 1. Write file
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

    drop(repo); // release mutex before touching redis

    // 2. Mark dirty + reset debounce window
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

/// POST /kubeconfig
///
/// Body: `{ "workspace": "my-workspace", "kubeconfig": "<yaml content>" }`
///
/// Writes to `kubeconfig/<workspace>.yaml` inside gitolite-admin and
/// schedules a debounced push. Returns 202 Accepted immediately.
///
/// No GET endpoint is provided; kubeconfig content is write-only from this
/// service's perspective.
pub async fn handle_kubeconfig(
    body: KubeconfigRequest,
    state: AppState,
) -> Result<impl warp::Reply, Infallible> {
    info!(
        "POST /kubeconfig workspace={} ({} bytes)",
        body.workspace,
        body.kubeconfig.len()
    );

    // Basic validation: workspace must not be empty
    if body.workspace.trim().is_empty() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "error",
                message: Some("workspace must not be empty".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }

    let repo = state.0.admin_repo.lock().await;

    if let Err(e) = repo.write_kubeconfig(&body.workspace, &body.kubeconfig).await {
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
                "kubeconfig for workspace '{}' queued for push",
                body.workspace
            )),
        }),
        StatusCode::ACCEPTED,
    ))
}

/// GET /healthz — liveness probe
pub async fn handle_health() -> Result<impl warp::Reply, Infallible> {
    Ok(warp::reply::with_status(
        warp::reply::json(&ApiResponse {
            status: "ok",
            message: None,
        }),
        StatusCode::OK,
    ))
}