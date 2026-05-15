use std::path::PathBuf;

use anyhow::Result;
use tracing::{info, warn};

use crate::git::{list_gitolite_repos, mirror_repo_to_github};
use crate::state::AppState;

const BACKUP_WORK_DIR: &str = "/tmp/gitolite-backup";

pub async fn run_backup(state: &AppState) -> Result<()> {
    let cfg = &state.0.config;
    let projects_list = PathBuf::from("/data/gitolite-home/projects.list");
    let work_dir = PathBuf::from(BACKUP_WORK_DIR);

    info!("[backup] ══════════════════════════════════════");
    info!("[backup] hourly backup run starting");
    info!("[backup]   work dir      : {BACKUP_WORK_DIR}");
    info!("[backup]   projects.list : {}", projects_list.display());
    info!("[backup]   gh target     : {}", cfg.gh_ssh_prefix);
    info!("[backup] ══════════════════════════════════════");

    tokio::fs::create_dir_all(&work_dir).await?;

    let repos = match list_gitolite_repos(&projects_list).await {
        Ok(r) => r,
        Err(e) => {
            warn!("[backup] ✗ could not read projects.list: {e:#}");
            warn!("[backup] skipping this backup run");
            return Ok(());
        }
    };

    let total = repos.len();
    let mut success = 0usize;
    let mut failure = 0usize;

    for (i, repo) in repos.iter().enumerate() {
        info!("[backup] [{}/{total}] mirroring '{repo}' …", i + 1);
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
            Ok(_) => {
                success += 1;
                info!("[backup] [{}/{total}] ✓ '{repo}' mirrored", i + 1);
            }
            Err(e) => {
                failure += 1;
                warn!("[backup] [{}/{total}] ✗ '{repo}' failed: {e:#}", i + 1);
            }
        }
    }

    info!("[backup] ══════════════════════════════════════");
    info!("[backup] run complete — {success}/{total} succeeded, {failure} failed");
    info!("[backup] ══════════════════════════════════════");
    Ok(())
}