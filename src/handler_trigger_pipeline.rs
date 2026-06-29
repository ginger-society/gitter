// src/handler_trigger_pipeline.rs

use std::convert::Infallible;
use std::path::PathBuf;

use tracing::{error, info};
use warp::http::StatusCode;

use crate::{handle_run_pipeline::resolve_head, state::AppState};

const REPOS_DIR: &str = "/home/git/repositories";
const ADMIN_GIT_DIR: &str = "/home/git/repositories/gitolite-admin.git";
const SIDECAR_URL: &str = "http://ginger-gitter-sidecar:8080";

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
    /// Names of the PipelineRuns that were created, in trigger order.
    /// Empty on error responses.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub pipeline_runs: Vec<PipelineRunInfo>,
}

/// Minimal info about a single triggered PipelineRun — enough for the
/// caller (e.g. ginger-code push --force-pipeline) to open the TUI.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct PipelineRunInfo {
    pub pipeline_name: String,
    pub run_name: String,
    pub namespace: String,
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

    // ── Resolve branch HEAD ───────────────────────────────────────────────────
    let repo_path = PathBuf::from(format!("{REPOS_DIR}/{repo}.git"));
    if !repo_path.exists() {
        return Ok(err(StatusCode::NOT_FOUND, &format!("repository '{repo}' not found"), &repo, &branch, None));
    }

    let refname = format!("refs/heads/{branch}");
    let rp = repo_path.clone();
    let rf = refname.clone();
    let new_rev = match tokio::task::spawn_blocking(move || resolve_head(&rp, &rf)).await {
        Ok(Ok(sha)) => sha,
        Ok(Err(e))  => return Ok(err(StatusCode::NOT_FOUND, &format!("branch '{branch}' not found: {e}"), &repo, &branch, None)),
        Err(e)      => return Ok(err(StatusCode::INTERNAL_SERVER_ERROR, &format!("internal error: {e}"), &repo, &branch, None)),
    };

    info!("[trigger] {refname} → {new_rev}");

    // ── Delegate to pipeline::run() ───────────────────────────────────────────
    let r  = repo.clone();
    let rv = new_rev.clone();
    let rf = refname.clone();
    let u  = gl_user.clone();

    let result = tokio::task::spawn_blocking(move || {
        crate::pipeline_hook::pipeline::run(
            &u,
            &r,
            &rf,
            "0000000000000000000000000000000000000000",
            &rv,
            ADMIN_GIT_DIR,
            REPOS_DIR,
            SIDECAR_URL,
        )
    }).await;

    match result {
        Ok(Ok(pipeline_runs)) => {
            info!("[trigger] ✓ {} pipeline(s) triggered for {repo}/{branch}", pipeline_runs.len());

            let runs: Vec<PipelineRunInfo> = pipeline_runs
                .into_iter()
                .map(|(pipeline_name, run_name, namespace)| PipelineRunInfo {
                    pipeline_name,
                    run_name,
                    namespace,
                })
                .collect();

            Ok(warp::reply::with_status(
                warp::reply::json(&TriggerPipelineResponse {
                    status: "ok",
                    message: format!("pipelines triggered for {repo}@{branch}"),
                    repo,
                    branch,
                    commit: Some(new_rev),
                    pipeline_runs: runs,
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
            pipeline_runs: Vec::new(),
        }),
        code,
    )
}