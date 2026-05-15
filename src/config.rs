use anyhow::{Context, Result};

#[derive(Clone, Debug)]
pub struct Config {
    // ── Network ──────────────────────────────────────────────────────────────
    pub port: u16,
    pub redis_url: String,

    // ── Gitolite admin repo ──────────────────────────────────────────────────
    /// SSH URL of the gitolite-admin repo, e.g. git@gitolite:gitolite-admin
    pub admin_repo_ssh_url: String,
    /// Local filesystem path where the repo is checked out inside the sidecar
    pub admin_repo_path: String,
    /// Path to the SSH private key that has admin access to gitolite
    pub admin_ssh_key_path: String,

    // ── GitHub / upstream backup ─────────────────────────────────────────────
    /// SSH URL prefix for backup remotes, e.g. git@github.com:my-org
    pub gh_ssh_prefix: String,
    /// GitHub username (used for PAT API calls to create repos)
    pub gh_username: String,
    /// GitHub Personal Access Token (for creating repos via API)
    pub gh_pat: String,
    /// Path to the SSH private key for GitHub pushes
    pub gh_ssh_key_path: String,

    // ── Gitolite SSH host ────────────────────────────────────────────────────
    /// Hostname / service name of the gitolite pod (for `ssh` calls)
    pub gitolite_host: String,
    /// SSH port of the gitolite service (default 22)
    pub gitolite_port: u16,

    // ── Debounce ─────────────────────────────────────────────────────────────
    /// Seconds to wait after the last write before pushing
    pub debounce_secs: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            port: env_var("PORT")
                .unwrap_or_else(|_| "8080".into())
                .parse()
                .context("PORT must be a number")?,
            redis_url: env_req("REDIS_URL")?,
            admin_repo_ssh_url: env_req("ADMIN_REPO_SSH_URL")?,
            admin_repo_path: env_var("ADMIN_REPO_PATH")
                .unwrap_or_else(|_| "/data/gitolite-admin".into()),
            admin_ssh_key_path: env_req("ADMIN_SSH_KEY_PATH")?,
            gh_ssh_prefix: env_req("GH_SSH_PREFIX")?,
            gh_username: env_req("GH_USERNAME")?,
            gh_pat: env_req("GH_PAT")?,
            gh_ssh_key_path: env_req("GH_SSH_KEY_PATH")?,
            gitolite_host: env_var("GITOLITE_HOST")
                .unwrap_or_else(|_| "gitolite".into()),
            gitolite_port: env_var("GITOLITE_PORT")
                .unwrap_or_else(|_| "22".into())
                .parse()
                .context("GITOLITE_PORT must be a number")?,
            debounce_secs: env_var("DEBOUNCE_SECS")
                .unwrap_or_else(|_| "10".into())
                .parse()
                .context("DEBOUNCE_SECS must be a number")?,
        })
    }
}

fn env_req(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("Missing required env var: {key}"))
}

fn env_var(key: &str) -> Result<String, std::env::VarError> {
    std::env::var(key)
}