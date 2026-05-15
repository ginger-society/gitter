use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Member type for workspace permission endpoints.
#[derive(Debug, Deserialize, Serialize, ToSchema, PartialEq, Eq, Clone)]
#[serde(rename_all = "lowercase")]
pub enum MemberTypeDto {
    /// A human user identified by their username (e.g. `vriksh`).
    User,
    /// An agent or API server identified by a UUID
    /// (e.g. `435a8c5a-da91-4b95-8364-40ca23cb1109`).
    Group,
}

/// POST /workspace/:workspace/member — add a user or group to a workspace.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AddMemberRequest {
    /// Whether this is a human `user` or an agent `group`.
    pub r#type: MemberTypeDto,

    /// The identifier: a username for users, a UUID for groups.
    #[schema(example = "vriksh")]
    pub name: String,
}

/// DELETE /workspace/:workspace/member — remove a user or group.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RemoveMemberRequest {
    /// Whether this is a human `user` or an agent `group`.
    pub r#type: MemberTypeDto,

    /// The identifier to remove.
    #[schema(example = "vriksh")]
    pub name: String,
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

/// POST /tekton-kubeconfig — write the shared Tekton kubeconfig to the repo root.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateTektonKubeconfigRequest {
    /// Raw kubeconfig YAML content.
    #[schema(example = "apiVersion: v1\nkind: Config\n...")]
    pub kubeconfig: String,
}


/// POST /pipeline-token — write a workspace Ginger token.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdatePipelineTokenRequest {
    /// Workspace identifier — becomes the filename:
    /// `pipeline-tokens/<workspace>`
    /// Only alphanumeric characters, `-`, and `_` are allowed; others are stripped silently.
    #[schema(example = "acme")]
    pub workspace: String,

    /// The raw GINGER_TOKEN value.
    #[schema(example = "ginger_tok_abc123")]
    pub token: String,
}