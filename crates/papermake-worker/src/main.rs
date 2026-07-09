//! Papermake worker.
//!
//! A single background job (one aggregator regardless of render-instance count)
//! that periodically:
//! 1. aggregates raw analytics NDJSON in S3 into `summary.json`, and
//! 2. prunes expired outputs and old analytics raw.
//!
//! No always-on database — S3 is the shared collation point.

use std::time::Duration;

use papermake_registry::render_storage::{aggregator, retention};
use papermake_registry::S3Storage;
use time::OffsetDateTime;
use tracing::{error, info};

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "papermake_worker=info".to_string()),
        )
        .init();

    let blob = S3Storage::from_env().expect("S3 configuration (S3_* env vars)");
    if let Err(e) = blob.ensure_bucket().await {
        error!("Failed to ensure S3 bucket: {}", e);
    }

    let interval = env_u64("WORKER_INTERVAL_SECONDS", 60);
    let analytics_retention_days = env_u32("ANALYTICS_RETENTION_DAYS", 30);
    info!(
        "papermake-worker started (interval {}s, analytics retention {}d)",
        interval, analytics_retention_days
    );

    loop {
        let now = OffsetDateTime::now_utc();

        match aggregator::run(&blob, now, aggregator::DEFAULT_RECENT_N).await {
            Ok(summary) => info!(
                "Aggregated: {} renders (24h), {} templates",
                summary.totals.renders_24h,
                summary.templates.len()
            ),
            Err(e) => error!("Aggregation failed: {}", e),
        }

        match retention::prune(&blob, now.date(), analytics_retention_days).await {
            Ok(stats) => info!(
                "Pruned: {} renders, {} expiry files, {} raw files",
                stats.renders_pruned, stats.expiry_files_consumed, stats.raw_files_deleted
            ),
            Err(e) => error!("Prune failed: {}", e),
        }

        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
}
