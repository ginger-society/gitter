use std::path::PathBuf;
use std::process::Command;

use git2::{Repository, ObjectType};

// ── Admin repo: still uses Command::new("git") ────────────────────────────────
// The admin repo is read via GIT_DIR env tricks (it's not necessarily a local
// bare repo we own) and we need "master:path" object notation. git2 can do
// this too, but the Command path is already battle-tested and only runs once
// per push on a small repo. Not worth changing.

pub fn read_from_admin_repo(admin_git_dir: &str, path: &str) -> Result<String, String> {
    let object = format!("master:{}", path);
    let output = Command::new("git")
        .args(["show", &object])
        .env("GIT_DIR", admin_git_dir)
        .output()
        .map_err(|e| format!("failed to run git show on admin repo: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "git show {} in admin repo failed: {}",
            object,
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn resolve_workspace(gl_repo: &str, admin_git_dir: &str) -> Result<String, String> {
    let conf = read_from_admin_repo(admin_git_dir, "conf/gitolite.conf")?;
    let mut current_workspace: Option<String> = None;
    let mut best_match: Option<(String, usize)> = None;

    for line in conf.lines() {
        if let Some(ws) = line.strip_prefix("# ── workspace: ") {
            current_workspace = Some(ws.trim_end_matches(" ──").trim().to_string());
            continue;
        }
        if let Some(pattern) = line.strip_prefix("repo ") {
            let pattern = pattern.trim();
            if let Some(ws) = &current_workspace {
                if pattern_matches(pattern, gl_repo) {
                    let specificity = pattern.len();
                    match &best_match {
                        None => best_match = Some((ws.clone(), specificity)),
                        Some((_, best_len)) if specificity > *best_len => {
                            best_match = Some((ws.clone(), specificity));
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    best_match
        .map(|(ws, _)| ws)
        .ok_or_else(|| format!("no workspace found in gitolite.conf for repo '{}'", gl_repo))
}

fn pattern_matches(pattern: &str, repo: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix(".*") {
        repo.starts_with(prefix)
    } else {
        pattern == repo
    }
}

// ── git2-based helpers ────────────────────────────────────────────────────────

fn open_bare(repo_path: &PathBuf) -> Result<Repository, String> {
    Repository::open_bare(repo_path)
        .map_err(|e| format!("failed to open bare repo at {}: {}", repo_path.display(), e))
}

/// List all .yaml/.yml files under .tekton/ in the given commit.
pub fn list_tekton_files(repo_path: &PathBuf, new_rev: &str) -> Result<Vec<String>, String> {
    let repo = open_bare(repo_path)?;

    let oid = repo
        .revparse_single(new_rev)
        .map_err(|e| format!("failed to resolve rev {}: {}", new_rev, e))?
        .id();

    let commit = repo
        .find_commit(oid)
        .map_err(|e| format!("failed to find commit {}: {}", new_rev, e))?;

    let tree = commit
        .tree()
        .map_err(|e| format!("failed to get tree for {}: {}", new_rev, e))?;

    // Walk into .tekton/ subtree only.
    let tekton_entry = match tree.get_name(".tekton") {
        Some(e) => e,
        None => return Ok(vec![]),
    };

    if tekton_entry.kind() != Some(ObjectType::Tree) {
        return Ok(vec![]);
    }

    let tekton_tree = repo
        .find_tree(tekton_entry.id())
        .map_err(|e| format!("failed to find .tekton/ tree: {}", e))?;

    let mut files = Vec::new();

    tekton_tree
        .walk(git2::TreeWalkMode::PreOrder, |root, entry| {
            if entry.kind() == Some(ObjectType::Blob) {
                let name = entry.name().unwrap_or("");
                let lower = name.to_lowercase();
                if lower.ends_with(".yaml") || lower.ends_with(".yml") {
                    let path = if root.is_empty() {
                        format!(".tekton/{}", name)
                    } else {
                        format!(".tekton/{}{}", root, name)
                    };
                    files.push(path);
                }
            }
            git2::TreeWalkResult::Ok
        })
        .map_err(|e| format!("tree walk failed: {}", e))?;

    files.sort();
    Ok(files)
}

/// Read a file's contents from a specific commit in a bare repo.
pub fn read_file_from_commit(
    repo_path: &PathBuf,
    rev: &str,
    path: &str,
) -> Result<String, String> {
    let repo = open_bare(repo_path)?;

    let object_spec = format!("{}:{}", rev, path);
    let obj = repo
        .revparse_single(&object_spec)
        .map_err(|e| format!("git show {} failed: {}", object_spec, e))?;

    let blob = obj
        .peel_to_blob()
        .map_err(|e| format!("object {} is not a blob: {}", object_spec, e))?;

    String::from_utf8(blob.content().to_vec())
        .map_err(|e| format!("file {} is not valid UTF-8: {}", path, e))
}

/// Get list of files changed between old_rev and new_rev.
pub fn get_changed_files(
    repo_path: &PathBuf,
    old_rev: &str,
    new_rev: &str,
) -> Result<Vec<String>, String> {
    let repo = open_bare(repo_path)?;
    let is_new_branch = old_rev.chars().all(|c| c == '0');

    let new_commit = repo
        .revparse_single(new_rev)
        .map_err(|e| format!("failed to resolve {}: {}", new_rev, e))?
        .peel_to_commit()
        .map_err(|e| format!("failed to peel {} to commit: {}", new_rev, e))?;

    let new_tree = new_commit
        .tree()
        .map_err(|e| format!("failed to get tree for {}: {}", new_rev, e))?;

    let diff = if is_new_branch {
        // New branch: diff the tip commit against its first parent, or against
        // an empty tree if it's the very first commit in the repo.
        let parent_tree = new_commit.parent(0).ok().and_then(|p| p.tree().ok());
        repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&new_tree), None)
            .map_err(|e| format!("diff_tree_to_tree failed: {}", e))?
    } else {
        let old_commit = repo
            .revparse_single(old_rev)
            .map_err(|e| format!("failed to resolve {}: {}", old_rev, e))?
            .peel_to_commit()
            .map_err(|e| format!("failed to peel {} to commit: {}", old_rev, e))?;
        let old_tree = old_commit
            .tree()
            .map_err(|e| format!("failed to get tree for {}: {}", old_rev, e))?;
        repo.diff_tree_to_tree(Some(&old_tree), Some(&new_tree), None)
            .map_err(|e| format!("diff_tree_to_tree failed: {}", e))?
    };

    let mut files = Vec::new();
    diff.foreach(
        &mut |delta, _progress| {
            if let Some(path) = delta.new_file().path().or_else(|| delta.old_file().path()) {
                let s = path.to_string_lossy().into_owned();
                if !s.is_empty() {
                    files.push(s);
                }
            }
            true
        },
        None,
        None,
        None,
    )
    .map_err(|e| format!("diff foreach failed: {}", e))?;

    Ok(files)
}