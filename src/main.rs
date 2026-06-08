mod backup;
mod config;
mod error;
mod git;
mod handlers;
mod redis_lock;
mod requests;
mod routes;
mod state;
mod permissions;
mod auth_helpers;
mod auth_schemas;
mod handler_create_db_taskrun;
mod kubectl_async;
mod repo_handler;
mod rabbit;
mod merge_queue_handler;
mod merge_consumer;
mod pipeline_hook;
mod handler_trigger_pipeline;
mod handle_run_pipeline;

use std::sync::Arc;

use anyhow::Result;
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::git::GitoliteAdmin;
use crate::rabbit::RabbitPool;
use crate::redis_lock::spawn_push_worker;
use crate::state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("gitolite_sidecar=debug".parse()?),
        )
        .with_target(false)
        .with_thread_ids(false)
        .init();

    println!();
    println!("╔══════════════════════════════════════════╗");
    println!("║       gitolite-sidecar  v{}           ║", env!("CARGO_PKG_VERSION"));
    println!("╚══════════════════════════════════════════╝");
    println!();

    let config = Config::from_env()?;

    info!("──────────────────────────────────────────");
    info!("  Config");
    info!("    port          : {}", config.port);
    info!("    debounce_secs : {}", config.debounce_secs);
    info!("    gitolite host : {}:{}", config.gitolite_host, config.gitolite_port);
    info!("    admin repo    : {}", config.admin_repo_path);
    info!("    redis url     : {}", config.redis_url);
    info!("    gh_username   : {}", config.gh_username);
    info!("    gh_ssh_prefix : {}", config.gh_ssh_prefix);
    info!("    ampq_uri      : {}", config.ampq_uri);
    info!("──────────────────────────────────────────");

    // ── Redis ─────────────────────────────────────────────────────────────────
    info!("[redis] connecting to {} …", config.redis_url);
    let redis_client = redis::Client::open(config.redis_url.clone())?;
    let redis_mgr = match redis_client.get_connection_manager().await {
        Ok(mgr) => {
            info!("[redis] ✓ connected");
            mgr
        }
        Err(e) => {
            error!("[redis] ✗ failed to connect: {e:#}");
            return Err(e.into());
        }
    };

    // ── gitolite-admin clone / verify ─────────────────────────────────────────
    info!("[git] initialising gitolite-admin repo …");
    let admin_repo = match GitoliteAdmin::init(&config).await {
        Ok(r) => {
            info!("[git] ✓ repo ready at {}", config.admin_repo_path);
            r
        }
        Err(e) => {
            error!("[git] ✗ failed to init repo: {e:#}");
            return Err(e);
        }
    };

    let state = AppState::new(config.clone(), admin_repo, redis_mgr);

    // ── Debounce push worker ──────────────────────────────────────────────────
    info!("[debounce] starting push worker (window = {}s) …", config.debounce_secs);
    spawn_push_worker(state.clone());
    info!("[debounce] ✓ push worker running");

    // ── Hourly backup cron ────────────────────────────────────────────────────
    info!("[backup] registering hourly cron job …");
    let scheduler = JobScheduler::new().await?;
    let backup_state = state.clone();
    scheduler
        .add(Job::new_async("0 0 * * * *", move |_uuid, _lock| {
            let s = backup_state.clone();
            Box::pin(async move {
                info!("[backup] ── cron tick: starting hourly backup ──");
                match backup::run_backup(&s).await {
                    Ok(_)  => info!("[backup] ✓ hourly backup complete"),
                    Err(e) => error!("[backup] ✗ hourly backup failed: {e:#}"),
                }
            })
        })?)
        .await?;
    scheduler.start().await?;
    info!("[backup] ✓ cron registered (fires at :00 of every hour)");

    // // ── Startup backup ────────────────────────────────────────────────────────
    // info!("[backup] running initial backup on startup …");
    // match backup::run_backup(&state).await {
    //     Ok(_)  => info!("[backup] ✓ startup backup complete"),
    //     Err(e) => error!("[backup] ✗ startup backup failed: {e:#}"),
    // }

    // ── RabbitMQ merge queue ──────────────────────────────────────────────────
    info!("[rabbitmq] connecting to {} …", config.ampq_uri);
    let rabbit_pool = Arc::new(RabbitPool::new(config.ampq_uri.clone()).await);
    info!("[rabbitmq] ✓ merge-queue publisher ready");

    // Start the stub consumer — prints each job and releases the Redis lock.
    merge_consumer::start_merge_consumer(state.clone(), rabbit_pool.clone()).await;
    info!("[rabbitmq] ✓ merge consumer started");

    // ── HTTP server ───────────────────────────────────────────────────────────
    println!();
    info!("──────────────────────────────────────────");
    info!("  Server listening");
    info!("    http://0.0.0.0:{}", config.port);
    info!("  Endpoints");
    info!("    POST http://0.0.0.0:{}/permissions", config.port);
    info!("    POST http://0.0.0.0:{}/kubeconfig", config.port);
    info!("    POST http://0.0.0.0:{}/org/merge-queue", config.port);
    info!("    GET  http://0.0.0.0:{}/healthz", config.port);
    info!("    GET  http://0.0.0.0:{}/api-doc.json", config.port);
    info!("    GET  http://0.0.0.0:{}/swagger-ui/", config.port);
    info!("──────────────────────────────────────────");
    println!();

    let routes = routes::build(state, rabbit_pool);
    warp::serve(routes).run(([0, 0, 0, 0], config.port)).await;

    Ok(())
}