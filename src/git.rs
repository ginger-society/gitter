/// Git operations against the gitolite-admin repo.
///
/// We shell out to `git` rather than use libgit2 because:
///   - SSH agent / key file handling with libgit2 is painful.
///   - `git` is always available in the container.
///   - The operations are simple and infrequent.
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use tokio::process::Command;
use tracing::{debug, info};

use crate::config::Config;

/// A handle to the local checkout of the gitolite-admin repository.
#[derive(Debug)]
pub struct GitoliteAdmin {
    pub repo_path: PathBuf,
    /// Path to the SSH private key used for push/pull.
    ssh_key: String,
    /// SSH URL the repo was cloned from / is pushed to.
    remote_url: String,
    /// gitolite hostname (for StrictHostKeyChecking workaround).
    gitolite_host: String,
    gitolite_port: u16,
}

impl GitoliteAdmin {
    /// Clone on first run; verify the checkout exists on subsequent starts.
    pub async fn init(config: &Config) -> Result<Self> {
        let repo_path = PathBuf::from(&config.admin_repo_path);

        let this = Self {
            repo_path: repo_path.clone(),
            ssh_key: config.admin_ssh_key_path.clone(),
            remote_url: config.admin_repo_ssh_url.clone(),
            gitolite_host: config.gitolite_host.clone(),
            gitolite_port: config.gitolite_port,
        };

        if repo_path.join(".git").exists() {
            info!("gitolite-admin already cloned at {}", repo_path.display());
            this.fetch().await?;
        } else {
            info!("Cloning gitolite-admin from {}", config.admin_repo_ssh_url);
            this.clone_repo().await?;
        }

        Ok(this)
    }

    // ── Write helpers ────────────────────────────────────────────────────────

    /// Overwrite `conf/gitolite.conf` with `content`.
    pub async fn write_gitolite_conf(&self, content: &str) -> Result<()> {
        let path = self.repo_path.join("conf/gitolite.conf");
        tokio::fs::create_dir_all(path.parent().unwrap()).await?;
        tokio::fs::write(&path, content)
            .await
            .context("write gitolite.conf")?;
        debug!("Wrote conf/gitolite.conf ({} bytes)", content.len());
        Ok(())
    }

    /// Write `content` to `kubeconfig/<workspace>.yaml`.
    pub async fn write_kubeconfig(&self, workspace: &str, content: &str) -> Result<()> {
        let dir = self.repo_path.join("kubeconfig");
        tokio::fs::create_dir_all(&dir).await?;
        let filename = sanitise_filename(workspace);
        let path = dir.join(format!("{filename}.yaml"));
        tokio::fs::write(&path, content)
            .await
            .context("write kubeconfig")?;
        debug!("Wrote kubeconfig/{filename}.yaml ({} bytes)", content.len());
        Ok(())
    }

    // ── Git operations ───────────────────────────────────────────────────────

    /// Stage everything, commit (if there are changes), and push to gitolite.
    pub async fn commit_and_push(&self, message: &str) -> Result<()> {
        self.git(&["add", "-A"]).await?;

        // Check if there's anything to commit
        let status = self
            .git_output(&["status", "--porcelain"])
            .await?;
        if status.trim().is_empty() {
            debug!("Nothing to commit, skipping push");
            return Ok(());
        }

        self.git(&["commit", "-m", message, "--author", "gitolite-sidecar <sidecar@local>"]).await?;

        info!("Pushing gitolite-admin…");
        self.git(&["push", "origin", "master"]).await?;
        info!("Push complete");
        Ok(())
    }

    /// Pull latest from remote (fast-forward only).
    pub async fn fetch(&self) -> Result<()> {
        self.git(&["fetch", "--prune", "origin"]).await?;
        self.git(&["reset", "--hard", "origin/master"]).await?;
        Ok(())
    }

    // ── Internal ─────────────────────────────────────────────────────────────

    async fn clone_repo(&self) -> Result<()> {
        let mut cmd = Command::new("git");
        cmd.args(["clone", &self.remote_url, self.repo_path.to_str().unwrap()])
            .env("GIT_SSH_COMMAND", self.ssh_command())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let status = cmd.status().await.context("git clone")?;
        if !status.success() {
            bail!("git clone failed with {status}");
        }

        // Set identity for commits
        self.git(&["config", "user.email", "sidecar@local"]).await?;
        self.git(&["config", "user.name", "gitolite-sidecar"]).await?;
        Ok(())
    }

    async fn git(&self, args: &[&str]) -> Result<()> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.repo_path)
            .env("GIT_SSH_COMMAND", self.ssh_command())
            .output()
            .await
            .with_context(|| format!("git {}", args.join(" ")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git {} failed: {stderr}", args.join(" "));
        }
        Ok(())
    }

    async fn git_output(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.repo_path)
            .env("GIT_SSH_COMMAND", self.ssh_command())
            .output()
            .await
            .with_context(|| format!("git {}", args.join(" ")))?;

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn ssh_command(&self) -> String {
        format!(
            "ssh -i {} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -p {}",
            self.ssh_key, self.gitolite_port
        )
    }
}

// ── Utilities ────────────────────────────────────────────────────────────────

/// Strip anything that isn't alphanumeric, `-`, or `_` to prevent path
/// traversal in workspace names.
fn sanitise_filename(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

// ── Backup-specific helpers ──────────────────────────────────────────────────

/// List all repository names known to gitolite by reading
/// `projects.list` from the gitolite data volume.
pub async fn list_gitolite_repos(projects_list_path: &Path) -> Result<Vec<String>> {
    let content = tokio::fs::read_to_string(projects_list_path)
        .await
        .context("read projects.list")?;
    Ok(content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

/// Clone `repo` from gitolite and push to the GitHub upstream, creating the
/// remote repo via the GitHub API if it does not exist yet.
pub async fn mirror_repo_to_github(
    repo: &str,
    gitolite_host: &str,
    gitolite_port: u16,
    admin_ssh_key: &str,
    gh_ssh_prefix: &str,
    gh_username: &str,
    gh_pat: &str,
    gh_ssh_key: &str,
    work_dir: &Path,
) -> Result<()> {
    let clone_url = format!("ssh://git@{gitolite_host}:{gitolite_port}/{repo}");
    let mirror_dir = work_dir.join(repo.replace('/', "_"));

    let ssh_cmd = format!(
        "ssh -i {admin_ssh_key} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -p {gitolite_port}"
    );

    // --mirror clone (or fetch if already exists)
    if mirror_dir.join("HEAD").exists() {
        run_git(
            &["remote", "update"],
            &mirror_dir,
            &ssh_cmd,
        ).await?;
    } else {
        tokio::fs::create_dir_all(&mirror_dir).await?;
        run_git_bare(
            &["clone", "--mirror", &clone_url, mirror_dir.to_str().unwrap()],
            work_dir,
            &ssh_cmd,
        ).await?;
    }

    // Ensure the GitHub repo exists (create via API if not)
    ensure_github_repo(repo, gh_username, gh_pat).await?;

    let push_url = format!("{gh_ssh_prefix}/{repo}.git");
    let gh_ssh_cmd = format!(
        "ssh -i {gh_ssh_key} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"
    );

    // Push all refs
    run_git(
        &["push", "--mirror", &push_url],
        &mirror_dir,
        &gh_ssh_cmd,
    ).await?;

    info!("Mirrored {repo} → {push_url}");
    Ok(())
}

async fn ensure_github_repo(repo: &str, username: &str, pat: &str) -> Result<()> {
    let (owner, name) = repo.split_once('/').unwrap_or((username, repo));
    let client = reqwest::Client::new();
    let check_url = format!("https://api.github.com/repos/{owner}/{name}");

    let resp = client
        .get(&check_url)
        .header("Authorization", format!("token {pat}"))
        .header("User-Agent", "gitolite-sidecar")
        .send()
        .await?;

    if resp.status() == 200 {
        return Ok(()); // already exists
    }

    // Create it
    let create_url = if owner == username {
        "https://api.github.com/user/repos".to_string()
    } else {
        format!("https://api.github.com/orgs/{owner}/repos")
    };

    let body = serde_json::json!({ "name": name, "private": true });
    client
        .post(&create_url)
        .header("Authorization", format!("token {pat}"))
        .header("User-Agent", "gitolite-sidecar")
        .json(&body)
        .send()
        .await?
        .error_for_status()?;

    info!("Created GitHub repo {owner}/{name}");
    Ok(())
}

async fn run_git(args: &[&str], cwd: &Path, ssh_cmd: &str) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_SSH_COMMAND", ssh_cmd)
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {stderr}", args.join(" "));
    }
    Ok(())
}

async fn run_git_bare(args: &[&str], cwd: &Path, ssh_cmd: &str) -> Result<()> {
    run_git(args, cwd, ssh_cmd).await
}