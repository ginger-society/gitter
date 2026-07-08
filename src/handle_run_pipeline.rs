// src/handler_run_pipeline.rs
//
// POST /repo/run-pipeline
//
// Directly triggers a named pipeline file from a repo's .tekton/ folder without
// requiring a git push. Useful for IaC pipelines, ephemeral environment setup,
// manual re-runs, etc.
//
// What it does (in order):
//   1. Validate inputs.
//   2. Resolve branch HEAD SHA via `git rev-parse` on the bare repo.
//   3. Read the gitolite-admin repo for workspace, tekton kubeconfig, and pipeline token.
//   4. Check that .tekton/<pipeline_name> exists in that commit.
//   5. Apply all .tekton/tasks/*.yaml|.yml files (idempotent).
//   6. Apply the named pipeline file.
//   7. Build a PipelineRun YAML (merging caller-supplied params on top of system params).
//   8. Create the PipelineRun via kubectl create.
//
// Returns a structured response that includes:
//   - the created PipelineRun name
//   - a `trace` string: every log line joined with "\n" for easy pretty-printing
//   - resolved metadata (workspace, commit SHA, namespace)

use std::convert::Infallible;
use std::path::PathBuf;
use std::process::Command;

use tracing::{error, info};
use warp::http::StatusCode;

use crate::pipeline_hook::gitops::{
    list_tekton_files, read_file_from_commit, read_from_admin_repo, resolve_workspace,
};
use crate::pipeline_hook::kubectl::{
    create_pipeline_run, ensure_buildah_pv, ensure_ginger_token_secret, ensure_namespace,
    ensure_pvcs, kubectl_apply,
};
use crate::pipeline_hook::yaml_transform::{
    build_pipeline_run, builtin_clone_task, builtin_init_credentials_task, transform_pipeline,
    transform_task,
};
use crate::state::AppState;

const REPOS_DIR: &str = "/home/git/repositories";
const ADMIN_GIT_DIR: &str = "/home/git/repositories/gitolite-admin.git";

// ── Request / Response ────────────────────────────────────────────────────────

/// Extra key/value param to inject into the PipelineRun.
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct PipelineParam {
    pub key: String,
    pub val: String,
}

/// POST /repo/run-pipeline
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct RunPipelineRequest {
    /// Gitolite repo path, e.g. `"acme/acme-api-service"`.
    #[schema(example = "acme/acme-api-service")]
    pub repo: String,

    /// Branch name (without `refs/heads/`).
    #[schema(example = "main")]
    pub branch: String,

    /// File name of the pipeline inside `.tekton/`, e.g. `"provisioner.yaml"`.
    /// Must NOT include a path separator — only the filename.
    #[schema(example = "provisioner.yaml")]
    pub pipeline_name: String,

    /// Identity recorded in PipelineRun labels. Defaults to `"manual"`.
    #[schema(example = "alice")]
    pub triggered_by: Option<String>,

    /// Extra params merged into the PipelineRun (on top of system params).
    #[serde(default)]
    pub params: Vec<PipelineParam>,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct RunPipelineResponse {
    /// `"ok"` or `"error"`.
    pub status: &'static str,

    /// Human-readable error detail (only on error).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    /// Created PipelineRun name, e.g. `"provisioner-run-xk9f2"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pipeline_run: Option<String>,

    /// Resolved workspace name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,

    /// Kubernetes namespace the PipelineRun was created in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,

    /// Commit SHA the pipeline runs against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,

    /// Every trace line joined with `"\n"`. Split on `"\n"` to pretty-print.
    pub trace: String,
}

// ── Handler ───────────────────────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/repo/run-pipeline",
    tag = "default",
    request_body(content = RunPipelineRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "PipelineRun created",      body = RunPipelineResponse),
        (status = 400, description = "Validation / not-found",   body = RunPipelineResponse),
        (status = 500, description = "Internal error",           body = RunPipelineResponse),
    )
)]
pub async fn handle_run_pipeline(
    body: RunPipelineRequest,
    _state: AppState,
) -> Result<impl warp::Reply, Infallible> {
    let mut trace: Vec<String> = Vec::new();
    macro_rules! t {
        ($($arg:tt)*) => {{
            let line = format!($($arg)*);
            info!("[run-pipeline] {}", line);
            trace.push(line);
        }};
    }

    // ── 1. Validate ───────────────────────────────────────────────────────────
    let repo          = body.repo.trim().to_string();
    let branch        = body.branch.trim().to_string();
    let pipeline_name = body.pipeline_name.trim().to_string();
    let gl_user       = body.triggered_by.as_deref().unwrap_or("manual").to_string();

    if repo.is_empty() || repo.contains("..") {
        return Ok(bad(StatusCode::BAD_REQUEST, "'repo' is empty or invalid", trace));
    }
    if branch.is_empty() {
        return Ok(bad(StatusCode::BAD_REQUEST, "'branch' must not be empty", trace));
    }
    if pipeline_name.is_empty() || pipeline_name.contains('/') || pipeline_name.contains("..") {
        return Ok(bad(
            StatusCode::BAD_REQUEST,
            "'pipeline_name' must be a plain filename with no path separators",
            trace,
        ));
    }

    t!("POST /repo/run-pipeline repo={repo} branch={branch} pipeline={pipeline_name} triggered_by={gl_user}");

    // ── 2. Resolve branch HEAD (blocking, off Tokio executor) ────────────────
    let repo_path = PathBuf::from(format!("{REPOS_DIR}/{repo}.git"));
    if !repo_path.exists() {
        t!("ERROR: repository '{repo}' not found at {}", repo_path.display());
        return Ok(bad(StatusCode::BAD_REQUEST, &format!("repository '{repo}' not found"), trace));
    }

    let refname  = format!("refs/heads/{branch}");
    let rp       = repo_path.clone();
    let rf       = refname.clone();
    let new_rev  = match tokio::task::spawn_blocking(move || resolve_head(&rp, &rf)).await {
        Ok(Ok(sha)) => sha,
        Ok(Err(e))  => {
            t!("ERROR: branch '{branch}' not found: {e}");
            return Ok(bad(StatusCode::BAD_REQUEST, &format!("branch '{branch}' not found: {e}"), trace));
        }
        Err(e) => {
            t!("ERROR: internal error resolving branch: {e}");
            return Ok(bad(StatusCode::INTERNAL_SERVER_ERROR, "internal error resolving branch", trace));
        }
    };
    t!("Resolved {refname} → {new_rev}");

    // ── 3. Resolve workspace from admin repo ──────────────────────────────────
    t!("Resolving workspace from gitolite.conf …");
    let workspace = match tokio::task::spawn_blocking({
        let repo = repo.clone();
        move || resolve_workspace(&repo, ADMIN_GIT_DIR)
    })
    .await
    {
        Ok(Ok(ws)) => ws,
        Ok(Err(e)) => {
            t!("ERROR: workspace resolution failed: {e}");
            return Ok(bad(StatusCode::BAD_REQUEST, &format!("workspace resolution failed: {e}"), trace));
        }
        Err(e) => {
            t!("ERROR: spawn_blocking panicked: {e}");
            return Ok(bad(StatusCode::INTERNAL_SERVER_ERROR, "internal error", trace));
        }
    };
    t!("Workspace: {workspace}");

    // ── 4. Read tekton kubeconfig from admin repo ─────────────────────────────
    t!("Reading Tekton control-plane kubeconfig …");
    let tekton_kubeconfig = match tokio::task::spawn_blocking(|| {
        read_from_admin_repo(ADMIN_GIT_DIR, "kubeconfig.yaml")
    })
    .await
    {
        Ok(Ok(kc)) => { t!("✓ Tekton kubeconfig loaded ({} bytes)", kc.len()); kc }
        Ok(Err(e)) => {
            t!("ERROR: tekton kubeconfig not found: {e}");
            return Ok(bad(StatusCode::INTERNAL_SERVER_ERROR, &format!("tekton kubeconfig not found: {e}"), trace));
        }
        Err(e) => {
            t!("ERROR: spawn_blocking panicked: {e}");
            return Ok(bad(StatusCode::INTERNAL_SERVER_ERROR, "internal error", trace));
        }
    };

    // ── 5. Read pipeline token for workspace ─────────────────────────────────
    let token_key = format!("pipeline-tokens/{workspace}");
    t!("Reading pipeline token ({token_key}) …");
    let ginger_token = match tokio::task::spawn_blocking({
        let tk = token_key.clone();
        move || read_from_admin_repo(ADMIN_GIT_DIR, &tk)
    })
    .await
    {
        Ok(Ok(t)) => { let t = t.trim().to_string(); trace.push(format!("✓ GINGER_TOKEN loaded ({} bytes)", t.len())); t }
        Ok(Err(e)) => {
            t!("ERROR: pipeline token not found: {e}");
            return Ok(bad(StatusCode::INTERNAL_SERVER_ERROR, &format!("pipeline token not found for workspace '{workspace}': {e}"), trace));
        }
        Err(e) => {
            t!("ERROR: spawn_blocking panicked: {e}");
            return Ok(bad(StatusCode::INTERNAL_SERVER_ERROR, "internal error", trace));
        }
    };

    // ── 6. List .tekton/ files and verify pipeline_name exists ───────────────
    t!("Scanning .tekton/ in commit {} …", &new_rev[..8.min(new_rev.len())]);
    let tekton_files = match tokio::task::spawn_blocking({
        let rp = repo_path.clone();
        let rv = new_rev.clone();
        move || list_tekton_files(&rp, &rv)
    })
    .await
    {
        Ok(Ok(files)) => files,
        Ok(Err(e)) => {
            t!("ERROR: failed to list .tekton/ files: {e}");
            return Ok(bad(StatusCode::BAD_REQUEST, &format!("failed to list .tekton/ files: {e}"), trace));
        }
        Err(e) => {
            t!("ERROR: spawn_blocking panicked: {e}");
            return Ok(bad(StatusCode::INTERNAL_SERVER_ERROR, "internal error", trace));
        }
    };

    t!("Found {} .tekton/ file(s):", tekton_files.len());
    for f in &tekton_files {
        t!("  {f}");
    }

    // The canonical path inside the repo is `.tekton/<pipeline_name>`.
    let pipeline_path = format!(".tekton/{pipeline_name}");
    if !tekton_files.iter().any(|f| f == &pipeline_path) {
        t!("ERROR: '{pipeline_path}' not found in .tekton/ — available files:");
        for f in &tekton_files {
            t!("  {f}");
        }
        return Ok(bad(
            StatusCode::BAD_REQUEST,
            &format!("'{pipeline_path}' not found in .tekton/"),
            trace,
        ));
    }
    t!("✓ Found target pipeline: {pipeline_path}");

    // ── 7. Derive namespace ───────────────────────────────────────────────────
    let repo_basename = repo
        .rsplit('/')
        .next()
        .unwrap_or(&repo)
        .replace('_', "-");
    let namespace = format!("tasks-{repo_basename}");
    t!("Target namespace: {namespace}");

    // ── 8. Ensure namespace + infra (all idempotent) ──────────────────────────
    // These are blocking kubectl calls; run on spawn_blocking.
    macro_rules! kubectl_step {
        ($label:expr, $block:expr) => {{
            t!("Ensuring {} …", $label);
            match tokio::task::spawn_blocking($block).await {
                Ok(Ok(())) => t!("✓ {}", $label),
                Ok(Err(e)) => {
                    t!("ERROR: {} failed: {e}", $label);
                    return Ok(bad(StatusCode::INTERNAL_SERVER_ERROR, &format!("{} failed: {e}", $label), trace));
                }
                Err(e) => {
                    t!("ERROR: spawn_blocking panicked: {e}");
                    return Ok(bad(StatusCode::INTERNAL_SERVER_ERROR, "internal error", trace));
                }
            }
        }};
    }

    let kc1 = tekton_kubeconfig.clone();
    let ns1 = namespace.clone();
    kubectl_step!("namespace", move || ensure_namespace(&kc1, &ns1));

    let kc2 = tekton_kubeconfig.clone();
    let ns2 = namespace.clone();
    kubectl_step!("buildah-cache-pv", move || ensure_buildah_pv(&kc2, &ns2));

    let kc3 = tekton_kubeconfig.clone();
    let ns3 = namespace.clone();
    kubectl_step!("PVCs", move || ensure_pvcs(&kc3, &ns3));

    let kc4 = tekton_kubeconfig.clone();
    let ns4 = namespace.clone();
    let gt  = ginger_token.clone();
    kubectl_step!("ginger-token-secret", move || ensure_ginger_token_secret(&kc4, &ns4, &gt));

    // ── 9. Apply built-in tasks ───────────────────────────────────────────────
    t!("Applying built-in init-credentials task …");
    {
        let kc = tekton_kubeconfig.clone();
        let ns = namespace.clone();
        match tokio::task::spawn_blocking(move || {
            let yaml = builtin_init_credentials_task(&ns);
            kubectl_apply(&kc, &yaml)
        })
        .await
        {
            Ok(Ok(_)) => t!("✓ init-credentials task applied"),
            Ok(Err(e)) => { t!("ERROR: apply init-credentials failed: {e}"); return Ok(bad(StatusCode::INTERNAL_SERVER_ERROR, &format!("apply init-credentials failed: {e}"), trace)); }
            Err(e) => { t!("ERROR: spawn_blocking panicked: {e}"); return Ok(bad(StatusCode::INTERNAL_SERVER_ERROR, "internal error", trace)); }
        }
    }

    t!("Applying built-in clone task …");
    {
        let kc = tekton_kubeconfig.clone();
        let ns = namespace.clone();
        match tokio::task::spawn_blocking(move || {
            let yaml = builtin_clone_task(&ns);
            kubectl_apply(&kc, &yaml)
        })
        .await
        {
            Ok(Ok(_)) => t!("✓ clone task applied"),
            Ok(Err(e)) => { t!("ERROR: apply clone failed: {e}"); return Ok(bad(StatusCode::INTERNAL_SERVER_ERROR, &format!("apply clone failed: {e}"), trace)); }
            Err(e) => { t!("ERROR: spawn_blocking panicked: {e}"); return Ok(bad(StatusCode::INTERNAL_SERVER_ERROR, "internal error", trace)); }
        }
    }

    // ── 10. Apply user-defined tasks from .tekton/tasks/ ─────────────────────
    let deployment_target_secret = format!(
        "deployment-target-{}",
        sanitize_secret_name(&branch)
    );

    let task_files: Vec<String> = tekton_files
        .iter()
        .filter(|f| {
            let lower = f.to_lowercase();
            (lower.contains("/tasks/") || lower.contains("\\tasks\\"))
                && (lower.ends_with(".yaml") || lower.ends_with(".yml"))
        })
        .cloned()
        .collect();

    t!("Applying {} user task file(s) …", task_files.len());
    for task_file in &task_files {
        let tf  = task_file.clone();
        let rp  = repo_path.clone();
        let rv  = new_rev.clone();
        let kc  = tekton_kubeconfig.clone();
        let ns  = namespace.clone();
        let dts = deployment_target_secret.clone();

        t!("  Applying task: {tf}");
        match tokio::task::spawn_blocking(move || -> Result<(), String> {
            let raw = read_file_from_commit(&rp, &rv, &tf)
                .map_err(|e| format!("read failed: {e}"))?;
            let transformed = transform_task(&raw, &ns, &dts)
                .map_err(|e| format!("transform failed: {e}"))?;
            kubectl_apply(&kc, &transformed)
                .map(|_| ())
                .map_err(|e| format!("apply failed: {e}"))
        })
        .await
        {
            Ok(Ok(())) => t!("  ✓ {task_file}"),
            Ok(Err(e)) => t!("  WARNING: skipping {task_file}: {e}"),
            Err(e) => { t!("ERROR: spawn_blocking panicked: {e}"); return Ok(bad(StatusCode::INTERNAL_SERVER_ERROR, "internal error", trace)); }
        }
    }

    // ── 11. Read, transform, and apply the named pipeline ────────────────────
    t!("Reading pipeline file: {pipeline_path} …");
    let pipeline_raw = match tokio::task::spawn_blocking({
        let rp = repo_path.clone();
        let rv = new_rev.clone();
        let pp = pipeline_path.clone();
        move || read_file_from_commit(&rp, &rv, &pp)
    })
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            t!("ERROR: failed to read {pipeline_path}: {e}");
            return Ok(bad(StatusCode::BAD_REQUEST, &format!("failed to read {pipeline_path}: {e}"), trace));
        }
        Err(e) => {
            t!("ERROR: spawn_blocking panicked: {e}");
            return Ok(bad(StatusCode::INTERNAL_SERVER_ERROR, "internal error", trace));
        }
    };
    t!("✓ Read {} bytes", pipeline_raw.len());

    t!("Transforming pipeline YAML (namespace={namespace}) …");
    let pipeline_transformed = match transform_pipeline(&pipeline_raw, &namespace) {
        Ok(y) => y,
        Err(e) => {
            t!("ERROR: transform_pipeline failed: {e}");
            return Ok(bad(StatusCode::INTERNAL_SERVER_ERROR, &format!("transform_pipeline failed: {e}"), trace));
        }
    };

    // Extract pipeline_name (metadata.name) from the transformed YAML.
    // We need it to reference via pipelineRef in the PipelineRun.
    let pipeline_metadata_name = extract_yaml_name(&pipeline_transformed)
        .unwrap_or_else(|| {
            pipeline_name
                .trim_end_matches(".yaml")
                .trim_end_matches(".yml")
                .to_string()
        });
    t!("Pipeline metadata.name: {pipeline_metadata_name}");

    t!("Applying pipeline: {pipeline_metadata_name} …");
    {
        let kc = tekton_kubeconfig.clone();
        let py = pipeline_transformed.clone();
        match tokio::task::spawn_blocking(move || kubectl_apply(&kc, &py)).await {
            Ok(Ok(_)) => t!("✓ Pipeline applied"),
            Ok(Err(e)) => {
                t!("ERROR: kubectl apply pipeline failed: {e}");
                return Ok(bad(StatusCode::INTERNAL_SERVER_ERROR, &format!("kubectl apply pipeline failed: {e}"), trace));
            }
            Err(e) => {
                t!("ERROR: spawn_blocking panicked: {e}");
                return Ok(bad(StatusCode::INTERNAL_SERVER_ERROR, "internal error", trace));
            }
        }
    }

    // ── 12. Build and create PipelineRun ──────────────────────────────────────
    // Merge caller-supplied params (overrides / extras) on top of system params.
    let extra_params: std::collections::HashMap<String, String> = body
        .params
        .iter()
        .map(|p| (p.key.clone(), p.val.clone()))
        .collect();

    t!("Building PipelineRun ({} extra param(s)) …", extra_params.len());
    for (k, v) in &extra_params {
        t!("  param: {k} = {v}");
    }

    let pipeline_run_yaml = build_pipeline_run(
        &pipeline_metadata_name,
        &namespace,
        &extra_params,
        &gl_user,
        &repo,
        &refname,
        &new_rev,
    );

    t!("Creating PipelineRun …");
    let created_output = match tokio::task::spawn_blocking({
        let kc  = tekton_kubeconfig.clone();
        let pry = pipeline_run_yaml.clone();
        move || create_pipeline_run(&kc, &pry)
    })
    .await
    {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            t!("ERROR: kubectl create PipelineRun failed: {e}");
            return Ok(bad(StatusCode::INTERNAL_SERVER_ERROR, &format!("kubectl create PipelineRun failed: {e}"), trace));
        }
        Err(e) => {
            t!("ERROR: spawn_blocking panicked: {e}");
            return Ok(bad(StatusCode::INTERNAL_SERVER_ERROR, "internal error", trace));
        }
    };

    // `kubectl create` prints: `pipelinerun.tekton.dev/<name> created`
    let pipeline_run_name = created_output
        .lines()
        .find_map(|line| {
            line.split_whitespace()
                .next()
                .and_then(|tok| tok.strip_prefix("pipelinerun.tekton.dev/"))
                .map(str::to_string)
        })
        .unwrap_or_else(|| created_output.trim().to_string());

    t!("✓ PipelineRun created: {pipeline_run_name}");
    t!("Done.");

    Ok(warp::reply::with_status(
        warp::reply::json(&RunPipelineResponse {
            status: "ok",
            message: None,
            pipeline_run: Some(pipeline_run_name),
            workspace: Some(workspace),
            namespace: Some(namespace),
            commit: Some(new_rev),
            trace: trace.join("\n"),
        }),
        StatusCode::OK,
    ))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub fn resolve_head(repo_path: &PathBuf, git_ref: &str) -> Result<String, String> {
    let repo = git2::Repository::open_bare(repo_path)
        .map_err(|e| format!("failed to open repo: {e}"))?;
    let obj = repo
        .revparse_single(git_ref)
        .map_err(|e| e.to_string())?;
    Ok(obj.id().to_string())
}

/// Extract `metadata.name` from a YAML string (simple line scan, no full parse).
fn extract_yaml_name(yaml: &str) -> Option<String> {
    let mut in_metadata = false;
    for line in yaml.lines() {
        if line.trim() == "metadata:" {
            in_metadata = true;
            continue;
        }
        if in_metadata {
            if !line.starts_with(' ') && !line.starts_with('\t') {
                in_metadata = false;
                continue;
            }
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("name:") {
                let name = rest.trim().trim_matches('"').trim_matches('\'').to_string();
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
    }
    None
}

/// Sanitize a branch name for use in a Kubernetes secret / label name.
fn sanitize_secret_name(branch: &str) -> String {
    let lowered = branch.to_lowercase();
    let cleaned: String = lowered
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    let collapsed = cleaned
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    collapsed[..235.min(collapsed.len())].to_string()
}

// ── Reply helper ──────────────────────────────────────────────────────────────

fn bad(
    code: StatusCode,
    message: &str,
    trace: Vec<String>,
) -> warp::reply::WithStatus<warp::reply::Json> {
    warp::reply::with_status(
        warp::reply::json(&RunPipelineResponse {
            status: "error",
            message: Some(message.to_string()),
            pipeline_run: None,
            workspace: None,
            namespace: None,
            commit: None,
            trace: trace.join("\n"),
        }),
        code,
    )
}