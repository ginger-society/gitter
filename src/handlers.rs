use std::convert::Infallible;

use ginger_shared_rs::rocket_utils::APIClaims;
use ginger_shared_rs::ISCClaims;
use tracing::{error, info, warn};
use warp::http::StatusCode;

use crate::permissions::{self, MemberType};
use crate::redis_lock::{mark_dirty, signal_pending};
use crate::requests::{
    AddMemberRequest, GenericResponse, KubeconfigRequest, MemberTypeDto, RemoveMemberRequest,
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
    tag = "default",
    security(("apiISCBearerAuth" = [])),
    params(
        ("workspace" = String, Path, description = "Workspace name"),
    ),
    request_body(content = AddMemberRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "Member already present", body = GenericResponse),
        (status = 201, description = "Member added", body = GenericResponse),
        (status = 400, description = "Validation error", body = GenericResponse),
        (status = 401, description = "Unauthorized", body = GenericResponse),
        (status = 500, description = "Internal error", body = GenericResponse),
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
            warp::reply::json(&GenericResponse {
                status: "error",
                message: Some("workspace must not be empty".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }
    if body.name.trim().is_empty() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&GenericResponse {
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
                warp::reply::json(&GenericResponse {
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
            warp::reply::json(&GenericResponse {
                status: "error",
                message: Some(e.to_string()),
            }),
            StatusCode::INTERNAL_SERVER_ERROR,
        ));
    }

    schedule_push(&state).await;

    if added {
        Ok(warp::reply::with_status(
            warp::reply::json(&GenericResponse {
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
            warp::reply::json(&GenericResponse {
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
    tag = "default",
    security(("apiISCBearerAuth" = [])),
    params(("workspace" = String, Path, description = "Workspace name")),
    request_body(content = RemoveMemberRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "Member removed or was not present", body = GenericResponse),
        (status = 400, description = "Validation error", body = GenericResponse),
        (status = 401, description = "Unauthorized", body = GenericResponse),
        (status = 500, description = "Internal error", body = GenericResponse),
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
            warp::reply::json(&GenericResponse {
                status: "error",
                message: Some("workspace must not be empty".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }
    if body.name.trim().is_empty() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&GenericResponse {
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
                warp::reply::json(&GenericResponse {
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
                warp::reply::json(&GenericResponse {
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
        warp::reply::json(&GenericResponse { status: "ok", message: Some(msg) }),
        StatusCode::OK,
    ))
}

// ── POST /kubeconfig ─────────────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/kubeconfig",
    tag = "default",
    security(("apiBearerAuth" = [])),
    request_body(content = KubeconfigRequest, content_type = "application/json"),
    responses(
        (status = 202, description = "Queued for push", body = GenericResponse),
        (status = 400, description = "Validation error", body = GenericResponse),
        (status = 401, description = "Unauthorized", body = GenericResponse),
        (status = 500, description = "Internal error", body = GenericResponse),
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
            warp::reply::json(&GenericResponse {
                status: "error",
                message: Some("workspace must not be empty".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }
    if body.environment.trim().is_empty() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&GenericResponse {
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
            warp::reply::json(&GenericResponse {
                status: "error",
                message: Some(e.to_string()),
            }),
            StatusCode::INTERNAL_SERVER_ERROR,
        ));
    }
    drop(repo);

    schedule_push(&state).await;

    Ok(warp::reply::with_status(
        warp::reply::json(&GenericResponse {
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
    tag = "default",
    responses(
        (status = 200, description = "Service is healthy", body = GenericResponse),
    )
)]
pub async fn handle_health() -> Result<impl warp::Reply, Infallible> {
    Ok(warp::reply::with_status(
        warp::reply::json(&GenericResponse { status: "ok", message: None }),
        StatusCode::OK,
    ))
}

// ── POST /tekton-kubeconfig ──────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/tekton-kubeconfig",
    tag = "default",
    security(("apiBearerAuth" = [])),
    request_body(content = UpdateTektonKubeconfigRequest, content_type = "application/json"),
    responses(
        (status = 202, description = "Queued for push", body = GenericResponse),
        (status = 400, description = "Validation error", body = GenericResponse),
        (status = 401, description = "Unauthorized", body = GenericResponse),
        (status = 500, description = "Internal error", body = GenericResponse),
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
            warp::reply::json(&GenericResponse {
                status: "error",
                message: Some("kubeconfig must not be empty".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }

    // ── Validate the kubeconfig actually works before persisting it ──────────
    //
    // A kubeconfig that parses fine but points at the wrong cluster, or lacks
    // write permission in tekton-pipelines, would otherwise only surface as a
    // failure much later — the first time some pipeline run actually needs
    // this config. Catching that here, synchronously, means the caller gets
    // an immediate, specific error instead of a kubeconfig silently getting
    // pushed and only failing downstream.
    //
    // The probe is a real merge-patch against the `feature-flags` ConfigMap
    // in `tekton-pipelines` — the same one-liner `kubectl patch configmap
    // feature-flags -n tekton-pipelines --type merge -p
    // '{"data":{"coschedule":"disabled"}}'` would do. Using `Patch::Merge`
    // (not `Patch::Apply`/strategic) is deliberate: a JSON merge patch only
    // touches the `coschedule` key and leaves every other key already in
    // `data` untouched, matching `kubectl --type merge` semantics exactly —
    // this is not a throwaway probe value, it's a real, idempotent,
    // permanent side effect of validating the config.
    if let Err(e) = validate_tekton_kubeconfig(&body.kubeconfig).await {
        warn!("[tekton-kubeconfig] validation failed: {e}");
        return Ok(warp::reply::with_status(
            warp::reply::json(&GenericResponse {
                status: "error",
                message: Some(format!("kubeconfig validation failed: {e}")),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }

    let repo = state.0.admin_repo.lock().await;
    if let Err(e) = repo.write_tekton_kubeconfig(&body.kubeconfig).await {
        error!("[git] write_tekton_kubeconfig failed: {e:#}");
        return Ok(warp::reply::with_status(
            warp::reply::json(&GenericResponse {
                status: "error",
                message: Some(e.to_string()),
            }),
            StatusCode::INTERNAL_SERVER_ERROR,
        ));
    }
    drop(repo);

    schedule_push(&state).await;

    Ok(warp::reply::with_status(
        warp::reply::json(&GenericResponse {
            status: "accepted",
            message: Some("tekton kubeconfig queued for push".into()),
        }),
        StatusCode::ACCEPTED,
    ))
}

/// Build a `kube::Client` from a raw kubeconfig YAML string and attempt a
/// real merge-patch against `tekton-pipelines/feature-flags`, setting
/// `coschedule: disabled`. Returns `Ok(())` only if the patch actually
/// succeeds against the live cluster — confirms the kubeconfig parses,
/// authenticates, and has write permission in `tekton-pipelines`, all in
/// one call.
///
/// This handler already runs under warp's async runtime, so — unlike the
/// git-hook binary's helpers, which wrap every call in `rt().block_on(...)`
/// because that binary has no async runtime of its own — this builds the
/// client and awaits the patch directly, no blocking-runtime wrapper needed.
async fn validate_tekton_kubeconfig(kubeconfig_yaml: &str) -> Result<(), String> {
    use k8s_openapi::api::core::v1::ConfigMap;
    use kube::api::{Api, Patch, PatchParams};
    use kube::config::{KubeConfigOptions, Kubeconfig};
    use kube::{Client, Config as KubeConfig};

    let kc: Kubeconfig = serde_yaml::from_str(kubeconfig_yaml)
        .map_err(|e| format!("failed to parse kubeconfig: {e}"))?;

    let cfg = KubeConfig::from_custom_kubeconfig(kc, &KubeConfigOptions::default())
        .await
        .map_err(|e| format!("failed to build kube config: {e}"))?;

    let client = Client::try_from(cfg)
        .map_err(|e| format!("failed to create kube client: {e}"))?;

    let api: Api<ConfigMap> = Api::namespaced(client, "tekton-pipelines");

    let patch = serde_json::json!({
        "data": { "coschedule": "disabled" }
    });

    api.patch(
        "feature-flags",
        &PatchParams::default(), // default PatchParams + Patch::Merge → application/merge-patch+json,
                                  // i.e. exactly `kubectl patch --type merge` — confirmed against kube-rs's
                                  // own docs.rs example (Patch::Merge(data) with PatchParams::default()).
        &Patch::Merge(patch),
    )
    .await
    .map_err(|e| format!("patch against tekton-pipelines/feature-flags failed: {e}"))?;

    Ok(())
}

// ── POST /pipeline-token ─────────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/pipeline-token",
    tag = "default",
    security(("apiBearerAuth" = [])),
    request_body(content = UpdatePipelineTokenRequest, content_type = "application/json"),
    responses(
        (status = 202, description = "Queued for push", body = GenericResponse),
        (status = 400, description = "Validation error", body = GenericResponse),
        (status = 401, description = "Unauthorized", body = GenericResponse),
        (status = 500, description = "Internal error", body = GenericResponse),
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
            warp::reply::json(&GenericResponse {
                status: "error",
                message: Some("workspace must not be empty".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }
    if body.token.trim().is_empty() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&GenericResponse {
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
            warp::reply::json(&GenericResponse {
                status: "error",
                message: Some(e.to_string()),
            }),
            StatusCode::INTERNAL_SERVER_ERROR,
        ));
    }
    drop(repo);

    schedule_push(&state).await;

    Ok(warp::reply::with_status(
        warp::reply::json(&GenericResponse {
            status: "accepted",
            message: Some(format!(
                "pipeline token for workspace '{}' queued for push",
                body.workspace
            )),
        }),
        StatusCode::ACCEPTED,
    ))
}