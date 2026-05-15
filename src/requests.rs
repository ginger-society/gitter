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

/// POST /kubeconfig — write a workspace+environment kubeconfig file.
#[derive(Debug, Deserialize, ToSchema)]
pub struct KubeconfigRequest {
    /// Workspace identifier — becomes the directory name:
    /// `kubeconfig/<workspace>/<environment>.yaml`.
    /// Only alphanumeric characters, `-`, and `_` are allowed; others are
    /// stripped silently.
    #[schema(example = "my-workspace")]
    pub workspace: String,

    /// Environment name — becomes the filename inside the workspace directory.
    /// E.g. `production`, `staging`, `dev`.
    /// Only alphanumeric characters, `-`, and `_` are allowed; others are
    /// stripped silently.
    #[schema(example = "production")]
    pub environment: String,

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