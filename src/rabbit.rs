// src/rabbit.rs
//
// RabbitMQ setup for the merge queue.
//
// Architecture:
//  - One durable *direct* exchange:  "gitter.merge"
//  - One durable *work queue*:       "gitter.merge.queue"
//    bound to the exchange with routing key "merge"
//
// Every broker instance shares the same queue (unlike the notification
// service's fanout design) so only ONE consumer processes each merge request.

use std::sync::Arc;
use lapin::{
    options::{
        BasicPublishOptions, ExchangeDeclareOptions,
        QueueBindOptions, QueueDeclareOptions,
    },
    BasicProperties, Channel, Connection, ConnectionProperties, ExchangeKind,
};
use tokio::sync::Mutex;

// ── Constants ─────────────────────────────────────────────────────────────────

pub const MERGE_EXCHANGE: &str = "gitter.merge";
pub const MERGE_QUEUE:    &str = "gitter.merge.queue";
pub const MERGE_ROUTING:  &str = "merge";

// ── Pool ──────────────────────────────────────────────────────────────────────

pub struct RabbitPool {
    pub channel: Arc<Mutex<Channel>>,
    /// Stored so the consumer can open its own connection with the same URI.
    pub ampq_uri: String,
}

pub type RabbitPoolRef = Arc<RabbitPool>;

impl RabbitPool {
    /// Construct a pool from the already-parsed `Config.ampq_uri`.
    pub async fn new(ampq_uri: String) -> Self {
        loop {
            match connect_channel(&ampq_uri).await {
                Ok(ch) => {
                    tracing::info!("[rabbitmq] merge-queue publisher ready ({})", ampq_uri);
                    return Self {
                        channel: Arc::new(Mutex::new(ch)),
                        ampq_uri,
                    };
                }
                Err(e) => {
                    tracing::error!("[rabbitmq] pool init failed: {e:#} — retrying in 5 s");
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                }
            }
        }
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Open a channel, then declare the exchange + queue so all callers share
/// the same topology regardless of which one connects first.
async fn open_channel(conn: &Connection) -> Result<Channel, lapin::Error> {
    let ch = conn.create_channel().await?;

    // Durable direct exchange — survives RabbitMQ restarts
    ch.exchange_declare(
        MERGE_EXCHANGE,
        ExchangeKind::Direct,
        ExchangeDeclareOptions {
            durable: true,
            ..Default::default()
        },
        Default::default(),
    )
    .await?;

    // Durable work queue — messages survive RabbitMQ restarts
    ch.queue_declare(
        MERGE_QUEUE,
        QueueDeclareOptions {
            durable: true,
            ..Default::default()
        },
        Default::default(),
    )
    .await?;

    ch.queue_bind(
        MERGE_QUEUE,
        MERGE_EXCHANGE,
        MERGE_ROUTING,
        QueueBindOptions::default(),
        Default::default(),
    )
    .await?;

    Ok(ch)
}

/// Connect to the broker at `addr` and return a fully-declared channel.
/// Used by both the publisher pool and the consumer.
pub async fn connect_channel(addr: &str) -> Result<Channel, lapin::Error> {
    let conn = Connection::connect(addr, ConnectionProperties::default()).await?;
    open_channel(&conn).await
}

// ── Publish ───────────────────────────────────────────────────────────────────

/// Serialise `payload` and push it onto the durable merge queue.
pub async fn publish_merge_request(pool: &RabbitPoolRef, payload: &str) {
    let ch = pool.channel.lock().await;
    if let Err(e) = ch
        .basic_publish(
            MERGE_EXCHANGE,
            MERGE_ROUTING,
            BasicPublishOptions::default(),
            payload.as_bytes(),
            // delivery_mode = 2 → message survives RabbitMQ restart
            BasicProperties::default().with_delivery_mode(2),
        )
        .await
    {
        tracing::error!("[rabbitmq] merge publish failed: {e:#}");
    }
}