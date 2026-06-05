use std::convert::Infallible;
use std::path::PathBuf;

use redis::AsyncCommands;
use tracing::{error, info, warn};
use warp::http::StatusCode;

use crate::requests::ApiResponse;
use crate::state::AppState;

// ── Constants ─────────────────────────────────────────────────────────────────

const REPOS_ROOT: &str = "/home/git/repositories";
const FILE_CACHE_TTL: u64 = 10; // seconds

// ── Request / Response types ──────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct FileContentRequest {
    /// Repository name (without .git suffix)
    pub repo: String,

    /// Branch name OR tag name — exactly one must be provided
    pub branch: Option<String>,
    pub tag: Option<String>,

    /// File path relative to the repo root, e.g. "src/main.rs"
    pub path: String,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct FileContentResponse {
    pub repo: String,
    pub r#ref: String,
    pub path: String,
    pub content: String,
    /// true when the response was served from Redis cache
    pub cached: bool,
}

/// Request for an org-wide diff: finds every repo whose name starts with
/// `{org_id}-` and diffs the named branch (if it exists) against `main`.
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct OrgDiffRequest {
    /// Organisation identifier — repos must be named `{org_id}-{anything}`
    pub org_id: String,

    /// Branch name to diff against `main` in every matching repo
    pub branch: String,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct FileDiff {
    /// Relative file path
    pub path: String,
    pub status: DiffStatus,
    /// Unified diff for this file (empty for binary files or pure adds/deletes
    /// without content)
    pub diff: String,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum DiffStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Unknown,
}

/// One entry in the org-wide diff response — one per repo that had the branch.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct RepoDiff {
    /// Repo name (without .git suffix), e.g. "acme-backend"
    pub repo: String,
    pub base: String,
    pub head: String,
    pub files: Vec<FileDiff>,
    pub has_merge_conflicts: bool,
    pub conflicting_files: Vec<String>,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct OrgDiffResponse {
    pub org_id: String,
    pub branch: String,
    /// Only repos that actually have the branch are included.
    pub repos: Vec<RepoDiff>,
    /// Repo names that were found but did NOT have the branch — informational.
    pub skipped_repos: Vec<String>,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Resolve the bare-repo path and sanity-check it exists.
fn repo_git_path(repo: &str) -> anyhow::Result<PathBuf> {
    let name = repo.trim_end_matches(".git");

    if name.is_empty() || name.contains('/') || name.contains("..") {
        anyhow::bail!("invalid repo name: {repo:?}");
    }

    let path = PathBuf::from(REPOS_ROOT).join(format!("{name}.git"));
    if !path.exists() {
        anyhow::bail!("repository '{name}' not found");
    }
    Ok(path)
}

/// Run a git command inside `git_dir` (a bare repo) and return stdout.
async fn git_cmd(git_dir: &PathBuf, args: &[&str]) -> anyhow::Result<String> {
    let out = tokio::process::Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .output()
        .await?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        anyhow::bail!("git {}: {stderr}", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Read a single file from a bare repo at the given ref.
async fn read_file(git_dir: &PathBuf, git_ref: &str, file_path: &str) -> anyhow::Result<String> {
    let clean: String = file_path
        .split('/')
        .filter(|s| !s.is_empty() && *s != "..")
        .collect::<Vec<_>>()
        .join("/");

    let object = format!("{git_ref}:{clean}");
    let content = git_cmd(git_dir, &["cat-file", "blob", &object]).await?;
    Ok(content)
}

/// Build the Redis cache key for a file-content request.
fn cache_key(repo: &str, git_ref: &str, path: &str) -> String {
    format!("gitter:file:{repo}:{git_ref}:{path}")
}

/// Core diff logic, shared by the single-repo and org-wide endpoints.
/// Returns `None` if the branch does not exist in `git_dir`.
async fn diff_repo(git_dir: &PathBuf, repo_name: &str, branch: &str) -> Option<RepoDiff> {
    // Check both required refs exist; bail silently if the branch is absent.
    if git_cmd(git_dir, &["rev-parse", "--verify", "main"]).await.is_err() {
        warn!("[diff] repo {repo_name}: 'main' ref not found — skipping");
        return None;
    }
    if git_cmd(git_dir, &["rev-parse", "--verify", branch]).await.is_err() {
        // Branch simply doesn't exist in this repo — not an error.
        return None;
    }

    // name-status diff (three-dot = from merge-base)
    let name_status = match git_cmd(
        git_dir,
        &["diff", "--name-status", &format!("main...{branch}")],
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            error!("[git] diff --name-status failed for {repo_name}: {e:#}");
            return None;
        }
    };

    let mut files: Vec<FileDiff> = Vec::new();

    for line in name_status.lines().filter(|l| !l.trim().is_empty()) {
        let mut parts = line.splitn(2, '\t');
        let status_char = parts.next().unwrap_or("?").trim();
        let file_path = parts.next().unwrap_or("").trim().to_string();

        if file_path.is_empty() {
            continue;
        }

        let status = match status_char.chars().next().unwrap_or('?') {
            'A' => DiffStatus::Added,
            'M' => DiffStatus::Modified,
            'D' => DiffStatus::Deleted,
            'R' => DiffStatus::Renamed,
            'C' => DiffStatus::Copied,
            _   => DiffStatus::Unknown,
        };

        let diff = git_cmd(
            git_dir,
            &[
                "diff",
                "--unified=3",
                &format!("main...{branch}"),
                "--",
                &file_path,
            ],
        )
        .await
        .unwrap_or_else(|e| {
            warn!("[git] per-file diff failed for {file_path} in {repo_name}: {e:#}");
            String::new()
        });

        files.push(FileDiff { path: file_path, status, diff });
    }

    let (has_merge_conflicts, conflicting_files) =
        detect_conflicts(git_dir, branch).await;

    Some(RepoDiff {
        repo: repo_name.to_string(),
        base: "main".into(),
        head: branch.to_string(),
        files,
        has_merge_conflicts,
        conflicting_files,
    })
}

/// Enumerate every `<org_id>-*.git` directory under REPOS_ROOT.
fn list_org_repos(org_id: &str) -> anyhow::Result<Vec<String>> {
    let prefix = format!("{org_id}-");
    let mut names = Vec::new();

    for entry in std::fs::read_dir(REPOS_ROOT)? {
        let entry = entry?;
        let fname = entry.file_name();
        let fname = fname.to_string_lossy();

        if fname.starts_with(&prefix) && fname.ends_with(".git") {
            // Strip the trailing ".git" to get the canonical repo name.
            let name = fname.trim_end_matches(".git").to_string();
            names.push(name);
        }
    }

    names.sort(); // deterministic ordering in the response
    Ok(names)
}

// ── Handler: POST /repo/file ──────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/repo/file",
    tag = "default",
    request_body(content = FileContentRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "File content", body = FileContentResponse),
        (status = 400, description = "Validation error", body = ApiResponse),
        (status = 404, description = "Repo or file not found", body = ApiResponse),
        (status = 500, description = "Internal error", body = ApiResponse),
    )
)]
pub async fn handle_file_content(
    body: FileContentRequest,
    state: AppState,
) -> Result<impl warp::Reply, Infallible> {
    // ── Resolve the ref ───────────────────────────────────────────────────────
    let git_ref = match (&body.branch, &body.tag) {
        (Some(b), None) => b.clone(),
        (None, Some(t)) => t.clone(),
        (Some(_), Some(_)) => {
            return Ok(warp::reply::with_status(
                warp::reply::json(&ApiResponse {
                    status: "error",
                    message: Some("provide either 'branch' or 'tag', not both".into()),
                }),
                StatusCode::BAD_REQUEST,
            ));
        }
        (None, None) => {
            return Ok(warp::reply::with_status(
                warp::reply::json(&ApiResponse {
                    status: "error",
                    message: Some("one of 'branch' or 'tag' is required".into()),
                }),
                StatusCode::BAD_REQUEST,
            ));
        }
    };

    if body.repo.trim().is_empty() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "error",
                message: Some("'repo' must not be empty".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }
    if body.path.trim().is_empty() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "error",
                message: Some("'path' must not be empty".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }

    info!(
        "POST /repo/file repo={} ref={} path={}",
        body.repo, git_ref, body.path
    );

    // ── Cache lookup ──────────────────────────────────────────────────────────
    let key = cache_key(&body.repo, &git_ref, &body.path);
    let mut redis = state.0.redis.clone();

    match redis.get::<_, Option<String>>(&key).await {
        Ok(Some(cached)) => {
            info!("[cache] HIT {key}");
            let resp = FileContentResponse {
                repo: body.repo.clone(),
                r#ref: git_ref.clone(),
                path: body.path.clone(),
                content: cached,
                cached: true,
            };
            return Ok(warp::reply::with_status(
                warp::reply::json(&resp),
                StatusCode::OK,
            ));
        }
        Ok(None) => info!("[cache] MISS {key}"),
        Err(e) => warn!("[cache] redis error (continuing without cache): {e:#}"),
    }

    // ── Read from bare repo ───────────────────────────────────────────────────
    let git_dir = match repo_git_path(&body.repo) {
        Ok(p) => p,
        Err(e) => {
            return Ok(warp::reply::with_status(
                warp::reply::json(&ApiResponse {
                    status: "error",
                    message: Some(e.to_string()),
                }),
                StatusCode::NOT_FOUND,
            ));
        }
    };

    let content = match read_file(&git_dir, &git_ref, &body.path).await {
        Ok(c) => c,
        Err(e) => {
            let msg = e.to_string();
            let code = if msg.contains("not found") || msg.contains("does not exist") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            error!("[git] read_file failed: {msg}");
            return Ok(warp::reply::with_status(
                warp::reply::json(&ApiResponse {
                    status: "error",
                    message: Some(msg),
                }),
                code,
            ));
        }
    };

    // ── Populate cache ────────────────────────────────────────────────────────
    if let Err(e) = redis
        .set_ex::<_, _, ()>(&key, &content, FILE_CACHE_TTL)
        .await
    {
        warn!("[cache] failed to write {key}: {e:#}");
    }

    Ok(warp::reply::with_status(
        warp::reply::json(&FileContentResponse {
            repo: body.repo,
            r#ref: git_ref,
            path: body.path,
            content,
            cached: false,
        }),
        StatusCode::OK,
    ))
}

// ── Handler: POST /org/diff ───────────────────────────────────────────────────
//
// Scans REPOS_ROOT for every directory named `{org_id}-*.git`, then diffs
// the requested branch against `main` in each one.  Repos where the branch
// does not exist are silently skipped and listed in `skipped_repos`.

#[utoipa::path(
    post,
    path = "/org/diff",
    tag = "default",
    request_body(content = OrgDiffRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "Org-wide diff result", body = OrgDiffResponse),
        (status = 400, description = "Validation error", body = ApiResponse),
        (status = 500, description = "Internal error", body = ApiResponse),
    )
)]
pub async fn handle_org_diff(
    body: OrgDiffRequest,
    _state: AppState,
) -> Result<impl warp::Reply, Infallible> {
    if body.org_id.trim().is_empty() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "error",
                message: Some("'org_id' must not be empty".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }
    if body.branch.trim().is_empty() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "error",
                message: Some("'branch' must not be empty".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }
    if body.branch == "main" {
        return Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "error",
                message: Some("'branch' must not be 'main' — diff is always against main".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }
    // Reject org_id values that could escape the prefix match (e.g. "../other")
    if body.org_id.contains('/') || body.org_id.contains("..") {
        return Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "error",
                message: Some("'org_id' contains invalid characters".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }

    info!(
        "POST /org/diff org_id={} branch={} vs main",
        body.org_id, body.branch
    );

    // Enumerate repos belonging to this org
    let repo_names = match list_org_repos(&body.org_id) {
        Ok(v) => v,
        Err(e) => {
            error!("[org/diff] failed to list repos: {e:#}");
            return Ok(warp::reply::with_status(
                warp::reply::json(&ApiResponse {
                    status: "error",
                    message: Some(format!("failed to enumerate repositories: {e}")),
                }),
                StatusCode::INTERNAL_SERVER_ERROR,
            ));
        }
    };

    info!(
        "[org/diff] found {} repo(s) for org '{}': {:?}",
        repo_names.len(),
        body.org_id,
        repo_names
    );

    let mut repos: Vec<RepoDiff> = Vec::new();
    let mut skipped_repos: Vec<String> = Vec::new();

    // Run diffs concurrently — one task per repo
    let mut handles = Vec::with_capacity(repo_names.len());

    for repo_name in repo_names {
        let branch = body.branch.clone();
        handles.push(tokio::spawn(async move {
            // repo_git_path is infallible here because list_org_repos already
            // confirmed the directory exists; we just need the PathBuf.
            let git_dir = match repo_git_path(&repo_name) {
                Ok(p) => p,
                Err(e) => {
                    warn!("[org/diff] repo_git_path({repo_name}) failed: {e:#}");
                    return (repo_name, None);
                }
            };
            let result = diff_repo(&git_dir, &repo_name, &branch).await;
            (repo_name, result)
        }));
    }

    for handle in handles {
        match handle.await {
            Ok((repo_name, Some(diff))) => repos.push(diff),
            Ok((repo_name, None))       => skipped_repos.push(repo_name),
            Err(e) => error!("[org/diff] task panicked: {e:#}"),
        }
    }

    // Keep a consistent order
    repos.sort_by(|a, b| a.repo.cmp(&b.repo));
    skipped_repos.sort();

    Ok(warp::reply::with_status(
        warp::reply::json(&OrgDiffResponse {
            org_id: body.org_id,
            branch: body.branch,
            repos,
            skipped_repos,
        }),
        StatusCode::OK,
    ))
}

// ── Conflict detection ────────────────────────────────────────────────────────

async fn detect_conflicts(git_dir: &PathBuf, branch: &str) -> (bool, Vec<String>) {
    // Fast path: if the branch is already an ancestor of main there are no conflicts.
    if git_cmd(git_dir, &["merge-base", "--is-ancestor", branch, "main"])
        .await
        .is_ok()
    {
        return (false, vec![]);
    }

    let main_sha = match git_cmd(git_dir, &["rev-parse", "main"]).await {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            warn!("[conflict] could not resolve main SHA: {e:#}");
            return (false, vec![]);
        }
    };
    let branch_sha = match git_cmd(git_dir, &["rev-parse", branch]).await {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            warn!("[conflict] could not resolve branch SHA: {e:#}");
            return (false, vec![]);
        }
    };
    let merge_base = match git_cmd(git_dir, &["merge-base", &main_sha, &branch_sha]).await {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            warn!("[conflict] merge-base failed: {e:#}");
            return (false, vec![]);
        }
    };

    let merge_tree_out = match git_cmd(
        git_dir,
        &["merge-tree", &merge_base, &main_sha, &branch_sha],
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            warn!("[conflict] merge-tree failed: {e:#}");
            return (false, vec![]);
        }
    };

    if !merge_tree_out.contains("<<<<<<<") {
        return (false, vec![]);
    }

    let mut conflicting: Vec<String> = Vec::new();
    let mut in_conflict_block = false;

    for line in merge_tree_out.lines() {
        if line.starts_with("changed in both") || line.starts_with("added in both") {
            in_conflict_block = true;
            continue;
        }
        if in_conflict_block {
            let trimmed = line.trim();
            if trimmed.starts_with("base")
                || trimmed.starts_with("our")
                || trimmed.starts_with("their")
            {
                if let Some(path) = trimmed.split_whitespace().last() {
                    let p = path.to_string();
                    if !conflicting.contains(&p) {
                        conflicting.push(p);
                    }
                }
            } else {
                in_conflict_block = false;
            }
        }
    }

    conflicting.dedup();
    (!conflicting.is_empty() || merge_tree_out.contains("<<<<<<<"), conflicting)
}