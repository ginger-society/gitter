/// Redis-backed distributed lock and debounce mechanism.
///
/// Two-key design:
///   DIRTY_KEY   — set whenever a write lands; cleared after a successful push
///   PENDING_KEY — set (with TTL = debounce_secs) on every write; acts as the
///                 timer. When it expires naturally, the window has closed.
///
/// Worker loop (1 s poll):
///   dirty? no  → sleep
///   dirty? yes, debounce active? yes → sleep (still in window)
///   dirty? yes, debounce active? no  → try lock → push → sync principals → clear dirty
use std::time::Duration;

use anyhow::Result;
use redis::AsyncCommands;
use tracing::{debug, info, warn};

use crate::state::AppState;

const PENDING_KEY: &str  = "gitolite:pending_push";
const DIRTY_KEY: &str    = "gitolite:dirty";
const LOCK_KEY: &str     = "gitolite:push_lock";
const LOCK_TTL_MS: u64   = 60_000; // 60 s hard cap on a single push

/// Path on the shared PVC where the gitolite SSH server reads principals from.
/// Must match the sshd_config `AuthorizedPrincipalsFile` directive.
const PRINCIPALS_FILE: &str = "/etc/ssh/auth_principals/git";

// ── Write-side helpers (called from handlers) ────────────────────────────────

/// Mark repo as having unpushed changes.
pub async fn mark_dirty(redis: &mut redis::aio::ConnectionManager) -> Result<()> {
    redis.set::<_, _, ()>(DIRTY_KEY, "1").await?;
    debug!("[redis] dirty flag SET");
    Ok(())
}

/// Reset (extend) the debounce countdown.
pub async fn signal_pending(
    redis: &mut redis::aio::ConnectionManager,
    debounce_secs: u64,
) -> Result<()> {
    redis
        .set_ex::<_, _, ()>(PENDING_KEY, "1", debounce_secs)
        .await?;
    debug!("[redis] debounce timer RESET → {}s TTL", debounce_secs);
    Ok(())
}

// ── Worker-side helpers ───────────────────────────────────────────────────────

pub async fn is_dirty(redis: &mut redis::aio::ConnectionManager) -> Result<bool> {
    let v: Option<String> = redis.get(DIRTY_KEY).await?;
    Ok(v.is_some())
}

pub async fn is_debounce_active(redis: &mut redis::aio::ConnectionManager) -> Result<bool> {
    let v: Option<String> = redis.get(PENDING_KEY).await?;
    Ok(v.is_some())
}

/// SET NX PX — returns true only if we won the lock.
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
    let ok = acquired.is_some();
    if ok {
        debug!("[redis] push lock ACQUIRED (id={lock_id}, ttl={LOCK_TTL_MS}ms)");
    } else {
        debug!("[redis] push lock CONTENDED — another instance holds it");
    }
    Ok(ok)
}

pub async fn release_push_lock(redis: &mut redis::aio::ConnectionManager) -> Result<()> {
    redis.del::<_, ()>(LOCK_KEY).await?;
    debug!("[redis] push lock RELEASED");
    Ok(())
}

pub async fn clear_dirty(redis: &mut redis::aio::ConnectionManager) -> Result<()> {
    redis.del::<_, ()>(DIRTY_KEY).await?;
    debug!("[redis] dirty flag CLEARED");
    Ok(())
}

// ── Principals sync ───────────────────────────────────────────────────────────

/// Collect every username from `permissions/*/users` files in the admin repo
/// and write them as a newline-delimited list to `PRINCIPALS_FILE`.
///
/// The gitolite SSH server reads this file to decide which certificate
/// principals are allowed to authenticate. Called after every successful push
/// so the file always reflects the current workspace membership.
pub async fn sync_principals(repo_root: &std::path::Path) -> Result<()> {
    let perms_dir = repo_root.join("permissions");

    let mut users: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    // Walk permissions/<workspace>/users files
    match tokio::fs::read_dir(&perms_dir).await {
        Ok(mut rd) => {
            while let Some(entry) = rd.next_entry().await? {
                if !entry.file_type().await?.is_dir() {
                    continue;
                }
                let users_file = entry.path().join("users");
                match tokio::fs::read_to_string(&users_file).await {
                    Ok(content) => {
                        for line in content.lines() {
                            let name = line.trim();
                            if !name.is_empty() {
                                users.insert(name.to_string());
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        warn!(
                            "[principals] failed to read {}: {e:#}",
                            users_file.display()
                        );
                    }
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // permissions/ dir doesn't exist yet — write an empty file
        }
        Err(e) => return Err(e.into()),
    }

    // Ensure parent directory exists (shared PVC must be mounted)
    if let Some(parent) = std::path::Path::new(PRINCIPALS_FILE).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let content = if users.is_empty() {
        String::new()
    } else {
        let mut s = users.iter().cloned().collect::<Vec<_>>().join("\n");
        s.push('\n');
        s
    };

    tokio::fs::write(PRINCIPALS_FILE, &content).await?;

    info!(
        "[principals] wrote {} principal(s) to {PRINCIPALS_FILE}: {}",
        users.len(),
        users.iter().cloned().collect::<Vec<_>>().join(", ")
    );

    Ok(())
}

// ── Background worker ────────────────────────────────────────────────────────

pub fn spawn_push_worker(state: AppState) {
    tokio::spawn(async move {
        info!("[debounce] worker loop started — polling every 1s");
        let mut last_log_dirty = false; // avoid spamming "waiting" logs

        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;

            let mut redis = state.0.redis.clone();

            // ── 1. Is there anything to push? ────────────────────────────────
            let dirty = match is_dirty(&mut redis).await {
                Ok(d) => d,
                Err(e) => {
                    warn!("[debounce] redis error (dirty check): {e}");
                    continue;
                }
            };

            if !dirty {
                if last_log_dirty {
                    // Transitioned from dirty → clean (after a successful push)
                    info!("[debounce] repo is clean — idle");
                    last_log_dirty = false;
                }
                continue;
            }

            // ── 2. Are we still inside the debounce window? ──────────────────
            let active = match is_debounce_active(&mut redis).await {
                Ok(a) => a,
                Err(e) => {
                    warn!("[debounce] redis error (pending check): {e}");
                    continue;
                }
            };

            if active {
                if !last_log_dirty {
                    info!("[debounce] write received — waiting for debounce window to close ({}s)", state.0.config.debounce_secs);
                    last_log_dirty = true;
                }
                debug!("[debounce] window still open — holding");
                continue;
            }

            // ── 3. Window closed — try to grab the distributed lock ──────────
            info!("[debounce] window closed — attempting push …");
            last_log_dirty = true; // still dirty until push succeeds

            let locked = match try_acquire_push_lock(&mut redis).await {
                Ok(l) => l,
                Err(e) => {
                    warn!("[debounce] redis error (lock acquire): {e}");
                    continue;
                }
            };

            if !locked {
                info!("[debounce] another replica holds the push lock — skipping this tick");
                continue;
            }

            // ── 4. Push ──────────────────────────────────────────────────────
            info!("[git] acquiring repo mutex …");
            let repo_root = {
                let repo = state.0.admin_repo.lock().await;
                let root = repo.repo_path.clone();

                info!("[git] pushing gitolite-admin to remote …");
                match repo.commit_and_push("chore: sidecar auto-push").await {
                    Ok(_) => {
                        info!("[git] ✓ push succeeded");
                    }
                    Err(e) => {
                        warn!("[git] ✗ push failed: {e:#}");
                        // Release lock but keep dirty so we retry next window
                        let _ = release_push_lock(&mut redis).await;
                        continue;
                    }
                }

                root
            }; // repo mutex released here

            // ── 5. Sync principals to shared PVC ─────────────────────────────
            if let Err(e) = sync_principals(&repo_root).await {
                warn!("[principals] sync failed (non-fatal): {e:#}");
            }

            // ── 6. Clean up Redis state ──────────────────────────────────────
            if let Err(e) = clear_dirty(&mut redis).await {
                warn!("[redis] failed to clear dirty flag: {e}");
            }
            if let Err(e) = release_push_lock(&mut redis).await {
                warn!("[redis] failed to release push lock: {e}");
            }
            info!("[debounce] ✓ cycle complete — repo is clean");
            last_log_dirty = false;
        }
    });
}