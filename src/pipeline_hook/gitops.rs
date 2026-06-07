use std::path::PathBuf;
use std::process::Command;

/// Read a file directly from the gitolite-admin bare repo at master HEAD.
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

/// Derive workspace from gitolite.conf. Longest pattern match wins.
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

/// List all .yaml/.yml files under .tekton/ in the given commit.
pub fn list_tekton_files(repo_path: &PathBuf, new_rev: &str) -> Result<Vec<String>, String> {
    let output = Command::new("git")
        .args(["ls-tree", "-r", "--name-only", new_rev, "--", ".tekton/"])
        .env("GIT_DIR", repo_path)
        .output()
        .map_err(|e| format!("failed to run git ls-tree: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("Not a valid object name") || stderr.contains("fatal") {
            return Err(format!("git ls-tree failed: {}", stderr.trim()));
        }
        return Ok(vec![]);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter(|l| {
            let lower = l.to_lowercase();
            lower.ends_with(".yaml") || lower.ends_with(".yml")
        })
        .map(|s| s.to_string())
        .collect())
}

/// Read a file's contents from a specific commit in a bare repo.
pub fn read_file_from_commit(
    repo_path: &PathBuf,
    rev: &str,
    path: &str,
) -> Result<String, String> {
    let object = format!("{}:{}", rev, path);
    let output = Command::new("git")
        .args(["show", &object])
        .env("GIT_DIR", repo_path)
        .output()
        .map_err(|e| format!("failed to run git show: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git show {} failed: {}", object, stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Get list of files changed between old_rev and new_rev.
pub fn get_changed_files(
    repo_path: &PathBuf,
    old_rev: &str,
    new_rev: &str,
) -> Result<Vec<String>, String> {
    let is_new_branch = old_rev.chars().all(|c| c == '0');
    let args: Vec<&str> = if is_new_branch {
        vec!["diff-tree", "--no-commit-id", "-r", "--name-only", new_rev]
    } else {
        vec!["diff", "--name-only", old_rev, new_rev]
    };

    let output = Command::new("git")
        .args(&args)
        .env("GIT_DIR", repo_path)
        .output()
        .map_err(|e| format!("failed to run git diff: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git diff failed: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|s| s.to_string())
        .collect())
}