use std::collections::HashMap;
use std::path::PathBuf;

use crate::pipeline_hook::gitops::{
    get_changed_files, list_tekton_files, read_file_from_commit, read_from_admin_repo,
    resolve_workspace,
};
use crate::pipeline_hook::kubectl::{
    create_pipeline_run, ensure_ginger_token_secret, ensure_namespace, ensure_pvcs, kubectl_apply,
};
use crate::pipeline_hook::types::{PipelineDefinition, PipelineRunContext};
use crate::pipeline_hook::yaml::{parse_pipeline_yaml, should_trigger};
use crate::pipeline_hook::yaml_transform::{
    build_pipeline_run, builtin_clone_task, builtin_init_credentials_task, transform_pipeline,
    transform_task,
};

pub fn run(
    gl_user: &str,
    gl_repo: &str,
    refname: &str,
    old_rev: &str,
    new_rev: &str,
    admin_git_dir: &str,
    repos_dir: &str,
    sidecar_url: &str,
    cluster_ttl_seconds: u32,
) -> Result<(), String> {

    // ── 1. Derive branch name ─────────────────────────────────────────────────
    let branch = refname.strip_prefix("refs/heads/").ok_or("invalid refname")?;
    let is_main = branch == "main";
    let is_dev_branch = branch.starts_with("dev-");
    println!("[ginger-gitter] Branch: {}", branch);

    if !refname.starts_with("refs/heads/") {
        println!("[ginger-gitter] Skipping non-branch ref: {}", refname);
        return Ok(());
    }
    if new_rev.chars().all(|c| c == '0') {
        println!("[ginger-gitter] Skipping branch deletion");
        return Ok(());
    }

    // ── 2. Resolve workspace ──────────────────────────────────────────────────
    let workspace = resolve_workspace(gl_repo, admin_git_dir)?;
    println!("[ginger-gitter] Workspace: {}", workspace);

    // ── 3. Verify repo path exists ────────────────────────────────────────────
    let repo_path = PathBuf::from(format!("{}/{}.git", repos_dir, gl_repo));
    if !repo_path.exists() {
        return Err(format!("repo path does not exist: {}", repo_path.display()));
    }

    // ── 4. Compute changed files ──────────────────────────────────────────────
    let changed_files = get_changed_files(&repo_path, old_rev, new_rev)?;
    println!("[ginger-gitter] Changed files ({}):", changed_files.len());
    for f in &changed_files {
        println!("[ginger-gitter]   {}", f);
    }

    // ── 5. Read .tekton/ pipeline files ───────────────────────────────────────
    println!(
        "[ginger-gitter] Scanning .tekton/ in commit {} of repo {}",
        &new_rev[..8.min(new_rev.len())],
        gl_repo
    );
    let tekton_files = list_tekton_files(&repo_path, new_rev)?;
    if tekton_files.is_empty() {
        println!("[ginger-gitter] No .tekton pipeline files found — nothing to trigger");
        return Ok(());
    }
    println!("[ginger-gitter] Found {} pipeline file(s):", tekton_files.len());
    for f in &tekton_files {
        println!("[ginger-gitter]   {}", f);
    }

    // ── 6. Parse pipeline annotations ────────────────────────────────────────
    let pipelines = parse_pipeline_files(&repo_path, new_rev, &tekton_files)?;
    println!("[ginger-gitter] Parsed {} pipeline definition(s)", pipelines.len());

    // ── 7. Filter triggered pipelines ────────────────────────────────────────
    let triggered: Vec<&PipelineDefinition> = pipelines
        .iter()
        .filter(|p| should_trigger(p, refname, &changed_files))
        .collect();

    if triggered.is_empty() {
        println!("[ginger-gitter] No pipelines matched trigger conditions for this push");
        return Ok(());
    }
    println!("[ginger-gitter] {} pipeline(s) will be triggered:", triggered.len());
    for p in &triggered {
        println!("[ginger-gitter]   {} (concurrency: {})", p.name, p.concurrency);
    }

    // ── 8. Resolve per-workspace kubeconfig ───────────────────────────────────
    let workspace_kubeconfig_key = if is_main {
        format!("kubeconfig/{}/staging.yaml", workspace)
    } else {
        format!("kubeconfig/{}/{}.yaml", workspace, branch)
    };
    println!(
        "[ginger-gitter] Resolving workspace kubeconfig: {}",
        workspace_kubeconfig_key
    );

    let workspace_kubeconfig = match read_from_admin_repo(admin_git_dir, &workspace_kubeconfig_key) {
        Ok(kc) => {
            println!("[ginger-gitter] ✓ Workspace kubeconfig found");
            // TODO: Here we will inject this kubeconfig as-is into the PipelineRun context and let the tasks use it for patching the image tag and restart the deployement.
            kc
        }
        Err(e) => {
            return Err(format!(
                "kubeconfig not found for key '{}': {} — has the environment been provisioned?",
                workspace_kubeconfig_key, e
            ));
        }
    };

    // ── 9. Read Tekton kubeconfig from admin repo root ────────────────────────
    println!("[ginger-gitter] Reading Tekton control plane kubeconfig …");
    let tekton_kubeconfig = read_from_admin_repo(admin_git_dir, "kubeconfig.yaml")
        .map_err(|e| format!("tekton kubeconfig not found in admin repo root: {e} — has it been uploaded via /tekton-kubeconfig?"))?;
    println!("[ginger-gitter] ✓ Tekton kubeconfig loaded ({} bytes)", tekton_kubeconfig.len());

    // ── 10. Read workspace pipeline token ────────────────────────────────────
    let pipeline_token_key = format!("pipeline-tokens/{}", workspace);
    println!("[ginger-gitter] Reading pipeline token for workspace '{}' …", workspace);
    let ginger_token = read_from_admin_repo(admin_git_dir, &pipeline_token_key)
        .map_err(|e| format!(
            "GINGER_TOKEN not found at '{}': {} — has it been uploaded via /pipeline-token?",
            pipeline_token_key, e
        ))?;
    println!("[ginger-gitter] ✓ GINGER_TOKEN loaded ({} bytes)", ginger_token.trim().len());

    // ── 11. Build pipeline run context ────────────────────────────────────────
    let context = PipelineRunContext {
        gl_user: gl_user.to_string(),
        gl_repo: gl_repo.to_string(),
        gl_refname: refname.to_string(),
        gl_branch: branch.to_string(),
        gl_old_rev: old_rev.to_string(),
        gl_new_rev: new_rev.to_string(),
        gl_changed_files: changed_files.clone(),
        workspace: workspace.clone(),
        kubeconfig: workspace_kubeconfig.clone(),
        sidecar_url: sidecar_url.to_string(),
        ginger_token: ginger_token.trim().to_string(),
    };

    println!("[ginger-gitter] Pipeline run context:");
    println!("[ginger-gitter]   workspace    : {}", context.workspace);
    println!("[ginger-gitter]   branch       : {}", context.gl_branch);
    println!("[ginger-gitter]   new_rev      : {}", context.gl_new_rev);
    println!("[ginger-gitter]   changed_files: {}", context.gl_changed_files.len());

    // ── 12. Trigger each matched pipeline ─────────────────────────────────────
    // Namespace follows the project convention: tasks-<workspace>-<repo>.
    // The user's pipeline YAML intentionally omits it; we own it here so
    // there is exactly one source of truth and no parsing ambiguity.
    let namespace = format!(
        "tasks-{}-{}",
        workspace,
        gl_repo.replace('/', "-").replace('_', "-")
    );
    println!("[ginger-gitter] Target namespace: {}", namespace);

    for pipeline in &triggered {
        println!("[ginger-gitter] Triggering pipeline: {}", pipeline.name);

        trigger_pipeline(
            pipeline,
            &context,
            &tekton_kubeconfig,
            &repo_path,
            new_rev,
            &tekton_files,
            gl_repo,
            &namespace,
        )?;

        println!("[ginger-gitter] ✓ Pipeline triggered: {}", pipeline.name);
    }

    println!("[ginger-gitter] ✓ All pipelines triggered successfully");
    Ok(())
}

/// Full pipeline trigger: ensure namespace + PVCs + secret, apply tasks +
/// pipeline, then create PipelineRun.
fn trigger_pipeline(
    pipeline: &PipelineDefinition,
    ctx: &PipelineRunContext,
    tekton_kubeconfig: &str,
    repo_path: &PathBuf,
    new_rev: &str,
    tekton_files: &[String],
    gl_repo: &str,
    namespace: &str,
) -> Result<(), String> {

    // ── 12a. Ensure namespace exists ─────────────────────────────────────────
    println!("[ginger-gitter] Ensuring namespace: {}", namespace);
    ensure_namespace(tekton_kubeconfig, namespace)
        .map_err(|e| format!("failed to ensure namespace {}: {}", namespace, e))?;

    // ── 12b. Ensure PVCs exist ────────────────────────────────────────────────
    println!("[ginger-gitter] Ensuring PVCs in namespace: {}", namespace);
    ensure_pvcs(tekton_kubeconfig, namespace)
        .map_err(|e| format!("failed to ensure PVCs in {}: {}", namespace, e))?;

    // ── 12c. Ensure ginger-token-secret exists ────────────────────────────────
    println!("[ginger-gitter] Ensuring ginger-token-secret in namespace: {}", namespace);
    ensure_ginger_token_secret(tekton_kubeconfig, namespace, &ctx.ginger_token)
        .map_err(|e| format!("failed to ensure ginger-token-secret: {}", e))?;

    // ── 12d. Apply built-in tasks (init-credentials, clone) ──────────────────
    println!("[ginger-gitter] Applying built-in tasks …");
    let init_creds_yaml = builtin_init_credentials_task(namespace);
    kubectl_apply(tekton_kubeconfig, &init_creds_yaml)
        .map_err(|e| format!("failed to apply init-credentials task: {}", e))?;
    println!("[ginger-gitter] ✓ init-credentials task applied");

    let clone_yaml = builtin_clone_task(namespace);
    kubectl_apply(tekton_kubeconfig, &clone_yaml)
        .map_err(|e| format!("failed to apply clone task: {}", e))?;
    println!("[ginger-gitter] ✓ clone task applied");

    // ── 12e. Apply user-defined tasks from .tekton/tasks/ ────────────────────
    let task_files: Vec<&String> = tekton_files
        .iter()
        .filter(|f| {
            let lower = f.to_lowercase();
            (lower.contains("/tasks/") || lower.contains("\\tasks\\"))
                && (lower.ends_with(".yaml") || lower.ends_with(".yml"))
        })
        .collect();

    println!("[ginger-gitter] Applying {} user task file(s) …", task_files.len());
    for task_file in &task_files {
        match read_file_from_commit(repo_path, new_rev, task_file) {
            Ok(raw_yaml) => {
                let transformed = transform_task(&raw_yaml, namespace)
                    .map_err(|e| format!("failed to transform task {}: {}", task_file, e))?;

                println!("[ginger-gitter] Applying task: {}", task_file);
                kubectl_apply(tekton_kubeconfig, &transformed)
                    .map_err(|e| format!("failed to apply task {}: {}", task_file, e))?;
                println!("[ginger-gitter] ✓ Task applied: {}", task_file);
            }
            Err(e) => {
                println!(
                    "[ginger-gitter] WARNING: could not read task file {}: {} — skipping",
                    task_file, e
                );
            }
        }
    }

    // ── 12f. Apply pipeline from source file ──────────────────────────────────
    println!("[ginger-gitter] Applying pipeline: {}", pipeline.source_file);
    let pipeline_raw = read_file_from_commit(repo_path, new_rev, &pipeline.source_file)
        .map_err(|e| format!("failed to read pipeline file {}: {}", pipeline.source_file, e))?;

    let pipeline_transformed = transform_pipeline(&pipeline_raw, namespace, gl_repo)
        .map_err(|e| format!("failed to transform pipeline {}: {}", pipeline.source_file, e))?;

    kubectl_apply(tekton_kubeconfig, &pipeline_transformed)
        .map_err(|e| format!("failed to apply pipeline {}: {}", pipeline.name, e))?;
    println!("[ginger-gitter] ✓ Pipeline applied: {}", pipeline.name);

    // ── 12g. Handle concurrency: cancel-previous ──────────────────────────────
    if pipeline.concurrency == "cancel-previous" {
        cancel_running_pipeline_runs(tekton_kubeconfig, namespace, &pipeline.pipeline_name, ctx)?;
    }

    // ── 12h. Create PipelineRun ───────────────────────────────────────────────
    // Build user params from pipeline params (empty map — pipeline uses its defaults;
    // callers can extend this with annotation-driven params in the future)
    let user_params: HashMap<String, String> = HashMap::new();

    let pipeline_run_yaml = build_pipeline_run(
        &pipeline.pipeline_name,
        namespace,
        &user_params,
        &ctx.gl_user,
        &ctx.gl_repo,
        &ctx.gl_refname,
        &ctx.gl_new_rev,
    );

    println!("[ginger-gitter] Creating PipelineRun for: {}", pipeline.pipeline_name);
    let created = create_pipeline_run(tekton_kubeconfig, &pipeline_run_yaml)
        .map_err(|e| format!("failed to create PipelineRun for {}: {}", pipeline.name, e))?;
    println!("[ginger-gitter] ✓ PipelineRun created: {}", created.trim());

    Ok(())
}

/// Cancel any currently-running PipelineRuns for this pipeline (concurrency: cancel-previous).
fn cancel_running_pipeline_runs(
    tekton_kubeconfig: &str,
    namespace: &str,
    pipeline_name: &str,
    ctx: &PipelineRunContext,
) -> Result<(), String> {
    use std::process::Command;

    println!(
        "[ginger-gitter] Checking for running PipelineRuns to cancel (pipeline={}, repo={})",
        pipeline_name, ctx.gl_repo
    );

    let kc_path = {
        use std::fs;
        use std::io::Write;
        let path = std::env::temp_dir().join(format!(
            "ginger-gitter-cancel-kc-{}.yaml",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        let mut f = fs::File::create(&path)
            .map_err(|e| format!("failed to create temp kubeconfig for cancel: {}", e))?;
        f.write_all(tekton_kubeconfig.as_bytes())
            .map_err(|e| format!("failed to write kubeconfig for cancel: {}", e))?;
        path
    };

    let cleanup_kc = || {
        let _ = std::fs::remove_file(&kc_path);
    };

    // List running PipelineRuns with matching pipeline label
    let output = Command::new("kubectl")
        .args([
            "--kubeconfig", kc_path.to_str().unwrap_or("/tmp/kc.yaml"),
            "get", "pipelinerun",
            "-n", namespace,
            "-l", &format!("tekton.dev/pipeline={},ginger-gitter/repo={}", pipeline_name, ctx.gl_repo.replace('/', "-").replace('_', "-")),
            "--field-selector", "status.conditions[0].reason=Running",
            "-o", "jsonpath={.items[*].metadata.name}",
        ])
        .output()
        .map_err(|e| {
            cleanup_kc();
            format!("failed to list PipelineRuns: {}", e)
        })?;

    let names_str = String::from_utf8_lossy(&output.stdout);
    let names: Vec<&str> = names_str
        .split_whitespace()
        .filter(|n| !n.is_empty())
        .collect();

    if names.is_empty() {
        println!("[ginger-gitter] No running PipelineRuns to cancel");
        cleanup_kc();
        return Ok(());
    }

    for name in &names {
        println!("[ginger-gitter] Cancelling PipelineRun: {}", name);
        let patch = r#"{"spec":{"status":"CancelledRunFinally"}}"#;
        let cancel_out = Command::new("kubectl")
            .args([
                "--kubeconfig", kc_path.to_str().unwrap_or("/tmp/kc.yaml"),
                "patch", "pipelinerun", name,
                "-n", namespace,
                "--type=merge",
                "-p", patch,
            ])
            .output();

        match cancel_out {
            Ok(o) if o.status.success() => {
                println!("[ginger-gitter] ✓ Cancelled: {}", name);
            }
            Ok(o) => {
                println!(
                    "[ginger-gitter] WARNING: failed to cancel {}: {}",
                    name,
                    String::from_utf8_lossy(&o.stderr).trim()
                );
            }
            Err(e) => {
                println!("[ginger-gitter] WARNING: error cancelling {}: {}", name, e);
            }
        }
    }

    cleanup_kc();
    Ok(())
}

fn parse_pipeline_files(
    repo_path: &PathBuf,
    new_rev: &str,
    files: &[String],
) -> Result<Vec<PipelineDefinition>, String> {
    // Only parse top-level .tekton/*.yaml files as pipelines (not .tekton/tasks/*)
    let pipeline_files: Vec<&String> = files
        .iter()
        .filter(|f| {
            let lower = f.to_lowercase();
            !lower.contains("/tasks/") && !lower.contains("\\tasks\\")
        })
        .collect();

    let mut pipelines = Vec::new();
    for file in pipeline_files {
        match read_file_from_commit(repo_path, new_rev, file) {
            Ok(content) => match parse_pipeline_yaml(&content, file) {
                Ok(def) => pipelines.push(def),
                Err(e) => println!(
                    "[ginger-gitter] WARNING: could not parse {}: {} — skipping",
                    file, e
                ),
            },
            Err(e) => println!(
                "[ginger-gitter] WARNING: could not read {}: {} — skipping",
                file, e
            ),
        }
    }
    Ok(pipelines)
}