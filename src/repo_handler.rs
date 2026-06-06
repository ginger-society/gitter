use std::convert::Infallible;
use std::path::PathBuf;

use git2::{
    Delta, DiffOptions, MergeOptions, Oid, Repository, Sort,
};
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
    pub repo: String,
    pub branch: Option<String>,
    pub tag: Option<String>,
    pub path: String,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct FileContentResponse {
    pub repo: String,
    pub r#ref: String,
    pub path: String,
    pub content: String,
    pub cached: bool,
}

#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct OrgDiffRequest {
    /// Organisation identifier — repos must be named `{org_id}-{anything}`
    pub org_id: String,
    /// Branch name to diff against `main` in every matching repo
    pub branch: String,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct DiffLine {
    pub origin: char,
    pub content: String,
    pub highlighted_light: String,
    pub highlighted_dark: String,
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct FileDiff {
    pub path: String,
    pub status: DiffStatus,
    pub diff: String,
    pub lines: Vec<DiffLine>,
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
    pub repos: Vec<RepoDiff>,
    pub skipped_repos: Vec<String>,
}

// ── New: commits endpoint types ───────────────────────────────────────────────

#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct OrgCommitsRequest {
    pub org_id: String,
    pub branch: String,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct BranchCommit {
    pub sha: String,
    pub short_sha: String,
    pub subject: String,       // first line of commit message
    pub body: String,          // remainder of commit message (may be empty)
    pub author: String,
    pub author_email: String,
    pub timestamp: i64,        // unix epoch seconds (UTC)
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct RepoBranchCommits {
    pub repo: String,
    /// Commits reachable from branch HEAD but NOT from merge-base with main,
    /// ordered newest-first.
    pub commits: Vec<BranchCommit>,
    /// Pre-filled suggested squash message:
    ///   - 1 commit  → that commit's full message
    ///   - >1 commit → empty string (author must write one)
    pub suggested_message: String,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct OrgBranchCommitsResponse {
    pub org_id: String,
    pub branch: String,
    pub repos: Vec<RepoBranchCommits>,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Resolve and open a bare repo, rejecting path-traversal attempts.
fn open_repo(repo_name: &str) -> anyhow::Result<Repository> {
    let name = repo_name.trim_end_matches(".git");

    if name.is_empty() || name.contains('/') || name.contains("..") {
        anyhow::bail!("invalid repo name: {repo_name:?}");
    }

    let path = PathBuf::from(REPOS_ROOT).join(format!("{name}.git"));
    if !path.exists() {
        anyhow::bail!("repository '{name}' not found");
    }

    Ok(Repository::open_bare(&path)?)
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
            names.push(fname.trim_end_matches(".git").to_string());
        }
    }

    names.sort();
    Ok(names)
}

fn cache_key(repo: &str, git_ref: &str, path: &str) -> String {
    format!("gitter:file:{repo}:{git_ref}:{path}")
}

// ── Core git logic ────────────────────────────────────────────────────────────

/// Read a single file from a bare repo at the given ref.
fn read_file_from_repo(repo: &Repository, git_ref: &str, file_path: &str) -> anyhow::Result<String> {
    let clean: String = file_path
        .split('/')
        .filter(|s| !s.is_empty() && *s != "..")
        .collect::<Vec<_>>()
        .join("/");

    let obj = repo.revparse_single(&format!("{git_ref}:{clean}"))?;
    let blob = obj.peel_to_blob()?;
    Ok(String::from_utf8_lossy(blob.content()).to_string())
}

/// Resolve a branch name to its tip commit Oid.
/// Returns None if the branch does not exist.
fn resolve_branch(repo: &Repository, branch: &str) -> Option<Oid> {
    repo.find_branch(branch, git2::BranchType::Local)
        .ok()
        .and_then(|b| b.get().target())
}

/// Find the merge-base between main and branch.
fn find_merge_base(repo: &Repository, main_oid: Oid, branch_oid: Oid) -> anyhow::Result<Oid> {
    Ok(repo.merge_base(main_oid, branch_oid)?)
}

fn highlight_line(
    content: &str,
    extension: &str,
    highlighter: &crate::state::HighlighterState,
) -> (String, String) {
    highlighter.highlight_line(content, extension)
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
     .replace('<', "&lt;")
     .replace('>', "&gt;")
}

/// Core diff logic. Returns None if branch does not exist in this repo.
fn diff_repo(repo: &Repository, repo_name: &str, branch: &str, highlighter: &crate::state::HighlighterState) -> Option<RepoDiff> {
    let main_oid = repo
        .find_branch("main", git2::BranchType::Local)
        .ok()?
        .get()
        .target()?;

    let branch_oid = resolve_branch(repo, branch)?;

    let base_oid = match find_merge_base(repo, main_oid, branch_oid) {
        Ok(o) => o,
        Err(e) => {
            warn!("[diff] merge_base failed for {repo_name}: {e:#}");
            return None;
        }
    };

    let base_commit = repo.find_commit(base_oid).ok()?;
    let branch_commit = repo.find_commit(branch_oid).ok()?;

    let base_tree = base_commit.tree().ok()?;
    let branch_tree = branch_commit.tree().ok()?;

    let mut diff_opts = DiffOptions::new();
    diff_opts.context_lines(3);

    let diff = match repo.diff_tree_to_tree(
        Some(&base_tree),
        Some(&branch_tree),
        Some(&mut diff_opts),
    ) {
        Ok(d) => d,
        Err(e) => {
            error!("[diff] diff_tree_to_tree failed for {repo_name}: {e:#}");
            return None;
        }
    };

    let mut files: Vec<FileDiff> = Vec::new();
    let mut current_path = String::new();
    let mut current_status = DiffStatus::Unknown;
    let mut current_diff = String::new();
    let mut current_lines: Vec<DiffLine> = Vec::new();

    let _ = diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        if !current_path.is_empty() && path != current_path {
            files.push(FileDiff {
                path: current_path.clone(),
                status: std::mem::replace(&mut current_status, DiffStatus::Unknown),
                diff: std::mem::take(&mut current_diff),
                lines: std::mem::take(&mut current_lines),
            });
        }

        if path != current_path {
            current_path = path;
            current_status = match delta.status() {
                Delta::Added    => DiffStatus::Added,
                Delta::Deleted  => DiffStatus::Deleted,
                Delta::Modified => DiffStatus::Modified,
                Delta::Renamed  => DiffStatus::Renamed,
                Delta::Copied   => DiffStatus::Copied,
                _               => DiffStatus::Unknown,
            };
        }

        let origin = line.origin();

        if let Ok(content) = std::str::from_utf8(line.content()) {
            match origin {
                '+' | '-' | ' ' => {
                    current_diff.push(origin);
                    current_diff.push_str(content);

                    let ext = std::path::Path::new(&current_path)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("txt");

                    let trimmed = content.trim_end_matches('\n');
                    let (highlighted_light, highlighted_dark) =
                        highlighter.highlight_line(trimmed, ext);

                    current_lines.push(DiffLine {
                        origin,
                        content: trimmed.to_string(),
                        highlighted_light,
                        highlighted_dark,
                        old_lineno: line.old_lineno(),
                        new_lineno: line.new_lineno(),
                    });
                }
                _ => {
                    current_diff.push_str(content);
                }
            }
        }

        true
    });

    if !current_path.is_empty() {
        files.push(FileDiff {
            path: current_path,
            status: current_status,
            diff: current_diff,
            lines: current_lines,
        });
    }

    let (has_merge_conflicts, conflicting_files) = detect_conflicts(repo, main_oid, branch_oid);

    Some(RepoDiff {
        repo: repo_name.to_string(),
        base: "main".into(),
        head: branch.to_string(),
        files,
        has_merge_conflicts,
        conflicting_files,
    })
}

/// Returns commits reachable from branch_oid but not from merge-base with main,
/// newest first. This is equivalent to `git log main..branch`.
fn commits_since_branch(
    repo: &Repository,
    repo_name: &str,
    branch: &str,
) -> Option<RepoBranchCommits> {
    let main_oid = repo
        .find_branch("main", git2::BranchType::Local)
        .ok()?
        .get()
        .target()?;

    let branch_oid = resolve_branch(repo, branch)?;

    let base_oid = match find_merge_base(repo, main_oid, branch_oid) {
        Ok(o) => o,
        Err(e) => {
            warn!("[commits] merge_base failed for {repo_name}: {e:#}");
            return None;
        }
    };

    let mut revwalk = repo.revwalk().ok()?;
    revwalk.set_sorting(Sort::TIME).ok()?;
    revwalk.push(branch_oid).ok()?;
    revwalk.hide(base_oid).ok()?;

    let commits: Vec<BranchCommit> = revwalk
        .filter_map(|oid| {
            let oid = oid.ok()?;
            let commit = repo.find_commit(oid).ok()?;

            let full_message = commit.message().unwrap_or("").to_string();
            let mut lines = full_message.splitn(2, '\n');
            let subject = lines.next().unwrap_or("").trim().to_string();
            let body = lines.next().unwrap_or("").trim_start_matches('\n').to_string();

            Some(BranchCommit {
                sha: oid.to_string(),
                short_sha: oid.to_string()[..7].to_string(),
                subject,
                body,
                author: commit.author().name().unwrap_or("").to_string(),
                author_email: commit.author().email().unwrap_or("").to_string(),
                timestamp: commit.time().seconds(),
            })
        })
        .collect();

    // Pre-fill suggested message only when there is exactly one commit —
    // no guessing needed, the author already wrote it.
    let suggested_message = if commits.len() == 1 {
        let c = &commits[0];
        if c.body.is_empty() {
            c.subject.clone()
        } else {
            format!("{}\n\n{}", c.subject, c.body)
        }
    } else {
        String::new()
    };

    Some(RepoBranchCommits {
        repo: repo_name.to_string(),
        commits,
        suggested_message,
    })
}

/// Detect merge conflicts using git2's in-memory merge.
fn detect_conflicts(
    repo: &Repository,
    main_oid: Oid,
    branch_oid: Oid,
) -> (bool, Vec<String>) {
    // If branch is already an ancestor of main there are no conflicts.
    if repo.merge_base(main_oid, branch_oid)
        .map(|base| base == branch_oid)
        .unwrap_or(false)
    {
        return (false, vec![]);
    }

    let main_commit   = match repo.find_commit(main_oid)   { Ok(c) => c, Err(_) => return (false, vec![]) };
    let branch_commit = match repo.find_commit(branch_oid) { Ok(c) => c, Err(_) => return (false, vec![]) };

    let main_tree   = match main_commit.tree()   { Ok(t) => t, Err(_) => return (false, vec![]) };
    let branch_tree = match branch_commit.tree() { Ok(t) => t, Err(_) => return (false, vec![]) };

    let base_oid    = match repo.merge_base(main_oid, branch_oid) { Ok(o) => o, Err(_) => return (false, vec![]) };
    let base_commit = match repo.find_commit(base_oid)  { Ok(c) => c, Err(_) => return (false, vec![]) };
    let base_tree   = match base_commit.tree()           { Ok(t) => t, Err(_) => return (false, vec![]) };

    let mut merge_opts = MergeOptions::new();
    let index = match repo.merge_trees(&base_tree, &main_tree, &branch_tree, Some(&merge_opts)) {
        Ok(idx) => idx,
        Err(e) => {
            warn!("[conflict] merge_trees failed: {e:#}");
            return (false, vec![]);
        }
    };

    if !index.has_conflicts() {
        return (false, vec![]);
    }

    let conflicting: Vec<String> = index
        .conflicts()
        .map(|conflicts| {
            conflicts
                .filter_map(|c| {
                    let entry = c.ok()?;
                    // any of ancestor/our/their path will do
                    let path = entry.our
                        .or(entry.their)
                        .or(entry.ancestor)?;
                    String::from_utf8(path.path).ok()
                })
                .collect()
        })
        .unwrap_or_default();

    (!conflicting.is_empty(), conflicting)
}

// ── Handler: POST /repo/file ──────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/repo/file",
    tag = "default",
    request_body(content = FileContentRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "File content",       body = FileContentResponse),
        (status = 400, description = "Validation error",   body = ApiResponse),
        (status = 404, description = "Repo/file not found",body = ApiResponse),
        (status = 500, description = "Internal error",     body = ApiResponse),
    )
)]
pub async fn handle_file_content(
    body: FileContentRequest,
    state: AppState,
) -> Result<impl warp::Reply, Infallible> {
    let git_ref = match (&body.branch, &body.tag) {
        (Some(b), None)  => b.clone(),
        (None,  Some(t)) => t.clone(),
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
        return Ok(bad_request("'repo' must not be empty"));
    }
    if body.path.trim().is_empty() {
        return Ok(bad_request("'path' must not be empty"));
    }

    info!("POST /repo/file repo={} ref={} path={}", body.repo, git_ref, body.path);

    // ── Cache lookup ──────────────────────────────────────────────────────────
    let key = cache_key(&body.repo, &git_ref, &body.path);
    let mut redis = state.0.redis.clone();

    match redis.get::<_, Option<String>>(&key).await {
        Ok(Some(cached)) => {
            info!("[cache] HIT {key}");
            return Ok(warp::reply::with_status(
                warp::reply::json(&FileContentResponse {
                    repo: body.repo,
                    r#ref: git_ref,
                    path: body.path,
                    content: cached,
                    cached: true,
                }),
                StatusCode::OK,
            ));
        }
        Ok(None)  => info!("[cache] MISS {key}"),
        Err(e)    => warn!("[cache] redis error (continuing without cache): {e:#}"),
    }

    // ── Read from bare repo ───────────────────────────────────────────────────
    let repo = match open_repo(&body.repo) {
        Ok(r)  => r,
        Err(e) => return Ok(not_found(e.to_string())),
    };

    let content = match read_file_from_repo(&repo, &git_ref, &body.path) {
        Ok(c)  => c,
        Err(e) => {
            let msg = e.to_string();
            error!("[git] read_file failed: {msg}");
            return Ok(not_found(msg));
        }
    };

    // ── Populate cache ────────────────────────────────────────────────────────
    if let Err(e) = redis.set_ex::<_, _, ()>(&key, &content, FILE_CACHE_TTL).await {
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

#[utoipa::path(
    post,
    path = "/org/diff",
    tag = "default",
    request_body(content = OrgDiffRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "Org-wide diff result", body = OrgDiffResponse),
        (status = 400, description = "Validation error",     body = ApiResponse),
        (status = 500, description = "Internal error",       body = ApiResponse),
    )
)]
pub async fn handle_org_diff(
    body: OrgDiffRequest,
    state: AppState,
) -> Result<impl warp::Reply, Infallible> {
    if let Err(r) = validate_org_branch(&body.org_id, &body.branch) {
        return Ok(r);
    }

    info!("POST /org/diff org_id={} branch={} vs main", body.org_id, body.branch);

    let repo_names = match list_org_repos(&body.org_id) {
        Ok(v)  => v,
        Err(e) => {
            error!("[org/diff] failed to list repos: {e:#}");
            return Ok(internal_error(format!("failed to enumerate repositories: {e}")));
        }
    };

    info!(
        "[org/diff] found {} repo(s) for org '{}': {:?}",
        repo_names.len(), body.org_id, repo_names
    );

    let mut handles = Vec::with_capacity(repo_names.len());
    for repo_name in repo_names {
        let branch = body.branch.clone();
        // Clone the Arc so the blocking task owns it
        let state = state.clone();
        handles.push(tokio::task::spawn_blocking(move || {
            let repo = match open_repo(&repo_name) {
                Ok(r)  => r,
                Err(e) => { warn!("[org/diff] open_repo({repo_name}) failed: {e:#}"); return (repo_name, None); }
            };
            let result = diff_repo(&repo, &repo_name, &branch, &state.0.highlighter);
            (repo_name, result)
        }));
    }

    let mut repos: Vec<RepoDiff> = Vec::new();
    let mut skipped_repos: Vec<String> = Vec::new();

    for handle in handles {
        match handle.await {
            Ok((_, Some(diff))) => repos.push(diff),
            Ok((name, None))    => skipped_repos.push(name),
            Err(e)              => error!("[org/diff] task panicked: {e:#}"),
        }
    }

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

// ── Handler: POST /org/commits ────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/org/commits",
    tag = "default",
    request_body(content = OrgCommitsRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "Commits per repo since branch point", body = OrgBranchCommitsResponse),
        (status = 400, description = "Validation error",                    body = ApiResponse),
        (status = 500, description = "Internal error",                      body = ApiResponse),
    )
)]
pub async fn handle_org_commits(
    body: OrgCommitsRequest,
    _state: AppState,
) -> Result<impl warp::Reply, Infallible> {
    if let Err(r) = validate_org_branch(&body.org_id, &body.branch) {
        return Ok(r);
    }

    info!("POST /org/commits org_id={} branch={}", body.org_id, body.branch);

    let repo_names = match list_org_repos(&body.org_id) {
        Ok(v)  => v,
        Err(e) => {
            error!("[org/commits] failed to list repos: {e:#}");
            return Ok(internal_error(format!("failed to enumerate repositories: {e}")));
        }
    };

    let mut handles = Vec::with_capacity(repo_names.len());
    for repo_name in repo_names {
        let branch = body.branch.clone();
        handles.push(tokio::task::spawn_blocking(move || {
            let repo = match open_repo(&repo_name) {
                Ok(r)  => r,
                Err(e) => { warn!("[org/commits] open_repo({repo_name}) failed: {e:#}"); return (repo_name, None); }
            };
            let result = commits_since_branch(&repo, &repo_name, &branch);
            (repo_name, result)
        }));
    }

    let mut repos: Vec<RepoBranchCommits> = Vec::new();
    for handle in handles {
        match handle.await {
            Ok((_, Some(r))) => repos.push(r),
            Ok((name, None)) => info!("[org/commits] {name} skipped (branch absent)"),
            Err(e)           => error!("[org/commits] task panicked: {e:#}"),
        }
    }

    repos.sort_by(|a, b| a.repo.cmp(&b.repo));

    Ok(warp::reply::with_status(
        warp::reply::json(&OrgBranchCommitsResponse {
            org_id: body.org_id,
            branch: body.branch,
            repos,
        }),
        StatusCode::OK,
    ))
}

// ── Small reply helpers ───────────────────────────────────────────────────────

type JsonReply = warp::reply::WithStatus<warp::reply::Json>;

fn bad_request(msg: impl Into<String>) -> JsonReply {
    warp::reply::with_status(
        warp::reply::json(&ApiResponse { status: "error", message: Some(msg.into()) }),
        StatusCode::BAD_REQUEST,
    )
}

fn not_found(msg: impl Into<String>) -> JsonReply {
    warp::reply::with_status(
        warp::reply::json(&ApiResponse { status: "error", message: Some(msg.into()) }),
        StatusCode::NOT_FOUND,
    )
}

fn internal_error(msg: impl Into<String>) -> JsonReply {
    warp::reply::with_status(
        warp::reply::json(&ApiResponse { status: "error", message: Some(msg.into()) }),
        StatusCode::INTERNAL_SERVER_ERROR,
    )
}

/// Shared validation for org_id + branch used by both org endpoints.
fn validate_org_branch(org_id: &str, branch: &str) -> Result<(), JsonReply> {
    if org_id.trim().is_empty() {
        return Err(bad_request("'org_id' must not be empty"));
    }
    if org_id.contains('/') || org_id.contains("..") {
        return Err(bad_request("'org_id' contains invalid characters"));
    }
    if branch.trim().is_empty() {
        return Err(bad_request("'branch' must not be empty"));
    }
    if branch == "main" {
        return Err(bad_request("'branch' must not be 'main' — diff is always against main"));
    }
    Ok(())
}