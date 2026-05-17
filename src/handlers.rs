use std::convert::Infallible;

use ginger_shared_rs::rocket_utils::APIClaims;
use ginger_shared_rs::ISCClaims;
use tracing::{error, info, warn};
use warp::http::StatusCode;

use crate::permissions::{self, MemberType};
use crate::redis_lock::{mark_dirty, signal_pending};
use crate::requests::{
    AddMemberRequest, ApiResponse, KubeconfigRequest, MemberTypeDto, RemoveMemberRequest,
    UpdatePipelineTokenRequest, UpdateTektonKubeconfigRequest,
};
use crate::state::AppState;

// ── DTO → domain type ────────────────────────────────────────────────────────

fn to_member_type(dto: &MemberTypeDto) -> MemberType {
    match dto {
        MemberTypeDto::User  => MemberType::User,
        MemberTypeDto::Group => MemberType::Group,
    }
}

// ── Helper: schedule a debounced push ────────────────────────────────────────

async fn schedule_push(state: &AppState) {
    let mut redis = state.0.redis.clone();
    if let Err(e) = mark_dirty(&mut redis).await {
        error!("[redis] mark_dirty failed: {e:#}");
    }
    if let Err(e) = signal_pending(&mut redis, state.0.config.debounce_secs).await {
        error!("[redis] signal_pending failed: {e:#}");
    }
}

// ── POST /workspace/:workspace/member ────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/workspace/{workspace}/member",
    tag = "Permissions",
    security(("apiISCBearerAuth" = [])),
    params(
        ("workspace" = String, Path, description = "Workspace name"),
    ),
    request_body(content = AddMemberRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "Member already present", body = ApiResponse),
        (status = 201, description = "Member added", body = ApiResponse),
        (status = 400, description = "Validation error", body = ApiResponse),
        (status = 401, description = "Unauthorized", body = ApiResponse),
        (status = 500, description = "Internal error", body = ApiResponse),
    )
)]
pub async fn handle_add_member(
    workspace: String,
    body: AddMemberRequest,
    _claims: ISCClaims,         // extracted by with_isc_auth() — identity confirmed
    state: AppState,
) -> Result<impl warp::Reply, Infallible> {
    info!(
        "POST /workspace/{workspace}/member type={:?} name={} caller={}",
        body.r#type, body.name, _claims.sub
    );

    if workspace.trim().is_empty() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "error",
                message: Some("workspace must not be empty".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }
    if body.name.trim().is_empty() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "error",
                message: Some("name must not be empty".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }

    let kind = to_member_type(&body.r#type);
    let repo = state.0.admin_repo.lock().await;
    let repo_root = repo.repo_path.clone();
    drop(repo);

    let added = match permissions::add_member(&repo_root, &workspace, &kind, &body.name).await {
        Ok(a) => a,
        Err(e) => {
            error!("[permissions] add_member failed: {e:#}");
            return Ok(warp::reply::with_status(
                warp::reply::json(&ApiResponse {
                    status: "error",
                    message: Some(e.to_string()),
                }),
                StatusCode::INTERNAL_SERVER_ERROR,
            ));
        }
    };

    if let Err(e) = permissions::regenerate_conf(&repo_root).await {
        error!("[permissions] regenerate_conf failed: {e:#}");
        return Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "error",
                message: Some(e.to_string()),
            }),
            StatusCode::INTERNAL_SERVER_ERROR,
        ));
    }

    schedule_push(&state).await;

    if added {
        Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "ok",
                message: Some(format!(
                    "added {} '{}' to workspace '{workspace}'",
                    kind.as_str(), body.name
                )),
            }),
            StatusCode::CREATED,
        ))
    } else {
        Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "ok",
                message: Some(format!(
                    "{} '{}' already in workspace '{workspace}' — no change",
                    kind.as_str(), body.name
                )),
            }),
            StatusCode::OK,
        ))
    }
}

// ── DELETE /workspace/:workspace/member ──────────────────────────────────────

#[utoipa::path(
    delete,
    path = "/workspace/{workspace}/member",
    tag = "Permissions",
    security(("apiISCBearerAuth" = [])),
    params(("workspace" = String, Path, description = "Workspace name")),
    request_body(content = RemoveMemberRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "Member removed or was not present", body = ApiResponse),
        (status = 400, description = "Validation error", body = ApiResponse),
        (status = 401, description = "Unauthorized", body = ApiResponse),
        (status = 500, description = "Internal error", body = ApiResponse),
    )
)]
pub async fn handle_remove_member(
    workspace: String,
    body: RemoveMemberRequest,
    _claims: ISCClaims,
    state: AppState,
) -> Result<impl warp::Reply, Infallible> {
    info!(
        "DELETE /workspace/{workspace}/member type={:?} name={} caller={}",
        body.r#type, body.name, _claims.sub
    );

    if workspace.trim().is_empty() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "error",
                message: Some("workspace must not be empty".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }
    if body.name.trim().is_empty() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "error",
                message: Some("name must not be empty".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }

    let kind = to_member_type(&body.r#type);
    let repo = state.0.admin_repo.lock().await;
    let repo_root = repo.repo_path.clone();
    drop(repo);

    let removed = match permissions::remove_member(&repo_root, &workspace, &kind, &body.name).await {
        Ok(r) => r,
        Err(e) => {
            error!("[permissions] remove_member failed: {e:#}");
            return Ok(warp::reply::with_status(
                warp::reply::json(&ApiResponse {
                    status: "error",
                    message: Some(e.to_string()),
                }),
                StatusCode::INTERNAL_SERVER_ERROR,
            ));
        }
    };

    if removed {
        if let Err(e) = permissions::regenerate_conf(&repo_root).await {
            error!("[permissions] regenerate_conf failed: {e:#}");
            return Ok(warp::reply::with_status(
                warp::reply::json(&ApiResponse {
                    status: "error",
                    message: Some(e.to_string()),
                }),
                StatusCode::INTERNAL_SERVER_ERROR,
            ));
        }
        schedule_push(&state).await;
    }

    let msg = if removed {
        format!("removed {} '{}' from workspace '{workspace}'", kind.as_str(), body.name)
    } else {
        format!("{} '{}' not in workspace '{workspace}' — no change", kind.as_str(), body.name)
    };

    Ok(warp::reply::with_status(
        warp::reply::json(&ApiResponse { status: "ok", message: Some(msg) }),
        StatusCode::OK,
    ))
}

// ── POST /kubeconfig ─────────────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/kubeconfig",
    tag = "Admin",
    security(("apiBearerAuth" = [])),
    request_body(content = KubeconfigRequest, content_type = "application/json"),
    responses(
        (status = 202, description = "Queued for push", body = ApiResponse),
        (status = 400, description = "Validation error", body = ApiResponse),
        (status = 401, description = "Unauthorized", body = ApiResponse),
        (status = 500, description = "Internal error", body = ApiResponse),
    )
)]
pub async fn handle_kubeconfig(
    body: KubeconfigRequest,
    _claims: APIClaims,
    state: AppState,
) -> Result<impl warp::Reply, Infallible> {
    info!(
        "POST /kubeconfig workspace={} environment={} ({} bytes) caller={}",
        body.workspace, body.environment, body.kubeconfig.len(), _claims.sub
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
        error!("[git] write_kubeconfig failed: {e:#}");
        return Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "error",
                message: Some(e.to_string()),
            }),
            StatusCode::INTERNAL_SERVER_ERROR,
        ));
    }
    drop(repo);

    schedule_push(&state).await;

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

// ── GET /healthz ─────────────────────────────────────────────────────────────

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
        warp::reply::json(&ApiResponse { status: "ok", message: None }),
        StatusCode::OK,
    ))
}

// ── POST /tekton-kubeconfig ──────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/tekton-kubeconfig",
    tag = "Admin",
    security(("apiBearerAuth" = [])),
    request_body(content = UpdateTektonKubeconfigRequest, content_type = "application/json"),
    responses(
        (status = 202, description = "Queued for push", body = ApiResponse),
        (status = 400, description = "Validation error", body = ApiResponse),
        (status = 401, description = "Unauthorized", body = ApiResponse),
        (status = 500, description = "Internal error", body = ApiResponse),
    )
)]
pub async fn handle_update_tekton_kubeconfig(
    body: UpdateTektonKubeconfigRequest,
    _claims: APIClaims,
    state: AppState,
) -> Result<impl warp::Reply, Infallible> {
    info!(
        "POST /tekton-kubeconfig ({} bytes) caller={}",
        body.kubeconfig.len(), _claims.sub
    );

    if body.kubeconfig.trim().is_empty() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "error",
                message: Some("kubeconfig must not be empty".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }

    let repo = state.0.admin_repo.lock().await;
    if let Err(e) = repo.write_tekton_kubeconfig(&body.kubeconfig).await {
        error!("[git] write_tekton_kubeconfig failed: {e:#}");
        return Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "error",
                message: Some(e.to_string()),
            }),
            StatusCode::INTERNAL_SERVER_ERROR,
        ));
    }
    drop(repo);

    schedule_push(&state).await;

    Ok(warp::reply::with_status(
        warp::reply::json(&ApiResponse {
            status: "accepted",
            message: Some("tekton kubeconfig queued for push".into()),
        }),
        StatusCode::ACCEPTED,
    ))
}

// ── POST /pipeline-token ─────────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/pipeline-token",
    tag = "Admin",
    security(("apiBearerAuth" = [])),
    request_body(content = UpdatePipelineTokenRequest, content_type = "application/json"),
    responses(
        (status = 202, description = "Queued for push", body = ApiResponse),
        (status = 400, description = "Validation error", body = ApiResponse),
        (status = 401, description = "Unauthorized", body = ApiResponse),
        (status = 500, description = "Internal error", body = ApiResponse),
    )
)]
pub async fn handle_update_pipeline_token(
    body: UpdatePipelineTokenRequest,
    _claims: APIClaims,
    state: AppState,
) -> Result<impl warp::Reply, Infallible> {
    info!(
        "POST /pipeline-token workspace={} ({} bytes) caller={}",
        body.workspace, body.token.len(), _claims.sub
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
    if body.token.trim().is_empty() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "error",
                message: Some("token must not be empty".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }

    let repo = state.0.admin_repo.lock().await;
    if let Err(e) = repo.write_pipeline_token(&body.workspace, &body.token).await {
        error!("[git] write_pipeline_token failed: {e:#}");
        return Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "error",
                message: Some(e.to_string()),
            }),
            StatusCode::INTERNAL_SERVER_ERROR,
        ));
    }
    drop(repo);

    schedule_push(&state).await;

    Ok(warp::reply::with_status(
        warp::reply::json(&ApiResponse {
            status: "accepted",
            message: Some(format!(
                "pipeline token for workspace '{}' queued for push",
                body.workspace
            )),
        }),
        StatusCode::ACCEPTED,
    ))
}