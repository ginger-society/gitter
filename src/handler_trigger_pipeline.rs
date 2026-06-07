// src/handler_trigger_pipeline.rs
//


use std::convert::Infallible;
use std::path::PathBuf;
use std::process::Command;

use tracing::{error, info, warn};
use warp::http::StatusCode;

use crate::state::AppState;

const REPOS_DIR: &str = "/home/git/repositories";
const ADMIN_GIT_DIR: &str = "/home/git/repositories/gitolite-admin.git";
const SIDECAR_URL: &str = "http://ginger-gitter-sidecar:8080";
const CLUSTER_TTL_SECONDS: u32 = 5 * 24 * 60 * 60;

// ── Request / Response ────────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct TriggerPipelineRequest {
    /// Gitolite repo path, e.g. `"acme/acme-api-service"`.
    #[schema(example = "acme/acme-api-service")]
    pub repo: String,

    /// Branch name without the `refs/heads/` prefix.
    #[schema(example = "main")]
    pub branch: String,

    /// Identity recorded in PipelineRun labels/params. Defaults to `"manual"`.
    #[schema(example = "alice")]
    pub triggered_by: Option<String>,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct TriggerPipelineResponse {
    pub status: &'static str,
    pub message: String,
    pub repo: String,
    pub branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

// ── Handler ───────────────────────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/repo/trigger-pipeline",
    tag = "default",
    request_body(content = TriggerPipelineRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "Pipeline(s) triggered",       body = TriggerPipelineResponse),
        (status = 400, description = "Validation error",             body = TriggerPipelineResponse),
        (status = 404, description = "Repo or branch not found",     body = TriggerPipelineResponse),
        (status = 500, description = "Internal error",               body = TriggerPipelineResponse),
    )
)]
pub async fn handle_trigger_pipeline(
    body: TriggerPipelineRequest,
    _state: AppState,
) -> Result<impl warp::Reply, Infallible> {
    let repo       = body.repo.trim().to_string();
    let branch     = body.branch.trim().to_string();
    let gl_user    = body.triggered_by.as_deref().unwrap_or("manual").to_string();

    // ── Validate ──────────────────────────────────────────────────────────────
    if repo.is_empty() || repo.contains("..") {
        return Ok(err(StatusCode::BAD_REQUEST, "'repo' is empty or invalid", &repo, &branch, None));
    }
    if branch.is_empty() {
        return Ok(err(StatusCode::BAD_REQUEST, "'branch' must not be empty", &repo, &branch, None));
    }

    info!("POST /repo/trigger-pipeline repo={repo} branch={branch} triggered_by={gl_user}");

    // ── Resolve branch HEAD (blocking I/O — off the async executor) ───────────
    let repo_path = PathBuf::from(format!("{REPOS_DIR}/{repo}.git"));
    if !repo_path.exists() {
        return Ok(err(StatusCode::NOT_FOUND, &format!("repository '{repo}' not found"), &repo, &branch, None));
    }

    let refname   = format!("refs/heads/{branch}");
    let rp        = repo_path.clone();
    let rf        = refname.clone();
    let new_rev   = match tokio::task::spawn_blocking(move || resolve_head(&rp, &rf)).await {
        Ok(Ok(sha)) => sha,
        Ok(Err(e))  => return Ok(err(StatusCode::NOT_FOUND, &format!("branch '{branch}' not found: {e}"), &repo, &branch, None)),
        Err(e)      => return Ok(err(StatusCode::INTERNAL_SERVER_ERROR, &format!("internal error: {e}"), &repo, &branch, None)),
    };

    info!("[trigger] {refname} → {new_rev}");

    // ── Delegate to the shared pipeline_hook::pipeline::run() ─────────────────
    //
    // old_rev = all-zeros: get_changed_files() treats this as a new branch and
    // runs `git diff-tree --no-commit-id -r <new_rev>`, giving the files touched
    // by the tip commit. path-filter / ignore-paths work normally.
    let r  = repo.clone();
    let rv = new_rev.clone();
    let rf = refname.clone();
    let u  = gl_user.clone();

    let result = tokio::task::spawn_blocking(move || {
        crate::pipeline_hook::pipeline::run(
            &u,                                          // gl_user
            &r,                                          // gl_repo
            &rf,                                         // refname
            "0000000000000000000000000000000000000000",  // old_rev
            &rv,                                         // new_rev
            ADMIN_GIT_DIR,
            REPOS_DIR,
            SIDECAR_URL,
            CLUSTER_TTL_SECONDS,
        )
    }).await;

    match result {
        Ok(Ok(())) => {
            info!("[trigger] ✓ pipelines triggered for {repo}/{branch}");
            Ok(warp::reply::with_status(
                warp::reply::json(&TriggerPipelineResponse {
                    status:  "ok",
                    message: format!("pipelines triggered for {repo}@{branch}"),
                    repo,
                    branch,
                    commit:  Some(new_rev),
                }),
                StatusCode::OK,
            ))
        }
        Ok(Err(e)) => {
            error!("[trigger] pipeline run failed: {e}");
            Ok(err(StatusCode::INTERNAL_SERVER_ERROR, &e, &repo, &branch, Some(&new_rev)))
        }
        Err(e) => {
            error!("[trigger] spawn_blocking panicked: {e:#}");
            Ok(err(StatusCode::INTERNAL_SERVER_ERROR, "internal error during trigger", &repo, &branch, Some(&new_rev)))
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Resolve the HEAD commit SHA for a ref in a bare repo.
fn resolve_head(repo_path: &PathBuf, git_ref: &str) -> Result<String, String> {
    let out = Command::new("git")
        .args(["rev-parse", git_ref])
        .env("GIT_DIR", repo_path)
        .output()
        .map_err(|e| format!("git rev-parse failed to spawn: {e}"))?;

    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn err(
    code: StatusCode,
    message: &str,
    repo: &str,
    branch: &str,
    commit: Option<&str>,
) -> warp::reply::WithStatus<warp::reply::Json> {
    warp::reply::with_status(
        warp::reply::json(&TriggerPipelineResponse {
            status:  "error",
            message: message.to_string(),
            repo:    repo.to_string(),
            branch:  branch.to_string(),
            commit:  commit.map(str::to_string),
        }),
        code,
    )
}