/// Git operations against the gitolite-admin repo.
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config::Config;

#[derive(Debug)]
pub struct GitoliteAdmin {
    pub repo_path: PathBuf,
    ssh_key: String,
    remote_url: String,
    gitolite_port: u16,
}

impl GitoliteAdmin {
    pub async fn init(config: &Config) -> Result<Self> {
        let repo_path = PathBuf::from(&config.admin_repo_path);

        let this = Self {
            repo_path: repo_path.clone(),
            ssh_key: config.admin_ssh_key_path.clone(),
            remote_url: config.admin_repo_ssh_url.clone(),
            gitolite_port: config.gitolite_port,
        };

        if repo_path.join(".git").exists() {
            info!("[git] repo already exists at {} — fetching latest", repo_path.display());
            this.fetch().await?;
            info!("[git] ✓ fetch + reset to origin/master complete");
        } else {
            info!("[git] no repo found — cloning from {}", config.admin_repo_ssh_url);
            this.clone_repo().await?;
            info!("[git] ✓ clone complete");
        }

        Ok(this)
    }

    // ── Write helpers ────────────────────────────────────────────────────────

    pub async fn write_gitolite_conf(&self, content: &str) -> Result<()> {
        let path = self.repo_path.join("conf/gitolite.conf");
        tokio::fs::create_dir_all(path.parent().unwrap()).await?;
        tokio::fs::write(&path, content)
            .await
            .context("write gitolite.conf")?;
        info!("[git] wrote conf/gitolite.conf ({} bytes)", content.len());
        Ok(())
    }

    pub async fn write_kubeconfig(
        &self,
        workspace: &str,
        environment: &str,
        content: &str,
    ) -> Result<()> {
        let ws  = sanitise_filename(workspace);
        let env = sanitise_filename(environment);
        let dir = self.repo_path.join("kubeconfig").join(&ws);
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{env}.yaml"));
        tokio::fs::write(&path, content)
            .await
            .context("write kubeconfig")?;
        info!("[git] wrote kubeconfig/{ws}/{env}.yaml ({} bytes)", content.len());
        Ok(())
    }

    // ── Git operations ───────────────────────────────────────────────────────

    pub async fn commit_and_push(&self, message: &str) -> Result<()> {
        info!("[git] staging all changes …");
        self.git(&["add", "-A"]).await?;

        let status = self.git_output(&["status", "--porcelain"]).await?;
        if status.trim().is_empty() {
            info!("[git] nothing to commit — working tree clean, skipping push");
            return Ok(());
        }

        // Log what's actually changing
        for line in status.trim().lines() {
            info!("[git] staged: {line}");
        }

        info!("[git] committing: \"{message}\" …");
        self.git(&[
            "commit", "-m", message,
            "--author", "gitolite-sidecar <sidecar@local>",
        ])
        .await?;

        info!("[git] pushing to origin/master …");
        self.git(&["push", "origin", "master"]).await?;
        info!("[git] ✓ push to gitolite complete");
        Ok(())
    }

    pub async fn fetch(&self) -> Result<()> {
        debug!("[git] fetching from origin …");
        self.git(&["fetch", "--prune", "origin"]).await?;
        self.git(&["reset", "--hard", "origin/master"]).await?;
        Ok(())
    }

    // ── Internal ─────────────────────────────────────────────────────────────

    async fn clone_repo(&self) -> Result<()> {
        let status = Command::new("git")
            .args(["clone", &self.remote_url, self.repo_path.to_str().unwrap()])
            .env("GIT_SSH_COMMAND", self.ssh_command())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await
            .context("git clone")?;

        if !status.success() {
            bail!("git clone failed with {status}");
        }

        self.git(&["config", "user.email", "sidecar@local"]).await?;
        self.git(&["config", "user.name", "gitolite-sidecar"]).await?;
        Ok(())
    }

    async fn git(&self, args: &[&str]) -> Result<()> {
        debug!("[git] running: git {}", args.join(" "));
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.repo_path)
            .env("GIT_SSH_COMMAND", self.ssh_command())
            .output()
            .await
            .with_context(|| format!("git {}", args.join(" ")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git {} failed:\n{stderr}", args.join(" "));
        }

        // Surface any git stderr at debug level (progress, remote messages)
        let stderr = String::from_utf8_lossy(&output.stderr);
        for line in stderr.lines() {
            if !line.trim().is_empty() {
                debug!("[git remote] {line}");
            }
        }
        Ok(())
    }

    pub async fn write_pipeline_token(&self, workspace: &str, token: &str) -> Result<()> {
        let ws = sanitise_filename(workspace);
        let dir = self.repo_path.join("pipeline-tokens");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(&ws);
        tokio::fs::write(&path, token)
            .await
            .context("write pipeline token")?;
        info!("[git] wrote pipeline-tokens/{ws} ({} bytes)", token.len());
        Ok(())
    }

    pub async fn write_tekton_kubeconfig(&self, content: &str) -> Result<()> {
        let path = self.repo_path.join("kubeconfig.yaml");
        tokio::fs::write(&path, content)
            .await
            .context("write tekton kubeconfig")?;
        info!("[git] wrote kubeconfig.yaml ({} bytes)", content.len());
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

fn sanitise_filename(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

// ── Backup helpers ───────────────────────────────────────────────────────────

/// Ask gitolite for its authoritative repo list by running:
///   ssh -i <key> -p <port> git@<host> info
///
/// Output looks like:
///   hello admin, this is git@gitolite running gitolite3 ...
///
///    R W    gitolite-admin
///    R W    testing
///    R W    some/nested-repo
///
/// We grab every indented line, strip the permission columns, and return the
/// bare repo names. gitolite-admin is excluded — the sidecar already pushes
/// it continuously.
pub async fn list_gitolite_repos(
    host: &str,
    port: u16,
    ssh_key: &str,
) -> Result<Vec<String>> {
    info!("[backup] querying gitolite for repo list via: ssh git@{host} info");

    let output = Command::new("ssh")
        .args([
            "-i", ssh_key,
            "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null",
            "-o", "BatchMode=yes",
            "-p", &port.to_string(),
            &format!("git@{host}"),
            "info",
        ])
        .output()
        .await
        .context("failed to run ssh git@{host} info")?;

    // gitolite writes to stderr on some versions, stdout on others — merge both.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        for line in stderr.lines() {
            warn!("[backup] gitolite stderr: {line}");
        }
        bail!(
            "ssh git@{host} info exited {}: {}",
            output.status,
            stderr.trim()
        );
    }

    debug!("[backup] gitolite info stdout:\n{stdout}");
    if !stderr.trim().is_empty() {
        debug!("[backup] gitolite info stderr:\n{stderr}");
    }

    // Each repo line is indented and looks like:  " R W    repo-name"
    // The last whitespace-separated token is always the repo name.
    let mut repos: Vec<String> = stdout
        .lines()
        .chain(stderr.lines())
        .filter(|l| l.starts_with(' ') || l.starts_with('\t'))
        .filter_map(|l| l.split_whitespace().last().map(String::from))
        .filter(|name| name != "gitolite-admin")
        .collect::<std::collections::HashSet<_>>() // dedup across stdout+stderr
        .into_iter()
        .collect();

    repos.sort();
    info!("[backup] found {} repos via gitolite info (gitolite-admin excluded)", repos.len());
    for r in &repos {
        debug!("[backup]   {r}");
    }
    Ok(repos)
}


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
    let safe_name  = repo.replace('/', "_");
    let source_dir = work_dir.join(format!("{safe_name}__source"));   // raw gitolite clone
    let rewrite_dir = work_dir.join(format!("{safe_name}__rewrite")); // filter-repo output

    let admin_ssh_cmd = format!(
        "ssh -i {admin_ssh_key} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -p {gitolite_port}"
    );
    let gh_ssh_cmd = format!(
        "ssh -i {gh_ssh_key} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"
    );

    // ── 1. Keep source_dir in sync with gitolite ──────────────────────────────
    if source_dir.join(".git").exists() {
        info!("[backup] [{repo}] fetching latest from gitolite …");
        run_git(&["fetch", "--prune", "origin"], &source_dir, &admin_ssh_cmd).await?;
        info!("[backup] [{repo}] ✓ fetch done");
    } else {
        info!("[backup] [{repo}] cloning from gitolite …");
        tokio::fs::create_dir_all(&source_dir).await?;
        run_git(
            &["clone", "--no-checkout", &clone_url, source_dir.to_str().unwrap()],
            work_dir,
            &admin_ssh_cmd,
        ).await?;
        info!("[backup] [{repo}] ✓ clone done");
    }

    // ── 2. Always blow away rewrite_dir and rebuild from source ──────────────
    // filter-repo rewrites SHAs deterministically (same source + same filter =
    // same SHA) so the push to GitHub will be a no-op when nothing changed.
    if rewrite_dir.exists() {
        tokio::fs::remove_dir_all(&rewrite_dir).await?;
    }
    // Copy source into rewrite_dir — filter-repo operates in-place
    run_cmd("cp", &["-r", source_dir.to_str().unwrap(), rewrite_dir.to_str().unwrap()]).await?;

    info!("[backup] [{repo}] rewriting commit authors via git-filter-repo …");
    let filter_status = tokio::process::Command::new("git")
        .args([
            "filter-repo",
            "--force",
            "--partial",  
            "--email-callback", "return b\"admin@domain.org\"",
            "--name-callback",  "return b\"Admin\"",
        ])
        .current_dir(&rewrite_dir)
        .output()
        .await
        .context("git filter-repo")?;

    if !filter_status.status.success() {
        let stderr = String::from_utf8_lossy(&filter_status.stderr);
        bail!("git filter-repo failed:\n{stderr}");
    }

    let stderr = String::from_utf8_lossy(&filter_status.stderr);
    for line in stderr.lines() {
        info!("[backup] [{repo}] filter-repo: {line}");
    }

    info!("[backup] [{repo}] ✓ author rewrite done");

    // DEBUG: log all refs in rewrite_dir to see what filter-repo left behind
    let refs_output = tokio::process::Command::new("git")
        .args(["show-ref"])
        .current_dir(&rewrite_dir)
        .output()
        .await?;
    let refs_str = String::from_utf8_lossy(&refs_output.stdout);
    for line in refs_str.lines() {
        info!("[backup] [{repo}] ref: {line}");
    }

    // ── 3. Push rewritten branches to GitHub ─────────────────────────────────
    info!("[backup] [{repo}] ensuring GitHub repo exists …");
    ensure_github_repo(repo, gh_username, gh_pat).await?;

    let push_url = format!("{gh_ssh_prefix}/{repo}.git");
    info!("[backup] [{repo}] pushing main + master to {push_url} …");

    let mut pushed_any = false;
    for branch in &["main", "master"] {
        let check = tokio::process::Command::new("git")
            .args(["rev-parse", "--verify", &format!("refs/remotes/origin/{branch}")])
            .current_dir(&rewrite_dir)
            .output()
            .await?;

        if check.status.success() {
            let refspec = format!("refs/remotes/origin/{branch}:refs/heads/{branch}");
            match run_git(&["push", "--force", &push_url, &refspec], &rewrite_dir, &gh_ssh_cmd).await {
                Ok(_) => {
                    info!("[backup] [{repo}] ✓ pushed {branch}");
                    pushed_any = true;
                }
                Err(e) => warn!("[backup] [{repo}] push {branch} failed: {e:#}"),
            }
        } else {
            debug!("[backup] [{repo}] branch {branch} not found — skipping");
        }
    }

    if pushed_any {
        info!("[backup] [{repo}] ✓ backup to GitHub complete");
    } else {
        warn!("[backup] [{repo}] no main or master branch found — nothing pushed");
    }

    Ok(())
}

// helper for non-git shell commands
async fn run_cmd(program: &str, args: &[&str]) -> Result<()> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("{program} {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{program} {} failed:\n{stderr}", args.join(" "));
    }
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

    if resp.status().as_u16() == 200 {
        debug!("[backup] GitHub repo {owner}/{name} already exists");
        return Ok(());
    }

    info!("[backup] GitHub repo {owner}/{name} not found — creating via API …");
    let create_url = if owner == username {
        "https://api.github.com/user/repos".to_string()
    } else {
        format!("https://api.github.com/orgs/{owner}/repos")
    };

    let body = serde_json::json!({ "name": name, "private": true });
    let create_resp = client
        .post(&create_url)
        .header("Authorization", format!("token {pat}"))
        .header("User-Agent", "gitolite-sidecar")
        .json(&body)
        .send()
        .await?;

    let status = create_resp.status();
    if !status.is_success() {
        let body = create_resp.text().await.unwrap_or_default();
        bail!("GitHub API create repo failed ({status}): {body}");
    }

    info!("[backup] ✓ created GitHub repo {owner}/{name}");
    Ok(())
}



async fn run_git(args: &[&str], cwd: &Path, ssh_cmd: &str) -> Result<()> {
    debug!("[git] git {} (cwd={})", args.join(" "), cwd.display());
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_SSH_COMMAND", ssh_cmd)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed:\n{stderr}", args.join(" "));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stderr.lines() {
        if !line.trim().is_empty() {
            debug!("[git remote] {line}");
        }
    }
    Ok(())
}

