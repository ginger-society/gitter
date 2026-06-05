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

#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct DiffRequest {
    /// Repository name (without .git suffix)
    pub repo: String,

    /// The branch to diff against `main`
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

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct DiffResponse {
    pub repo: String,
    pub base: String,   // always "main"
    pub head: String,   // the requested branch
    pub files: Vec<FileDiff>,
    /// true when the branch cannot be merged into main without conflicts
    pub has_merge_conflicts: bool,
    /// Files that have merge conflicts (subset of `files`)
    pub conflicting_files: Vec<String>,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Resolve the bare-repo path and sanity-check it exists.
fn repo_git_path(repo: &str) -> anyhow::Result<PathBuf> {
    // Strip a trailing ".git" the caller may have included, then re-add it
    // so we always look for "<name>.git".
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
    // Sanitise the path — prevent directory traversal
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

// ── Handler: POST /repo/diff ──────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/repo/diff",
    tag = "default",
    request_body(content = DiffRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "Diff result", body = DiffResponse),
        (status = 400, description = "Validation error", body = ApiResponse),
        (status = 404, description = "Repo or branch not found", body = ApiResponse),
        (status = 500, description = "Internal error", body = ApiResponse),
    )
)]
pub async fn handle_diff(
    body: DiffRequest,
    state: AppState,
) -> Result<impl warp::Reply, Infallible> {
    if body.repo.trim().is_empty() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&ApiResponse {
                status: "error",
                message: Some("'repo' must not be empty".into()),
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

    info!(
        "POST /repo/diff repo={} branch={} vs main",
        body.repo, body.branch
    );

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

    // ── Verify both refs exist ────────────────────────────────────────────────
    for r in &["main", body.branch.as_str()] {
        if let Err(e) = git_cmd(&git_dir, &["rev-parse", "--verify", r]).await {
            return Ok(warp::reply::with_status(
                warp::reply::json(&ApiResponse {
                    status: "error",
                    message: Some(format!("ref '{r}' not found: {e}")),
                }),
                StatusCode::NOT_FOUND,
            ));
        }
    }

    // ── Diff name-status (main...branch — three-dot = merge-base diff) ────────
    //
    // Three-dot diff shows only what diverged on branch since it split from
    // main, excluding any commits that landed on main after the branch point.
    let name_status = match git_cmd(
        &git_dir,
        &["diff", "--name-status", &format!("main...{}", body.branch)],
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            error!("[git] diff --name-status failed: {e:#}");
            return Ok(warp::reply::with_status(
                warp::reply::json(&ApiResponse {
                    status: "error",
                    message: Some(e.to_string()),
                }),
                StatusCode::INTERNAL_SERVER_ERROR,
            ));
        }
    };

    // ── Collect per-file unified diffs ────────────────────────────────────────
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

        // Per-file unified diff
        let diff = git_cmd(
            &git_dir,
            &[
                "diff",
                "--unified=3",
                &format!("main...{}", body.branch),
                "--",
                &file_path,
            ],
        )
        .await
        .unwrap_or_else(|e| {
            warn!("[git] per-file diff failed for {file_path}: {e:#}");
            String::new()
        });

        files.push(FileDiff {
            path: file_path,
            status,
            diff,
        });
    }

    // ── Merge-conflict detection ──────────────────────────────────────────────
    //
    // Strategy: attempt a merge-tree (dry-run, no working-tree changes).
    // `git merge-tree` exits 0 even with conflicts, but conflict markers
    // appear in the output.  We also fall back to checking
    // `git merge-base --is-ancestor` to handle the trivial cases.
    let (has_merge_conflicts, conflicting_files) =
        detect_conflicts(&git_dir, &body.branch).await;

    Ok(warp::reply::with_status(
        warp::reply::json(&DiffResponse {
            repo: body.repo,
            base: "main".into(),
            head: body.branch,
            files,
            has_merge_conflicts,
            conflicting_files,
        }),
        StatusCode::OK,
    ))
}

// ── Conflict detection ────────────────────────────────────────────────────────

async fn detect_conflicts(git_dir: &PathBuf, branch: &str) -> (bool, Vec<String>) {
    // Fast path: if the branch is already an ancestor of main (fully merged)
    // or main is an ancestor of branch (fast-forward), there are no conflicts.
    if let Ok(_) = git_cmd(
        git_dir,
        &["merge-base", "--is-ancestor", branch, "main"],
    )
    .await
    {
        // branch is already in main — no conflict possible
        return (false, vec![]);
    }

    // Resolve commit SHAs for merge-tree
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

    // `git merge-tree <base> <main> <branch>` — outputs the merged tree and
    // embeds conflict markers when it cannot auto-resolve a hunk.
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

    // Extract conflicting file paths from the merge-tree output.
    // The format for a conflicted section looks like:
    //   changed in both
    //     base   100644 <sha> path/to/file
    //     our    100644 <sha> path/to/file
    //     their  100644 <sha> path/to/file
    let mut conflicting: Vec<String> = Vec::new();
    let mut in_conflict_block = false;

    for line in merge_tree_out.lines() {
        if line.starts_with("changed in both") || line.starts_with("added in both") {
            in_conflict_block = true;
            continue;
        }
        if in_conflict_block {
            // Lines inside the block are tab-indented and contain the path as
            // the last whitespace-separated token.
            let trimmed = line.trim();
            if trimmed.starts_with("base") || trimmed.starts_with("our") || trimmed.starts_with("their") {
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

    // Deduplicate (the loop above already does it but be safe)
    conflicting.dedup();

    (!conflicting.is_empty() || merge_tree_out.contains("<<<<<<<"), conflicting)
}