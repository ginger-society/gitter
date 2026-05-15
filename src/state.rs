use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::git::GitoliteAdmin;

/// Shared across all Warp filters via Arc<AppStateInner>.
#[derive(Clone)]
pub struct AppState(pub Arc<AppStateInner>);

pub struct AppStateInner {
    pub config: Config,
    /// The gitolite-admin repo handle — serialised behind a Mutex so only one
    /// coroutine writes/pushes at a time. Redis is used for cross-process
    /// locking (in case we ever scale the sidecar), but the Mutex gives us a
    /// fast in-process guard.
    pub admin_repo: Mutex<GitoliteAdmin>,
    pub redis: redis::aio::ConnectionManager,
}

impl AppState {
    pub fn new(
        config: Config,
        admin_repo: GitoliteAdmin,
        redis: redis::aio::ConnectionManager,
    ) -> Self {
        Self(Arc::new(AppStateInner {
            config,
            admin_repo: Mutex::new(admin_repo),
            redis,
        }))
    }
}