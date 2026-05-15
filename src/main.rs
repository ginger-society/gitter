mod backup;
mod config;
mod error;
mod git;
mod handlers;
mod redis_lock;
mod requests;
mod routes;
mod state;

use anyhow::Result;
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::git::GitoliteAdmin;
use crate::redis_lock::spawn_push_worker;
use crate::state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("gitolite_sidecar=info".parse()?),
        )
        .init();

    let config = Config::from_env()?;
    info!("Starting gitolite-sidecar on :{}", config.port);

    // ── Redis ─────────────────────────────────────────────────────────────────
    let redis_client = redis::Client::open(config.redis_url.clone())?;
    let redis_mgr = redis_client.get_connection_manager().await?;
    info!("Connected to Redis at {}", config.redis_url);

    // ── gitolite-admin clone / verify ─────────────────────────────────────────
    let admin_repo = GitoliteAdmin::init(&config).await?;
    info!("gitolite-admin repo ready at {}", config.admin_repo_path);

    let state = AppState::new(config.clone(), admin_repo, redis_mgr);

    // ── Debounce push worker ──────────────────────────────────────────────────
    spawn_push_worker(state.clone());

    // ── Hourly backup cron ────────────────────────────────────────────────────
    let scheduler = JobScheduler::new().await?;
    let backup_state = state.clone();
    scheduler
        .add(Job::new_async("0 0 * * * *", move |_uuid, _lock| {
            let s = backup_state.clone();
            Box::pin(async move {
                if let Err(e) = backup::run_backup(&s).await {
                    tracing::error!("Backup job failed: {e:#}");
                }
            })
        })?)
        .await?;
    scheduler.start().await?;
    info!("Hourly backup scheduler started (cron: 0 0 * * * *)");

    // ── HTTP server ───────────────────────────────────────────────────────────
    // Swagger UI: http://<host>:<port>/swagger-ui/
    // OpenAPI JSON: http://<host>:<port>/api-doc.json
    let routes = routes::build(state);
    let port = config.port;
    info!("Swagger UI available at http://0.0.0.0:{port}/swagger-ui/");
    warp::serve(routes).run(([0, 0, 0, 0], port)).await;

    Ok(())
}