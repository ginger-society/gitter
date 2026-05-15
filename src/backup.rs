/// Hourly backup: mirror every gitolite repo to the GitHub upstream.
///
/// Reads the list of repos from `projects.list` on the gitolite PVC, then
/// calls `git clone --mirror` (or `remote update` for subsequent runs) and
/// `git push --mirror` to the GitHub remote.
///
/// A separate work directory under /tmp is used so we don't interfere with the
/// gitolite-admin checkout.
use std::path::PathBuf;

use anyhow::Result;
use tracing::{info, warn};

use crate::git::{list_gitolite_repos, mirror_repo_to_github};
use crate::state::AppState;

const BACKUP_WORK_DIR: &str = "/tmp/gitolite-backup";

/// Entry point called by the cron scheduler.
pub async fn run_backup(state: &AppState) -> Result<()> {
    let cfg = &state.0.config;
    info!("Starting hourly backup run…");

    // projects.list lives on the gitolite data PVC at /home/git/projects.list
    // We mount that same PVC (read-only) at /data/gitolite-home in the sidecar.
    let projects_list = PathBuf::from("/data/gitolite-home/projects.list");

    let repos = match list_gitolite_repos(&projects_list).await {
        Ok(r) => r,
        Err(e) => {
            warn!("Could not read projects.list: {e:#}. Skipping backup run.");
            return Ok(());
        }
    };

    info!("Found {} repos to back up", repos.len());
    let work_dir = PathBuf::from(BACKUP_WORK_DIR);
    tokio::fs::create_dir_all(&work_dir).await?;

    let mut success = 0usize;
    let mut failure = 0usize;

    for repo in &repos {
        match mirror_repo_to_github(
            repo,
            &cfg.gitolite_host,
            cfg.gitolite_port,
            &cfg.admin_ssh_key_path,
            &cfg.gh_ssh_prefix,
            &cfg.gh_username,
            &cfg.gh_pat,
            &cfg.gh_ssh_key_path,
            &work_dir,
        )
        .await
        {
            Ok(_) => success += 1,
            Err(e) => {
                warn!("Failed to mirror repo '{repo}': {e:#}");
                failure += 1;
            }
        }
    }

    info!("Backup run complete: {success} succeeded, {failure} failed");
    Ok(())
}