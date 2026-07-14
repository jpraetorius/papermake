//! Papermake worker.
//!
//! A background process — **run one or more** — that each cycle:
//! 1. claims and renders queued/orphaned **batch shards** (reusing a warm world),
//! 2. aggregates raw analytics NDJSON in S3 into `summary.json`,
//! 3. prunes expired outputs, old analytics raw, and stale jobs.
//!
//! Shards are claimed with an owner + lease, so many workers split one big batch
//! and a dead worker's shard is reclaimed (resuming only items whose output is
//! missing) once its lease expires. No compare-and-set is needed: render output
//! is content-addressed and thus idempotent, so a rare double-claim only wastes
//! CPU. No always-on database — S3 is the shared collation point.
//!
//! Note: aggregation/pruning are safe to run from multiple workers (idempotent),
//! but redundant; that's fine at small worker counts.

use std::sync::Arc;
use std::time::Duration;

use papermake_registry::render_storage::{aggregator, retention};
use papermake_registry::{Registry, S3BufferedRenderStorage, S3Storage};
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

    // Fonts loaded once at startup so batch renders are fast from the first item.
    papermake::preload_fonts();

    let blob = S3Storage::from_env().expect("S3 configuration (S3_* env vars)");
    if let Err(e) = blob.ensure_bucket().await {
        error!("Failed to ensure S3 bucket: {}", e);
    }

    let worker_id = std::env::var("PAPERMAKE_WORKER_ID")
        .or_else(|_| std::env::var("PAPERMAKE_INSTANCE_ID"))
        .unwrap_or_else(|_| "worker".to_string());
    let render_retention_days = env_u32("RENDER_RETENTION_DAYS", 30);

    // Registry with a buffered render store so batch renders emit analytics
    // records (flushed after each job). Shares the same S3 backend.
    let render_storage =
        S3BufferedRenderStorage::new(Arc::new(blob.clone()), Some(worker_id.clone()), 1000);
    let registry =
        Registry::new(blob.clone(), render_storage).with_retention_days(render_retention_days);

    let interval = env_u64("WORKER_INTERVAL_SECONDS", 60);
    let analytics_retention_days = env_u32("ANALYTICS_RETENTION_DAYS", 30);
    let job_retention_days = env_u32("JOB_RETENTION_DAYS", 7);
    let lease_secs = env_u64("WORKER_LEASE_SECONDS", 120);
    let max_attempts = env_u32("WORKER_MAX_ATTEMPTS", 3);
    info!(
        "papermake-worker started (id={}, interval {}s, lease {}s, max attempts {})",
        worker_id, interval, lease_secs, max_attempts
    );

    loop {
        // 1. Drain claimable batch *shards* across all jobs (pending, plus
        //    orphaned ones whose lease expired). Many workers run this loop
        //    concurrently, so one big batch is split across them. One warm world
        //    per shard; resumes items whose output already exists.
        loop {
            match registry
                .claim_next_shard(
                    &worker_id,
                    lease_secs,
                    max_attempts,
                    OffsetDateTime::now_utc(),
                )
                .await
            {
                Ok(Some((meta, shard, inputs))) => {
                    let job_id = meta.job_id.clone();
                    let (shard_index, shard_len) = (shard.index, shard.len);
                    info!(
                        "Rendering shard {} of job {} ({} items)",
                        shard_index, job_id, shard_len
                    );
                    if let Err(e) = registry
                        .run_claimed_shard(meta, shard, inputs, lease_secs)
                        .await
                    {
                        error!("Job {} shard {} failed: {}", job_id, shard_index, e);
                    }
                    // Persist analytics records staged during the shard.
                    if let Some(rs) = registry.render_storage()
                        && let Err(e) = rs.flush().await
                    {
                        error!(
                            "Analytics flush after job {} shard {} failed: {}",
                            job_id, shard_index, e
                        );
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    error!("Claiming batch shard failed: {}", e);
                    break;
                }
            }
        }

        // 2. Aggregate analytics.
        let now = OffsetDateTime::now_utc();
        match aggregator::run(&blob, now, aggregator::DEFAULT_RECENT_N).await {
            Ok(summary) => info!(
                "Aggregated: {} renders (24h), {} templates",
                summary.totals.renders_24h,
                summary.templates.len()
            ),
            Err(e) => error!("Aggregation failed: {}", e),
        }

        // 3. Prune expired outputs, old analytics raw, and stale batch jobs.
        match retention::prune(
            &blob,
            now.date(),
            analytics_retention_days,
            job_retention_days,
        )
        .await
        {
            Ok(stats) => info!(
                "Pruned: {} renders, {} expiry files, {} raw files, {} jobs",
                stats.renders_pruned,
                stats.expiry_files_consumed,
                stats.raw_files_deleted,
                stats.jobs_pruned
            ),
            Err(e) => error!("Prune failed: {}", e),
        }

        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
}
