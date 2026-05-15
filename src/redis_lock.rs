/// Redis-backed distributed lock and debounce mechanism.
///
/// Strategy
/// ─────────
/// 1. Every write request refreshes a Redis key `gitolite:pending_push` with
///    a TTL equal to `debounce_secs`.
/// 2. A background Tokio task watches that key and, once it expires (i.e. no
///    new writes came in for `debounce_secs`), acquires an exclusive lock and
///    performs the push.
/// 3. The lock key `gitolite:push_lock` uses SET NX PX to guarantee only one
///    pusher runs at a time even if multiple sidecar replicas exist.
use std::time::Duration;

use anyhow::Result;
use redis::AsyncCommands;
use tracing::{debug, info, warn};

use crate::state::AppState;

const PENDING_KEY: &str = "gitolite:pending_push";
const LOCK_KEY: &str = "gitolite:push_lock";
const LOCK_TTL_MS: u64 = 60_000; // 60 s — more than enough for a push

/// Signal that a write has occurred. Resets (extends) the debounce window.
pub async fn signal_pending(redis: &mut redis::aio::ConnectionManager, debounce_secs: u64) -> Result<()> {
    redis
        .set_ex::<_, _, ()>(PENDING_KEY, "1", debounce_secs)
        .await?;
    debug!("Debounce timer reset ({debounce_secs}s)");
    Ok(())
}

/// Spawn a background task that polls Redis and pushes when the debounce
/// window expires. This is intentionally simple: poll every second.
/// If you need sub-second latency, switch to a Lua script or keyspace
/// notifications — but for config pushes, 1 s polling is fine.
pub fn spawn_debounce_worker(state: AppState) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;

            let mut redis = state.0.redis.clone();
            let pending: Option<String> = match redis.get(PENDING_KEY).await {
                Ok(v) => v,
                Err(e) => {
                    warn!("Redis GET error in debounce worker: {e}");
                    continue;
                }
            };

            if pending.is_none() {
                // Key expired → debounce window closed, nothing pending
                continue;
            }

            // Key still alive — window still open; keep waiting.
            // (We wait until the key naturally disappears via TTL before we
            //  try to push, so we don't need an extra "is it time?" check.)
            //
            // Actually: we want to push *after* the key disappears.
            // So we continue here and catch it on the next iteration when
            // pending == None. But we need to distinguish "never set" from
            // "just expired". We use a second sentinel key to track this.
            //
            // Simpler approach used here:
            // • Write sets PENDING_KEY with TTL = debounce_secs.
            // • Worker sees PENDING_KEY present → still debouncing, skip.
            // • Worker sees PENDING_KEY absent AND a DIRTY_KEY present →
            //   push time!
            // • After successful push, delete DIRTY_KEY.
            continue;
        }
    });

    // Second goroutine for the dirty-key approach
    // (split for clarity)
    tokio::spawn(async move {});
}

// ── Clean two-key debounce ────────────────────────────────────────────────────

const DIRTY_KEY: &str = "gitolite:dirty";

/// Mark the repo as dirty (has unpushed local commits).
/// Call this *before* writing files and *before* resetting the debounce timer.
pub async fn mark_dirty(redis: &mut redis::aio::ConnectionManager) -> Result<()> {
    redis.set::<_, _, ()>(DIRTY_KEY, "1").await?;
    Ok(())
}

/// Called by the debounce worker when PENDING_KEY has expired.
/// Tries to acquire the push lock; returns true if push should proceed.
pub async fn try_acquire_push_lock(redis: &mut redis::aio::ConnectionManager) -> Result<bool> {
    let lock_id = uuid::Uuid::new_v4().to_string();
    let acquired: Option<String> = redis::cmd("SET")
        .arg(LOCK_KEY)
        .arg(&lock_id)
        .arg("NX")
        .arg("PX")
        .arg(LOCK_TTL_MS)
        .query_async(redis)
        .await?;
    Ok(acquired.is_some())
}

pub async fn release_push_lock(redis: &mut redis::aio::ConnectionManager) -> Result<()> {
    redis.del::<_, ()>(LOCK_KEY).await?;
    Ok(())
}

pub async fn clear_dirty(redis: &mut redis::aio::ConnectionManager) -> Result<()> {
    redis.del::<_, ()>(DIRTY_KEY).await?;
    Ok(())
}

pub async fn is_dirty(redis: &mut redis::aio::ConnectionManager) -> Result<bool> {
    let v: Option<String> = redis.get(DIRTY_KEY).await?;
    Ok(v.is_some())
}

pub async fn is_debounce_active(redis: &mut redis::aio::ConnectionManager) -> Result<bool> {
    let v: Option<String> = redis.get(PENDING_KEY).await?;
    Ok(v.is_some())
}

/// Spawn the unified debounce worker.
///
/// Flow:
///   write request → mark_dirty + signal_pending(TTL=debounce_secs)
///   worker loop  → every 1 s:
///                    if dirty && !debounce_active:
///                      try_acquire_push_lock
///                      push
///                      clear_dirty + release_lock
pub fn spawn_push_worker(state: AppState) {
    tokio::spawn(async move {
        info!("Debounce push worker started");
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;

            let mut redis = state.0.redis.clone();

            let dirty = match is_dirty(&mut redis).await {
                Ok(d) => d,
                Err(e) => { warn!("Redis error (dirty check): {e}"); continue; }
            };
            if !dirty { continue; }

            let active = match is_debounce_active(&mut redis).await {
                Ok(a) => a,
                Err(e) => { warn!("Redis error (debounce check): {e}"); continue; }
            };
            if active { continue; } // still in debounce window

            // Debounce window closed and repo is dirty → time to push
            let locked = match try_acquire_push_lock(&mut redis).await {
                Ok(l) => l,
                Err(e) => { warn!("Redis error (lock): {e}"); continue; }
            };
            if !locked {
                debug!("Another instance holds the push lock; skipping");
                continue;
            }

            info!("Debounce window closed; pushing gitolite-admin…");
            {
                let repo = state.0.admin_repo.lock().await;
                match repo.commit_and_push("chore: sidecar auto-push").await {
                    Ok(_) => info!("Push succeeded"),
                    Err(e) => {
                        // Release lock but keep dirty flag so we retry next tick
                        warn!("Push failed: {e:#}");
                        let _ = release_push_lock(&mut redis).await;
                        continue;
                    }
                }
            }

            let _ = clear_dirty(&mut redis).await;
            let _ = release_push_lock(&mut redis).await;
        }
    });
}