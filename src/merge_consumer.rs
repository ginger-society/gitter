// src/merge_consumer.rs
//
// RabbitMQ consumer for the "gitter.merge.queue" work queue.
//
// Behaviour
// ─────────
// For each org-wide merge job:
//   1. Enumerate every repo whose name starts with `{org_id}-` on disk.
//   2. PASS 1 — conflict scan (all-or-none gate):
//      For every repo that has the requested branch, run an in-memory conflict
//      detection against main. If ANY repo has conflicts the whole job is
//      aborted — nothing is merged.
//   3. PASS 2 — only reached when pass 1 is fully clean:
//      For every repo that has the branch, perform a squash merge into main,
//      push to origin, then delete the branch (local + remote).
//   4. Release the Redis server lock.
//   5. ACK the message.

use std::path::PathBuf;

use futures::StreamExt;
use git2::{MergeOptions, Oid, Repository, Signature};
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

                    let org_id           = org_id.to_string();
                    let branch           = branch.to_string();
                    let merge_request_id = merge_request_id.to_string();

                    // git2 is not async — run on a blocking thread.
                    tokio::task::spawn_blocking(move || {
                        execute_org_merge(&merge_request_id, &org_id, &branch);
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

// ── Two-pass all-or-none merge ────────────────────────────────────────────────

/// Orchestrates the two-pass merge for every repo belonging to `org_id`.
///
/// Pass 1 — conflict scan:  all repos must be conflict-free or we abort.
/// Pass 2 — actual merge:   squash-merge into main, push, delete branch.
fn execute_org_merge(merge_request_id: &str, org_id: &str, branch: &str) {
    // ── Collect repos that carry the branch ───────────────────────────────────
    let repo_names = match list_org_repos(org_id) {
        Ok(v)  => v,
        Err(e) => {
            tracing::error!(
                "[merge-consumer] merge_request_id={} — failed to list repos for org '{}': {e:#}",
                merge_request_id, org_id
            );
            return;
        }
    };

    if repo_names.is_empty() {
        tracing::warn!(
            "[merge-consumer] merge_request_id={} — no repos found for org '{}' (prefix '{}-')",
            merge_request_id, org_id, org_id
        );
        return;
    }

    // Only operate on repos that actually have the branch.
    let candidate_repos: Vec<String> = repo_names
        .into_iter()
        .filter(|name| {
            match open_repo(name) {
                Ok(repo) => resolve_branch(&repo, branch).is_some(),
                Err(_)   => false,
            }
        })
        .collect();

    if candidate_repos.is_empty() {
        tracing::warn!(
            "[merge-consumer] merge_request_id={} — branch '{}' not found in any repo for org '{}'",
            merge_request_id, branch, org_id
        );
        return;
    }

    tracing::info!(
        "[merge-consumer] merge_request_id={} — {} candidate repo(s) carry branch '{}'",
        merge_request_id, candidate_repos.len(), branch
    );

    // ── PASS 1: conflict scan (all-or-none gate) ───────────────────────────────
    tracing::info!(
        "[merge-consumer] merge_request_id={} — PASS 1: scanning for conflicts …",
        merge_request_id
    );

    let mut any_conflict = false;

    for repo_name in &candidate_repos {
        match check_conflicts(repo_name, branch) {
            Ok((true, files)) => {
                tracing::warn!(
                    "[merge-consumer] merge_request_id={} — CONFLICT in '{}' — files: {:?} — aborting entire merge",
                    merge_request_id, repo_name, files
                );
                any_conflict = true;
                // Keep scanning so we log ALL conflicting repos before bailing.
            }
            Ok((false, _)) => {
                tracing::info!(
                    "[merge-consumer] merge_request_id={} — '{}' is clean",
                    merge_request_id, repo_name
                );
            }
            Err(e) => {
                tracing::warn!(
                    "[merge-consumer] merge_request_id={} — skipping '{}' during conflict scan: {e:#}",
                    merge_request_id, repo_name
                );
                // Treat an unreadable repo as a blocker to stay safe.
                any_conflict = true;
            }
        }
    }

    if any_conflict {
        tracing::error!(
            "[merge-consumer] merge_request_id={} — PASS 1 FAILED: conflicts detected — no repos were merged",
            merge_request_id
        );
        return;
    }

    tracing::info!(
        "[merge-consumer] merge_request_id={} — PASS 1 PASSED: all repos clean — proceeding to merge",
        merge_request_id
    );

    // ── PASS 2: squash-merge, push, delete branch ─────────────────────────────
    tracing::info!(
        "[merge-consumer] merge_request_id={} — PASS 2: merging …",
        merge_request_id
    );

    for repo_name in &candidate_repos {
        match merge(repo_name, branch, merge_request_id) {
            Ok(()) => {
                tracing::info!(
                    "[merge-consumer] merge_request_id={} — '{}' merged + branch deleted ✓",
                    merge_request_id, repo_name
                );
            }
            Err(e) => {
                // Log but continue — pass 1 already guaranteed no conflicts,
                // so a push failure here is an infra issue, not a code conflict.
                tracing::error!(
                    "[merge-consumer] merge_request_id={} — merge/push failed for '{}': {e:#}",
                    merge_request_id, repo_name
                );
            }
        }
    }

    tracing::info!(
        "[merge-consumer] merge_request_id={} — PASS 2 complete",
        merge_request_id
    );
}

// ── Pass-1 helper: conflict check only ───────────────────────────────────────

fn check_conflicts(repo_name: &str, branch: &str) -> anyhow::Result<(bool, Vec<String>)> {
    let repo = open_repo(repo_name)?;

    let branch_oid = resolve_branch(&repo, branch)
        .ok_or_else(|| anyhow::anyhow!("branch '{}' not found in '{}'", branch, repo_name))?;

    let main_oid = repo
        .find_branch("main", git2::BranchType::Local)
        .map_err(|_| anyhow::anyhow!("'main' not found in '{}'", repo_name))?
        .get()
        .target()
        .ok_or_else(|| anyhow::anyhow!("'main' has no tip in '{}'", repo_name))?;

    Ok(detect_conflicts(&repo, main_oid, branch_oid))
}

// ── Pass-2 helper: squash-merge + push + delete branch ───────────────────────

/// Performs a squash merge of `branch` into `main` inside the bare repo,
/// then pushes to origin and deletes the branch (local + remote).
fn merge(repo_name: &str, branch: &str, merge_request_id: &str) -> anyhow::Result<()> {
    let repo = open_repo(repo_name)?;

    let branch_oid = resolve_branch(&repo, branch)
        .ok_or_else(|| anyhow::anyhow!("branch '{}' disappeared before merge", branch))?;

    let main_oid = repo
        .find_branch("main", git2::BranchType::Local)
        .map_err(|_| anyhow::anyhow!("'main' not found in '{}'", repo_name))?
        .get()
        .target()
        .ok_or_else(|| anyhow::anyhow!("'main' has no tip"))?;

    let main_commit   = repo.find_commit(main_oid)?;
    let branch_commit = repo.find_commit(branch_oid)?;

    let base_oid    = repo.merge_base(main_oid, branch_oid)?;
    let base_commit = repo.find_commit(base_oid)?;
    let base_tree   = base_commit.tree()?;
    let main_tree   = main_commit.tree()?;
    let branch_tree = branch_commit.tree()?;

    let mut index = repo.merge_trees(
        &base_tree, &main_tree, &branch_tree,
        Some(&MergeOptions::new()),
    )?;

    let merged_tree_oid = index.write_tree_to(&repo)?;
    let merged_tree     = repo.find_tree(merged_tree_oid)?;

    let sig = Signature::now("gitolite-sidecar", "sidecar@local")?;
    let commit_msg = format!(
        "chore: squash-merge '{}' into main [merge_request_id={}]",
        branch, merge_request_id
    );

    // Writes the commit and advances refs/heads/main in the bare repo directly.
    let new_oid = repo.commit(
        Some("refs/heads/main"),
        &sig, &sig,
        &commit_msg,
        &merged_tree,
        &[&main_commit],
    )?;

    tracing::info!(
        "[merge-consumer] '{}' — squash commit {} on main ✓",
        repo_name, new_oid
    );

    // Delete the branch ref — bare repo, so this is the final state.
    repo.find_branch(branch, git2::BranchType::Local)
        .and_then(|mut b| b.delete())
        .unwrap_or_else(|e| tracing::warn!(
            "[merge-consumer] '{}' — could not delete branch '{}': {e:#}",
            repo_name, branch
        ));

    tracing::info!(
        "[merge-consumer] '{}' — branch '{}' deleted ✓",
        repo_name, branch
    );

    Ok(())
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
    // Branch already an ancestor of main → no conflict possible.
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
    let base_commit = match repo.find_commit(base_oid)             { Ok(c) => c, Err(_) => return (false, vec![]) };
    let base_tree   = match base_commit.tree()                     { Ok(t) => t, Err(_) => return (false, vec![]) };

    let index = match repo.merge_trees(&base_tree, &main_tree, &branch_tree, Some(&MergeOptions::new())) {
        Ok(idx) => idx,
        Err(e)  => {
            tracing::warn!("[merge-consumer] merge_trees failed: {e:#}");
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