use std::collections::HashMap;
use std::path::PathBuf;

use crate::pipeline_hook::gitops::{
    get_changed_files, list_tekton_files, read_file_from_commit, read_from_admin_repo,
    resolve_workspace,
};
use crate::pipeline_hook::kubectl::{
    create_pipeline_run, ensure_buildah_pv, ensure_deployment_target_secret,
    ensure_ginger_token_secret, ensure_namespace, ensure_pvcs, kubectl_apply, rt,
};
use crate::pipeline_hook::types::{PipelineDefinition, PipelineRunContext};
use crate::pipeline_hook::yaml::{parse_pipeline_yaml, should_trigger};
use crate::pipeline_hook::yaml_transform::{
    build_pipeline_run, builtin_clone_task, builtin_init_credentials_task, sanitize_label,
    transform_pipeline, transform_task,
};


fn parse_run_name(raw: &str) -> String {
    // kubectl output format: "<resource>/<name>  <verb>"
    // Take the first whitespace-delimited token, then take everything after the '/'
    raw.split_whitespace()
        .next()
        .and_then(|token| token.split('/').nth(1))
        .unwrap_or(raw)
        .to_string()
}

/// Run the full pipeline hook for a push event.
///
/// Returns a list of `(pipeline_name, run_name, namespace)` tuples for every
/// PipelineRun that was successfully created, in trigger order. The list is
/// empty when no pipelines matched the trigger conditions (not an error).
pub fn run(
    gl_user: &str,
    gl_repo: &str,
    refname: &str,
    old_rev: &str,
    new_rev: &str,
    admin_git_dir: &str,
    repos_dir: &str,
    sidecar_url: &str,
) -> Result<Vec<(String, String, String)>, String> {

    // ── 1. Derive branch name ─────────────────────────────────────────────────
    let branch = refname.strip_prefix("refs/heads/").ok_or("invalid refname")?;
    let is_main = branch == "main";
    println!("[ginger-gitter] Branch: {}", branch);

    if !refname.starts_with("refs/heads/") {
        println!("[ginger-gitter] Skipping non-branch ref: {}", refname);
        return Ok(vec![]);
    }
    if new_rev.chars().all(|c| c == '0') {
        println!("[ginger-gitter] Skipping branch deletion");
        return Ok(vec![]);
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
        return Ok(vec![]);
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
        return Ok(vec![]);
    }
    println!("[ginger-gitter] {} pipeline(s) will be triggered:", triggered.len());
    for p in &triggered {
        println!("[ginger-gitter]   {} (concurrency: {})", p.name, p.concurrency);
    }

    // ── 8. Resolve per-workspace kubeconfig (optional) ───────────────────────
    let workspace_kubeconfig_key = if is_main {
        format!("kubeconfig/{}/staging.yaml", workspace)
    } else {
        format!("kubeconfig/{}/{}.yaml", workspace, branch)
    };
    println!(
        "[ginger-gitter] Resolving workspace kubeconfig: {}",
        workspace_kubeconfig_key
    );

    let workspace_kubeconfig: Option<String> =
        match read_from_admin_repo(admin_git_dir, &workspace_kubeconfig_key) {
            Ok(kc) => {
                println!("[ginger-gitter] ✓ Workspace kubeconfig found ({} bytes)", kc.len());
                Some(kc)
            }
            Err(_) => {
                println!(
                    "[ginger-gitter] ⚠ No workspace kubeconfig found for '{}' — \
                     pipeline will run but deployment steps (if any) will have no target",
                    workspace_kubeconfig_key
                );
                None
            }
        };

    // ── 9. Read Tekton kubeconfig ─────────────────────────────────────────────
    println!("[ginger-gitter] Reading Tekton control plane kubeconfig …");
    let tekton_kubeconfig = read_from_admin_repo(admin_git_dir, "kubeconfig.yaml")
        .map_err(|e| format!("tekton kubeconfig not found in admin repo root: {e}"))?;
    println!("[ginger-gitter] ✓ Tekton kubeconfig loaded ({} bytes)", tekton_kubeconfig.len());

    // ── 10. Read workspace pipeline token ────────────────────────────────────
    let pipeline_token_key = format!("pipeline-tokens/{}", workspace);
    println!("[ginger-gitter] Reading pipeline token for workspace '{}' …", workspace);
    let ginger_token = read_from_admin_repo(admin_git_dir, &pipeline_token_key)
        .map_err(|e| format!("GINGER_TOKEN not found at '{}': {}", pipeline_token_key, e))?;
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
    println!("[ginger-gitter]   workspace      : {}", context.workspace);
    println!("[ginger-gitter]   branch         : {}", context.gl_branch);
    println!("[ginger-gitter]   new_rev        : {}", context.gl_new_rev);
    println!("[ginger-gitter]   changed_files  : {}", context.gl_changed_files.len());
    println!(
        "[ginger-gitter]   deploy target  : {}",
        if context.kubeconfig.is_some() { "✓ kubeconfig present" } else { "⚠ none (build only)" }
    );

    // ── 12. Trigger each matched pipeline ─────────────────────────────────────
    let repo_basename = gl_repo
        .rsplit('/')
        .next()
        .unwrap_or(gl_repo)
        .replace('_', "-");
    let namespace = format!("tasks-{}", repo_basename);
    println!("[ginger-gitter] Target namespace: {}", namespace);

    let mut created_runs: Vec<(String, String, String)> = Vec::new();

    for pipeline in &triggered {
        println!("[ginger-gitter] Triggering pipeline: {}", pipeline.name);

        let run_name = trigger_pipeline(
            pipeline,
            &context,
            &tekton_kubeconfig,
            &repo_path,
            new_rev,
            &tekton_files,
            gl_repo,
            &namespace,
        )?;

        println!("[ginger-gitter] ✓ PipelineRun/{} created", run_name);
        created_runs.push((pipeline.pipeline_name.clone(), run_name, namespace.clone()));
    }

    println!("[ginger-gitter] ✓ All pipelines triggered successfully");
    Ok(created_runs)
}

/// Full pipeline trigger: ensure namespace + PVCs + secret, apply tasks +
/// pipeline, then create PipelineRun.
///
/// Returns the name of the created PipelineRun.
fn trigger_pipeline(
    pipeline: &PipelineDefinition,
    ctx: &PipelineRunContext,
    tekton_kubeconfig: &str,
    repo_path: &PathBuf,
    new_rev: &str,
    tekton_files: &[String],
    gl_repo: &str,
    namespace: &str,
) -> Result<String, String> {

    // ── 12a. Ensure namespace ─────────────────────────────────────────────────
    println!("[ginger-gitter] Ensuring namespace: {}", namespace);
    ensure_namespace(tekton_kubeconfig, namespace)
        .map_err(|e| format!("failed to ensure namespace {}: {}", namespace, e))?;

    // ── 12b. Ensure PVs + PVCs ───────────────────────────────────────────────
    println!("[ginger-gitter] Ensuring buildah-cache-pv (cluster-level NFS PV) …");
    ensure_buildah_pv(tekton_kubeconfig, namespace)
        .map_err(|e| format!("failed to ensure buildah-cache-pv: {}", e))?;

    println!("[ginger-gitter] Ensuring PVCs in namespace: {}", namespace);
    ensure_pvcs(tekton_kubeconfig, namespace)
        .map_err(|e| format!("failed to ensure PVCs in {}: {}", namespace, e))?;

    // ── 12c. Ensure ginger-token-secret ──────────────────────────────────────
    println!("[ginger-gitter] Ensuring ginger-token-secret in namespace: {}", namespace);
    ensure_ginger_token_secret(tekton_kubeconfig, namespace, &ctx.ginger_token)
        .map_err(|e| format!("failed to ensure ginger-token-secret: {}", e))?;

    // ── 12d. Ensure deployment-target secret ─────────────────────────────────
    let deployment_target_secret_name =
        format!("deployment-target-{}", sanitize_secret_name(&ctx.gl_branch));
    println!(
        "[ginger-gitter] Ensuring deployment target secret: {}",
        deployment_target_secret_name
    );
    ensure_deployment_target_secret(
        tekton_kubeconfig,
        namespace,
        &deployment_target_secret_name,
        &ctx.kubeconfig,
    )
    .map_err(|e| format!("failed to ensure deployment target secret: {}", e))?;

    // ── 12e. Apply built-in tasks ─────────────────────────────────────────────
    println!("[ginger-gitter] Applying built-in tasks …");
    let init_creds_yaml = builtin_init_credentials_task(namespace);
    kubectl_apply(tekton_kubeconfig, &init_creds_yaml)
        .map_err(|e| format!("failed to apply init-credentials task: {}", e))?;
    println!("[ginger-gitter] ✓ init-credentials task applied");

    let clone_yaml = builtin_clone_task(namespace);
    kubectl_apply(tekton_kubeconfig, &clone_yaml)
        .map_err(|e| format!("failed to apply clone task: {}", e))?;
    println!("[ginger-gitter] ✓ clone task applied");

    // ── 12f. Apply user-defined tasks ────────────────────────────────────────
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
                let transformed =
                    transform_task(&raw_yaml, namespace, &deployment_target_secret_name)
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

    // ── 12g. Apply pipeline ───────────────────────────────────────────────────
    println!("[ginger-gitter] Applying pipeline: {}", pipeline.source_file);
    let pipeline_raw = read_file_from_commit(repo_path, new_rev, &pipeline.source_file)
        .map_err(|e| format!("failed to read pipeline file {}: {}", pipeline.source_file, e))?;

    let pipeline_transformed = transform_pipeline(&pipeline_raw, namespace)
        .map_err(|e| format!("failed to transform pipeline {}: {}", pipeline.source_file, e))?;

    kubectl_apply(tekton_kubeconfig, &pipeline_transformed)
        .map_err(|e| format!("failed to apply pipeline {}: {}", pipeline.name, e))?;
    println!("[ginger-gitter] ✓ Pipeline applied: {}", pipeline.name);

    // ── 12h. Handle concurrency: cancel-previous ──────────────────────────────
    if pipeline.concurrency == "cancel-previous" {
        cancel_running_pipeline_runs(tekton_kubeconfig, namespace, &pipeline.pipeline_name, ctx)?;
    }

    // ── 12i. Create PipelineRun ───────────────────────────────────────────────
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
    let run_name = create_pipeline_run(tekton_kubeconfig, &pipeline_run_yaml)
        .map_err(|e| format!("failed to create PipelineRun for {}: {}", pipeline.name, e))?;

    Ok(parse_run_name(run_name.trim()))
}

fn cancel_running_pipeline_runs(
    tekton_kubeconfig: &str,
    namespace: &str,
    pipeline_name: &str,
    ctx: &PipelineRunContext,
) -> Result<(), String> {
    use kube::api::{Api, ListParams, Patch, PatchParams};
    use kube::config::{KubeConfigOptions, Kubeconfig};
    use kube::{Client, Config as KubeConfig, ResourceExt};

    println!(
        "[ginger-gitter] Checking for running PipelineRuns to cancel \
         (pipeline={}, repo={})",
        pipeline_name, ctx.gl_repo
    );

    let repo_label = sanitize_label(&ctx.gl_repo);

    rt().block_on(async {
        let kc: Kubeconfig = serde_yaml::from_str(tekton_kubeconfig)
            .map_err(|e| format!("failed to parse kubeconfig: {e}"))?;
        let cfg = KubeConfig::from_custom_kubeconfig(kc, &KubeConfigOptions::default())
            .await
            .map_err(|e| format!("failed to build kube config: {e}"))?;
        let client = Client::try_from(cfg)
            .map_err(|e| format!("failed to create kube client: {e}"))?;

        let ar = kube::discovery::ApiResource {
            group:       "tekton.dev".to_string(),
            version:     "v1beta1".to_string(),
            api_version: "tekton.dev/v1beta1".to_string(),
            kind:        "PipelineRun".to_string(),
            plural:      "pipelineruns".to_string(),
        };
        let api: Api<kube::core::DynamicObject> =
            Api::namespaced_with(client, namespace, &ar);

        let label_selector = format!(
            "tekton.dev/pipeline={},ginger-gitter/repo={}",
            pipeline_name, repo_label
        );
        let lp = ListParams::default().labels(&label_selector);
        let list = api
            .list(&lp)
            .await
            .map_err(|e| format!("failed to list PipelineRuns: {e}"))?;

        if list.items.is_empty() {
            println!("[ginger-gitter] No PipelineRuns found for label selector '{label_selector}'");
            return Ok(());
        }

        let running: Vec<_> = list
            .items
            .iter()
            .filter(|pr| {
                pr.data["status"]["conditions"]
                    .as_array()
                    .and_then(|conds| conds.first())
                    .and_then(|c| c["reason"].as_str())
                    == Some("Running")
            })
            .collect();

        if running.is_empty() {
            println!("[ginger-gitter] No running PipelineRuns to cancel");
            return Ok(());
        }

        let patch_params = PatchParams::default();
        let cancel_patch: serde_json::Value =
            serde_json::json!({ "spec": { "status": "CancelledRunFinally" } });

        for pr in running {
            let name = pr.name_any();
            println!("[ginger-gitter] Cancelling PipelineRun: {name}");
            match api
                .patch(&name, &patch_params, &Patch::Merge(&cancel_patch))
                .await
            {
                Ok(_) => println!("[ginger-gitter] ✓ Cancelled: {name}"),
                Err(e) => println!(
                    "[ginger-gitter] WARNING: failed to cancel {name}: {e:#}"
                ),
            }
        }

        Ok(())
    })
}

fn parse_pipeline_files(
    repo_path: &PathBuf,
    new_rev: &str,
    files: &[String],
) -> Result<Vec<PipelineDefinition>, String> {
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