/// Parsed representation of a .tekton/*.yaml pipeline definition.
#[derive(Debug)]
pub struct PipelineDefinition {
    pub source_file: String,
    pub name: String,
    pub namespace: String,
    pub pipeline_name: String,
    pub enabled: bool,
    pub trigger_branches: Vec<String>,
    pub path_filter: Vec<String>,
    pub ignore_paths: Vec<String>,
    pub concurrency: String,
}

/// All context passed to a triggered PipelineRun.
///
/// `kubeconfig` is the workspace/environment kubeconfig (staging or per-branch
/// ephemeral). It is `None` when no environment has been provisioned yet —
/// the build pipeline runs normally but deployment steps have no target.
#[derive(Debug)]
pub struct PipelineRunContext {
    pub gl_user: String,
    pub gl_repo: String,
    pub gl_refname: String,
    pub gl_branch: String,
    pub gl_old_rev: String,
    pub gl_new_rev: String,
    pub gl_changed_files: Vec<String>,
    pub workspace: String,
    pub kubeconfig: Option<String>,
    pub sidecar_url: String,
    pub ginger_token: String,
}