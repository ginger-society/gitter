// src/merge_consumer.rs
//
// RabbitMQ consumer for the "gitter.merge.queue" work queue.
//
// Current behaviour (stub)
// ────────────────────────
// For each org-wide merge job:
//   1. Enumerate every repo whose name starts with `{org_id}-` on disk.
//   2. For each repo, check whether the requested branch exists.
//   3. If it does, run an in-memory conflict detection (same logic as
//      repo_handler::detect_conflicts).
//   4. Print the result — no actual merge is performed yet.
//   5. Release the Redis server lock.
//   6. ACK the message.
//
// The real merge (git squash + push) will replace step 4 later.

use std::path::PathBuf;

use futures::StreamExt;
use git2::{MergeOptions, Oid, Repository};
use lapin::options::{BasicAckOptions, BasicConsumeOptions};
use redis::AsyncCommands;

use crate::merge_queue_handler::{CURRENT_MERGE_KEY, SERVER_LOCK_KEY};
use crate::rabbit::{connect_channel, RabbitPoolRef, MERGE_QUEUE};
use crate::state::AppState;

const REPOS_ROOT: &str = "/home/git/repositories";

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn start_merge_consumer(state: AppState, rabbit: RabbitPoolRef) {
    tokio::spawn(async move {
        run_consumer_loop(state, rabbit).await;
    });
}

// ── Consumer loop ─────────────────────────────────────────────────────────────

async fn run_consumer_loop(state: AppState, rabbit: RabbitPoolRef) {
    let addr = rabbit.ampq_uri.clone();

    loop {
        match connect_channel(&addr).await {
            Ok(channel) => {
                tracing::info!("[merge-consumer] connected — consuming from {MERGE_QUEUE}");

                if let Err(e) = process_deliveries(channel, &state).await {
                    tracing::error!("[merge-consumer] channel error: {e:#}");
                }
            }
            Err(e) => {
                tracing::error!("[merge-consumer] connect failed: {e:#}");
            }
        }

        tracing::info!("[merge-consumer] reconnecting in 5 s…");
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    }
}

async fn process_deliveries(
    channel: lapin::Channel,
    state: &AppState,
) -> Result<(), lapin::Error> {
    let mut consumer = channel
        .basic_consume(
            MERGE_QUEUE,
            &format!("gitter_merge_consumer_{}", uuid::Uuid::new_v4()),
            BasicConsumeOptions::default(),
            Default::default(),
        )
        .await?;

    while let Some(delivery) = consumer.next().await {
        match delivery {
            Ok(delivery) => {
                let raw = String::from_utf8_lossy(&delivery.data).to_string();
                tracing::info!("[merge-consumer] received job: {}", raw);

                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) {
                    let merge_request_id = val["merge_request_id"].as_str().unwrap_or("?");
                    let org_id           = val["org_id"].as_str().unwrap_or("");
                    let branch           = val["branch"].as_str().unwrap_or("");

                    tracing::info!(
                        "[merge-consumer] processing merge_request_id={} org_id={} branch={}",
                        merge_request_id, org_id, branch
                    );

                    // Spawn onto a blocking thread — git2 is not async.
                    let org_id = org_id.to_string();
                    let branch = branch.to_string();
                    let merge_request_id = merge_request_id.to_string();

                    tokio::task::spawn_blocking(move || {
                        inspect_org_repos(&merge_request_id, &org_id, &branch);
                    })
                    .await
                    .ok();
                } else {
                    tracing::warn!("[merge-consumer] failed to parse job payload: {}", raw);
                }

                // ── Release the Redis server lock ─────────────────────────────
                release_lock(&state.0.redis).await;

                // ── ACK ───────────────────────────────────────────────────────
                delivery.ack(BasicAckOptions::default()).await?;

                tracing::info!("[merge-consumer] job done — lock released");
            }
            Err(e) => {
                tracing::error!("[merge-consumer] delivery error: {e:#}");
                return Err(e);
            }
        }
    }

    Ok(())
}

// ── Org repo inspection (stub — no actual merge) ──────────────────────────────

fn inspect_org_repos(merge_request_id: &str, org_id: &str, branch: &str) {
    let repo_names = match list_org_repos(org_id) {
        Ok(v)  => v,
        Err(e) => {
            tracing::error!("[merge-consumer] failed to list repos for org '{}': {e:#}", org_id);
            return;
        }
    };

    if repo_names.is_empty() {
        tracing::warn!(
            "[merge-consumer] no repos found for org '{}' (prefix '{}-')",
            org_id, org_id
        );
        return;
    }

    tracing::info!(
        "[merge-consumer] merge_request_id={} — found {} repo(s) for org '{}'",
        merge_request_id,
        repo_names.len(),
        org_id,
    );

    for repo_name in &repo_names {
        match process_repo(repo_name, branch) {
            Ok(outcome) => {
                // ── THE STUB OUTPUT ───────────────────────────────────────────
                println!("{}", outcome);
                tracing::info!("[merge-consumer] {}", outcome);
            }
            Err(e) => {
                tracing::warn!(
                    "[merge-consumer] skipping '{}': {e:#}",
                    repo_name
                );
            }
        }
    }
}

/// Returns a human-readable outcome string for one repo.
/// Returns Err if the repo can't be opened or the branch doesn't exist.
fn process_repo(repo_name: &str, branch: &str) -> anyhow::Result<String> {
    let repo = open_repo(repo_name)?;

    // Does the branch exist in this repo?
    let branch_oid = match resolve_branch(&repo, branch) {
        Some(oid) => oid,
        None => {
            anyhow::bail!("branch '{}' not found", branch);
        }
    };

    // Does main exist?
    let main_oid = repo
        .find_branch("main", git2::BranchType::Local)
        .map_err(|_| anyhow::anyhow!("'main' not found"))?
        .get()
        .target()
        .ok_or_else(|| anyhow::anyhow!("'main' has no tip"))?;

    // Conflict check via in-memory merge (mirrors repo_handler::detect_conflicts)
    let (has_conflicts, conflicting_files) = detect_conflicts(&repo, main_oid, branch_oid);

    if has_conflicts {
        Ok(format!(
            "[merge-consumer] found branch '{}' in repo '{}' — MERGE CONFLICTS detected in: {:?} — manual resolution required",
            branch, repo_name, conflicting_files
        ))
    } else {
        Ok(format!(
            "[merge-consumer] found branch '{}' in repo '{}' — no merge conflicts found — will merge it",
            branch, repo_name
        ))
    }
}

// ── git2 helpers ──────────────────────────────────────────────────────────────

fn open_repo(repo_name: &str) -> anyhow::Result<Repository> {
    let name = repo_name.trim_end_matches(".git");
    if name.is_empty() || name.contains('/') || name.contains("..") {
        anyhow::bail!("invalid repo name: {:?}", repo_name);
    }
    let path = PathBuf::from(REPOS_ROOT).join(format!("{name}.git"));
    if !path.exists() {
        anyhow::bail!("repository '{}' not found at {}", name, path.display());
    }
    Ok(Repository::open_bare(&path)?)
}

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

fn resolve_branch(repo: &Repository, branch: &str) -> Option<Oid> {
    repo.find_branch(branch, git2::BranchType::Local)
        .ok()
        .and_then(|b| b.get().target())
}

/// In-memory conflict detection — identical to repo_handler::detect_conflicts.
fn detect_conflicts(repo: &Repository, main_oid: Oid, branch_oid: Oid) -> (bool, Vec<String>) {
    // Branch already an ancestor of main → no conflict possible
    if repo
        .merge_base(main_oid, branch_oid)
        .map(|base| base == branch_oid)
        .unwrap_or(false)
    {
        return (false, vec![]);
    }

    let main_commit   = match repo.find_commit(main_oid)   { Ok(c) => c, Err(_) => return (false, vec![]) };
    let branch_commit = match repo.find_commit(branch_oid) { Ok(c) => c, Err(_) => return (false, vec![]) };
    let main_tree     = match main_commit.tree()            { Ok(t) => t, Err(_) => return (false, vec![]) };
    let branch_tree   = match branch_commit.tree()          { Ok(t) => t, Err(_) => return (false, vec![]) };

    let base_oid    = match repo.merge_base(main_oid, branch_oid) { Ok(o) => o, Err(_) => return (false, vec![]) };
    let base_commit = match repo.find_commit(base_oid)  { Ok(c) => c, Err(_) => return (false, vec![]) };
    let base_tree   = match base_commit.tree()           { Ok(t) => t, Err(_) => return (false, vec![]) };

    let index = match repo.merge_trees(&base_tree, &main_tree, &branch_tree, Some(&MergeOptions::new())) {
        Ok(idx) => idx,
        Err(e)  => { tracing::warn!("[merge-consumer] merge_trees failed: {e:#}"); return (false, vec![]); }
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
                    let path  = entry.our.or(entry.their).or(entry.ancestor)?;
                    String::from_utf8(path.path).ok()
                })
                .collect()
        })
        .unwrap_or_default();

    (!conflicting.is_empty(), conflicting)
}

// ── Lock helpers ──────────────────────────────────────────────────────────────

async fn release_lock(redis: &redis::aio::ConnectionManager) {
    let mut conn = redis.clone();
    match conn.del::<_, u64>(&[SERVER_LOCK_KEY, CURRENT_MERGE_KEY]).await {
        Ok(n)  => tracing::info!("[merge-consumer] lock released ({n} key(s) deleted)"),
        Err(e) => tracing::error!("[merge-consumer] failed to release lock: {e:#}"),
    }
}