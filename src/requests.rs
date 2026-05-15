use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// POST /permissions — full gitolite.conf replacement.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PermissionsRequest {
    /// Complete contents of gitolite.conf.
    /// The sidecar writes this verbatim to conf/gitolite.conf in the
    /// gitolite-admin repo and schedules a debounced push.
    #[schema(example = "repo gitolite-admin\n    RW+ = @all\n\nrepo testing\n    RW+ = @all\n")]
    pub conf: String,
}

/// POST /kubeconfig — write a workspace kubeconfig file.
#[derive(Debug, Deserialize, ToSchema)]
pub struct KubeconfigRequest {
    /// Workspace identifier. Used as the filename:
    /// `kubeconfig/<workspace>.yaml` inside gitolite-admin.
    /// Only alphanumeric characters, `-`, and `_` are allowed; others are
    /// stripped silently.
    #[schema(example = "staging-eu-west")]
    pub workspace: String,

    /// Raw kubeconfig YAML content.
    #[schema(example = "apiVersion: v1\nkind: Config\n...")]
    pub kubeconfig: String,
}

/// Standard API response envelope.
#[derive(Debug, Serialize, ToSchema)]
pub struct ApiResponse {
    /// `ok`, `accepted`, or `error`.
    pub status: &'static str,
    /// Human-readable detail (omitted on success where not needed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}