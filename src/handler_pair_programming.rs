// src/handler_pair_programming.rs
//
// Pair-programming branch permission endpoints.
//
// The idea: a branch prefix like `TICKET-{n}-*` is a "pair programming slot".
// Any number of individual users can be granted write access to branches that
// match that prefix inside a specific repo of a workspace.
//
// Storage layout (inside the gitolite-admin repo):
//
//   pair-programming/
//     <workspace>/
//       <repo>/
//         <branch-prefix>    ← newline-delimited usernames (no groups)
//
// Gitolite conf contribution (written by regenerate_conf in permissions.rs):
//
//   repo <workspace>-<repo>
//       RW+ refs/heads/<branch-prefix>*  =  <name_a> <name_b> …
//
// Endpoints
// ─────────
//   POST   /pair-programming/member          – add a user to a prefix slot
//   DELETE /pair-programming/member          – remove a user from a prefix slot
//   DELETE /pair-programming/branch-config   – delete the entire prefix slot
//
// All three share the same JSON shape (branch_config is a subset):
//
//   POST / DELETE /pair-programming/member
//     { "workspace": "…", "repo": "…", "branch_prefix": "…", "name": "…" }
//
//   DELETE /pair-programming/branch-config
//     { "workspace": "…", "repo": "…", "branch_prefix": "…" }
//
// Auth: ISC (X-ISC-API-Authorization: Bearer <token>)

use std::convert::Infallible;
use std::path::{Path, PathBuf};

use ginger_shared_rs::ISCClaims;
use serde::{Deserialize, Serialize};
use tracing::{error, info};
use utoipa::ToSchema;
use warp::http::StatusCode;

use crate::permissions::regenerate_conf;
use crate::redis_lock::{mark_dirty, signal_pending};
use crate::requests::GenericResponse;
use crate::state::AppState;

// ── Request types ─────────────────────────────────────────────────────────────

/// Body for POST /pair-programming/member and DELETE /pair-programming/member.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PairMemberRequest {
    /// Workspace identifier, e.g. `"acme"`.
    #[schema(example = "acme")]
    pub workspace: String,

    /// Bare repo name within the workspace (without the `<workspace>-` prefix),
    /// e.g. `"api-service"`. The gitolite repo is `<workspace>-<repo>`.
    #[schema(example = "api-service")]
    pub repo: String,

    /// Branch prefix, e.g. `"TICKET-42"`. Gitolite will guard
    /// `refs/heads/<branch_prefix>-*`.
    #[schema(example = "TICKET-42")]
    pub branch_prefix: String,

    /// Username to add or remove (no group UUIDs here).
    #[schema(example = "alice")]
    pub name: String,
}

/// Body for DELETE /pair-programming/branch-config (drops the entire slot).
#[derive(Debug, Deserialize, ToSchema)]
pub struct PairBranchConfigRequest {
    /// Workspace identifier.
    #[schema(example = "acme")]
    pub workspace: String,

    /// Bare repo name within the workspace.
    #[schema(example = "api-service")]
    pub repo: String,

    /// Branch prefix whose entire config should be deleted.
    #[schema(example = "TICKET-42")]
    pub branch_prefix: String,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Path to the file that stores usernames for a given (workspace, repo, prefix).
fn slot_file(repo_root: &Path, workspace: &str, repo: &str, branch_prefix: &str) -> PathBuf {
    repo_root
        .join("pair-programming")
        .join(workspace)
        .join(repo)
        .join(branch_prefix)
}

/// Read a slot file; returns an empty Vec if the file does not exist yet.
async fn read_slot(path: &Path) -> Result<Vec<String>, std::io::Error> {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => Ok(content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Persist a slot file (sorted, deduplicated, trailing newline).
async fn write_slot(path: &Path, names: &[String]) -> Result<(), std::io::Error> {
    tokio::fs::create_dir_all(path.parent().unwrap()).await?;
    let mut sorted: Vec<String> = names.to_vec();
    sorted.sort();
    sorted.dedup();
    let content = if sorted.is_empty() {
        String::new()
    } else {
        format!("{}\n", sorted.join("\n"))
    };
    tokio::fs::write(path, content).await
}

/// Validate the three shared path components.
fn validate_path_parts(
    workspace: &str,
    repo: &str,
    branch_prefix: &str,
) -> Result<(), &'static str> {
    if workspace.trim().is_empty() {
        return Err("'workspace' must not be empty");
    }
    if workspace.contains('/') || workspace.contains("..") {
        return Err("'workspace' contains invalid characters");
    }
    if repo.trim().is_empty() {
        return Err("'repo' must not be empty");
    }
    if repo.contains('/') || repo.contains("..") {
        return Err("'repo' contains invalid characters");
    }
    if branch_prefix.trim().is_empty() {
        return Err("'branch_prefix' must not be empty");
    }
    if branch_prefix.contains('/') || branch_prefix.contains("..") {
        return Err("'branch_prefix' contains invalid characters");
    }
    Ok(())
}

/// Schedule a debounced push (identical pattern used in handlers.rs).
async fn schedule_push(state: &AppState) {
    let mut redis = state.0.redis.clone();
    if let Err(e) = mark_dirty(&mut redis).await {
        error!("[redis] mark_dirty failed: {e:#}");
    }
    if let Err(e) = signal_pending(&mut redis, state.0.config.debounce_secs).await {
        error!("[redis] signal_pending failed: {e:#}");
    }
}

fn bad_req(msg: &str) -> warp::reply::WithStatus<warp::reply::Json> {
    warp::reply::with_status(
        warp::reply::json(&GenericResponse {
            status: "error",
            message: Some(msg.to_string()),
        }),
        StatusCode::BAD_REQUEST,
    )
}

fn internal(msg: String) -> warp::reply::WithStatus<warp::reply::Json> {
    warp::reply::with_status(
        warp::reply::json(&GenericResponse {
            status: "error",
            message: Some(msg),
        }),
        StatusCode::INTERNAL_SERVER_ERROR,
    )
}

// ── POST /pair-programming/member ─────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/pair-programming/member",
    tag = "default",
    security(("apiISCBearerAuth" = [])),
    request_body(content = PairMemberRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "Member already present",  body = GenericResponse),
        (status = 201, description = "Member added",            body = GenericResponse),
        (status = 400, description = "Validation error",        body = GenericResponse),
        (status = 401, description = "Unauthorized",            body = GenericResponse),
        (status = 500, description = "Internal error",          body = GenericResponse),
    )
)]
pub async fn handle_add_pair_member(
    body: PairMemberRequest,
    _claims: ISCClaims,
    state: AppState,
) -> Result<impl warp::Reply, Infallible> {
    info!(
        "POST /pair-programming/member workspace={} repo={} branch_prefix={} name={} caller={}",
        body.workspace, body.repo, body.branch_prefix, body.name, _claims.sub
    );

    if let Err(msg) = validate_path_parts(&body.workspace, &body.repo, &body.branch_prefix) {
        return Ok(bad_req(msg));
    }
    if body.name.trim().is_empty() {
        return Ok(bad_req("'name' must not be empty"));
    }
    if body.name.contains('/') || body.name.contains("..") {
        return Ok(bad_req("'name' contains invalid characters"));
    }

    let repo = state.0.admin_repo.lock().await;
    let repo_root = repo.repo_path.clone();
    drop(repo);

    let path = slot_file(&repo_root, &body.workspace, &body.repo, &body.branch_prefix);

    let mut names = match read_slot(&path).await {
        Ok(n) => n,
        Err(e) => {
            error!("[pair-programming] read slot failed: {e:#}");
            return Ok(internal(e.to_string()));
        }
    };

    if names.contains(&body.name) {
        return Ok(warp::reply::with_status(
            warp::reply::json(&GenericResponse {
                status: "ok",
                message: Some(format!(
                    "'{}' already has access to {}/{} '{}' — no change",
                    body.name, body.workspace, body.repo, body.branch_prefix
                )),
            }),
            StatusCode::OK,
        ));
    }

    names.push(body.name.clone());

    if let Err(e) = write_slot(&path, &names).await {
        error!("[pair-programming] write slot failed: {e:#}");
        return Ok(internal(e.to_string()));
    }

    if let Err(e) = regenerate_conf(&repo_root).await {
        error!("[pair-programming] regenerate_conf failed: {e:#}");
        return Ok(internal(e.to_string()));
    }

    schedule_push(&state).await;

    info!(
        "[pair-programming] added '{}' to {}/{}/{}",
        body.name, body.workspace, body.repo, body.branch_prefix
    );

    Ok(warp::reply::with_status(
        warp::reply::json(&GenericResponse {
            status: "ok",
            message: Some(format!(
                "added '{}' to pair-programming slot {}/{} '{}'",
                body.name, body.workspace, body.repo, body.branch_prefix
            )),
        }),
        StatusCode::CREATED,
    ))
}

// ── DELETE /pair-programming/member ──────────────────────────────────────────

#[utoipa::path(
    delete,
    path = "/pair-programming/member",
    tag = "default",
    security(("apiISCBearerAuth" = [])),
    request_body(content = PairMemberRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "Member removed or was not present", body = GenericResponse),
        (status = 400, description = "Validation error",                  body = GenericResponse),
        (status = 401, description = "Unauthorized",                      body = GenericResponse),
        (status = 500, description = "Internal error",                    body = GenericResponse),
    )
)]
pub async fn handle_remove_pair_member(
    body: PairMemberRequest,
    _claims: ISCClaims,
    state: AppState,
) -> Result<impl warp::Reply, Infallible> {
    info!(
        "DELETE /pair-programming/member workspace={} repo={} branch_prefix={} name={} caller={}",
        body.workspace, body.repo, body.branch_prefix, body.name, _claims.sub
    );

    if let Err(msg) = validate_path_parts(&body.workspace, &body.repo, &body.branch_prefix) {
        return Ok(bad_req(msg));
    }
    if body.name.trim().is_empty() {
        return Ok(bad_req("'name' must not be empty"));
    }

    let repo = state.0.admin_repo.lock().await;
    let repo_root = repo.repo_path.clone();
    drop(repo);

    let path = slot_file(&repo_root, &body.workspace, &body.repo, &body.branch_prefix);

    let mut names = match read_slot(&path).await {
        Ok(n) => n,
        Err(e) => {
            error!("[pair-programming] read slot failed: {e:#}");
            return Ok(internal(e.to_string()));
        }
    };

    let before = names.len();
    names.retain(|n| n != &body.name);
    let removed = names.len() < before;

    if removed {
        if let Err(e) = write_slot(&path, &names).await {
            error!("[pair-programming] write slot failed: {e:#}");
            return Ok(internal(e.to_string()));
        }

        if let Err(e) = regenerate_conf(&repo_root).await {
            error!("[pair-programming] regenerate_conf failed: {e:#}");
            return Ok(internal(e.to_string()));
        }

        schedule_push(&state).await;

        info!(
            "[pair-programming] removed '{}' from {}/{}/{}",
            body.name, body.workspace, body.repo, body.branch_prefix
        );
    }

    let message = if removed {
        format!(
            "removed '{}' from pair-programming slot {}/{} '{}'",
            body.name, body.workspace, body.repo, body.branch_prefix
        )
    } else {
        format!(
            "'{}' was not in slot {}/{} '{}' — no change",
            body.name, body.workspace, body.repo, body.branch_prefix
        )
    };

    Ok(warp::reply::with_status(
        warp::reply::json(&GenericResponse {
            status: "ok",
            message: Some(message),
        }),
        StatusCode::OK,
    ))
}

// ── DELETE /pair-programming/branch-config ────────────────────────────────────

#[utoipa::path(
    delete,
    path = "/pair-programming/branch-config",
    tag = "default",
    security(("apiISCBearerAuth" = [])),
    request_body(content = PairBranchConfigRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "Branch config deleted or did not exist", body = GenericResponse),
        (status = 400, description = "Validation error",                       body = GenericResponse),
        (status = 401, description = "Unauthorized",                           body = GenericResponse),
        (status = 500, description = "Internal error",                         body = GenericResponse),
    )
)]
pub async fn handle_delete_pair_branch_config(
    body: PairBranchConfigRequest,
    _claims: ISCClaims,
    state: AppState,
) -> Result<impl warp::Reply, Infallible> {
    info!(
        "DELETE /pair-programming/branch-config workspace={} repo={} branch_prefix={} caller={}",
        body.workspace, body.repo, body.branch_prefix, _claims.sub
    );

    if let Err(msg) = validate_path_parts(&body.workspace, &body.repo, &body.branch_prefix) {
        return Ok(bad_req(msg));
    }

    let repo = state.0.admin_repo.lock().await;
    let repo_root = repo.repo_path.clone();
    drop(repo);

    let path = slot_file(&repo_root, &body.workspace, &body.repo, &body.branch_prefix);

    let existed = match tokio::fs::remove_file(&path).await {
        Ok(())                                                    => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            error!("[pair-programming] remove slot file failed: {e:#}");
            return Ok(internal(e.to_string()));
        }
    };

    if existed {
        // Best-effort: remove the parent repo directory if now empty.
        let repo_dir = path.parent().unwrap();
        let _ = tokio::fs::remove_dir(repo_dir).await; // ok if not empty

        if let Err(e) = regenerate_conf(&repo_root).await {
            error!("[pair-programming] regenerate_conf failed: {e:#}");
            return Ok(internal(e.to_string()));
        }

        schedule_push(&state).await;

        info!(
            "[pair-programming] deleted branch config {}/{}/{}",
            body.workspace, body.repo, body.branch_prefix
        );
    }

    let message = if existed {
        format!(
            "pair-programming config for {}/{} '{}' deleted",
            body.workspace, body.repo, body.branch_prefix
        )
    } else {
        format!(
            "pair-programming config for {}/{} '{}' did not exist — no change",
            body.workspace, body.repo, body.branch_prefix
        )
    };

    Ok(warp::reply::with_status(
        warp::reply::json(&GenericResponse {
            status: "ok",
            message: Some(message),
        }),
        StatusCode::OK,
    ))
}

// ── Public re-export for conf generation ──────────────────────────────────────

/// Load every pair-programming slot under `pair-programming/<workspace>/`.
///
/// Returns a map:
///   repo → branch_prefix → Vec<username>
///
/// Called by `permissions::regenerate_conf` so it can weave these rules into
/// the gitolite.conf it generates.
pub async fn load_pair_programming_slots(
    repo_root: &Path,
    workspace: &str,
) -> anyhow::Result<std::collections::BTreeMap<String, std::collections::BTreeMap<String, Vec<String>>>> {
    use std::collections::BTreeMap;

    let ws_dir = repo_root.join("pair-programming").join(workspace);
    let mut result: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();

    let mut repo_entries = match tokio::fs::read_dir(&ws_dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(result),
        Err(e) => return Err(e.into()),
    };

    while let Some(repo_entry) = repo_entries.next_entry().await? {
        if !repo_entry.file_type().await?.is_dir() {
            continue;
        }
        let repo_name = repo_entry.file_name().to_string_lossy().into_owned();
        let mut prefix_entries = tokio::fs::read_dir(repo_entry.path()).await?;

        while let Some(prefix_entry) = prefix_entries.next_entry().await? {
            if !prefix_entry.file_type().await?.is_file() {
                continue;
            }
            let prefix = prefix_entry.file_name().to_string_lossy().into_owned();
            let names = read_slot(&prefix_entry.path()).await.unwrap_or_default();
            if !names.is_empty() {
                result
                    .entry(repo_name.clone())
                    .or_default()
                    .insert(prefix, names);
            }
        }
    }

    Ok(result)
}