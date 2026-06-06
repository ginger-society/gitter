// src/merge_consumer.rs
//
// RabbitMQ consumer for the "gitter.merge.queue" work queue.
//
// Current behaviour (stub)
// ────────────────────────
// 1. Connect to RabbitMQ using the URI stored in the RabbitPool (sourced from
//    Config.ampq_uri — no env look-up at runtime).
// 2. For each delivery, print the raw message payload.
// 3. Release the Redis server lock (delete `git:server:lock` and
//    `git:merge:current`).
// 4. ACK the message so RabbitMQ removes it from the queue.
// 5. Reconnect with a 5-second back-off on any channel error.
//
// The actual merge logic (git squash + push) will replace step 3 later.

use futures::StreamExt;
use lapin::options::{BasicAckOptions, BasicConsumeOptions};
use redis::AsyncCommands;

use crate::merge_queue_handler::{CURRENT_MERGE_KEY, SERVER_LOCK_KEY};
use crate::rabbit::{connect_channel, RabbitPoolRef, MERGE_QUEUE};
use crate::state::AppState;

// ── Entry point ───────────────────────────────────────────────────────────────

/// Spawn the consumer loop. Call once from `main` after AppState is ready.
pub async fn start_merge_consumer(state: AppState, rabbit: RabbitPoolRef) {
    tokio::spawn(async move {
        run_consumer_loop(state, rabbit).await;
    });
}

// ── Consumer loop ─────────────────────────────────────────────────────────────

async fn run_consumer_loop(state: AppState, rabbit: RabbitPoolRef) {
    // The consumer opens its own AMQP connection so it doesn't share the
    // publish channel's Mutex, but uses the same URI from the pool.
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
            // manual ACK — we confirm only after releasing the lock
            BasicConsumeOptions::default(),
            Default::default(),
        )
        .await?;

    while let Some(delivery) = consumer.next().await {
        match delivery {
            Ok(delivery) => {
                let raw = String::from_utf8_lossy(&delivery.data).to_string();

                // ── Print the payload (stub — real merge logic goes here) ──────
                tracing::info!("[merge-consumer] ── received merge job ──");
                tracing::info!("[merge-consumer] payload: {}", raw);
                println!("[merge-consumer] RAW payload:\n{raw}\n");

                // ── Structured logging ────────────────────────────────────────
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) {
                    tracing::info!(
                        "[merge-consumer] merge_request_id={} repo={} branch={}",
                        val["merge_request_id"].as_str().unwrap_or("?"),
                        val["repo"].as_str().unwrap_or("?"),
                        val["branch"].as_str().unwrap_or("?"),
                    );
                }

                // ── Release the Redis server lock ─────────────────────────────
                release_lock(&state.0.redis).await;

                // ── ACK the message ───────────────────────────────────────────
                delivery.ack(BasicAckOptions::default()).await?;

                tracing::info!("[merge-consumer] job processed and lock released");
            }
            Err(e) => {
                tracing::error!("[merge-consumer] delivery error: {e:#}");
                return Err(e);
            }
        }
    }

    Ok(())
}

// ── Lock helpers ──────────────────────────────────────────────────────────────

/// Delete both Redis keys that constitute the server lock.
async fn release_lock(redis: &redis::aio::ConnectionManager) {
    let mut conn = redis.clone();

    match conn.del::<_, u64>(&[SERVER_LOCK_KEY, CURRENT_MERGE_KEY]).await {
        Ok(n)  => tracing::info!("[merge-consumer] lock released ({n} key(s) deleted)"),
        Err(e) => tracing::error!("[merge-consumer] failed to release lock: {e:#}"),
    }
}