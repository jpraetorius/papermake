//! Papermake worker.
//!
//! One binary, two roles (set `WORKER_ROLE`):
//! - **`render`** — poll for claimable batch **shards** and render them (reusing
//!   a warm world). Stateless and horizontally scalable: run as many as you want
//!   to split a big batch across them. Does *not* aggregate or prune.
//! - **`maintenance`** — each cycle, aggregate raw analytics NDJSON into
//!   `summary.json` and prune expired outputs / old raw / stale jobs. Run **one**
//!   (idempotent, but redundant if duplicated). Does no rendering, so stats stay
//!   fresh regardless of render load.
//! - **`all`** (default) — do both in one process (simple single-node / dev).
//!
//! Shards are claimed with an owner + lease, so a dead worker's shard is
//! reclaimed (resuming only items whose output is missing) once its lease
//! expires. No compare-and-set is needed: render output is content-addressed and
//! thus idempotent, so a rare double-claim only wastes CPU. No always-on
//! database — S3 is the shared collation point.

use std::sync::Arc;
use std::time::Duration;

use papermake_registry::render_storage::{aggregator, retention};
use papermake_registry::{Registry, S3BufferedRenderStorage, S3Storage};
use time::OffsetDateTime;
use tracing::{error, info, warn};

/// Which responsibilities this worker process takes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Render shards only (scalable).
    Render,
    /// Aggregate analytics + prune only (run one).
    Maintenance,
    /// Both, in one process (default).
    All,
}

impl Role {
    fn from_env() -> Self {
        match std::env::var("WORKER_ROLE")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "render" | "renderer" => Role::Render,
            "maintenance" | "maint" => Role::Maintenance,
            "" | "all" | "both" => Role::All,
            other => {
                warn!("Unknown WORKER_ROLE '{other}', defaulting to 'all'");
                Role::All
            }
        }
    }

    fn renders(self) -> bool {
        matches!(self, Role::Render | Role::All)
    }

    fn maintains(self) -> bool {
        matches!(self, Role::Maintenance | Role::All)
    }
}

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

    let role = Role::from_env();

    let blob = S3Storage::from_env().expect("S3 configuration (S3_* env vars)");
    if let Err(e) = blob.ensure_bucket().await {
        error!("Failed to ensure S3 bucket: {}", e);
    }

    // Worker id must be UNIQUE per process: it's the shard-claim owner and the
    // analytics instance key. Explicit env wins; otherwise fall back to the
    // container hostname (distinct per scaled replica), then the PID — so
    // `--scale papermake-worker=N` gives each replica a distinct id automatically.
    let worker_id = std::env::var("PAPERMAKE_WORKER_ID")
        .or_else(|_| std::env::var("PAPERMAKE_INSTANCE_ID"))
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty()))
        .unwrap_or_else(|| format!("worker-{}", std::process::id()));

    let interval = env_u64("WORKER_INTERVAL_SECONDS", 60);
    let analytics_retention_days = env_u32("ANALYTICS_RETENTION_DAYS", 30);
    let job_retention_days = env_u32("JOB_RETENTION_DAYS", 7);
    let lease_secs = env_u64("WORKER_LEASE_SECONDS", 120);
    let max_attempts = env_u32("WORKER_MAX_ATTEMPTS", 3);

    // Only render workers need fonts + a registry with a render store; a
    // maintenance-only worker stays lean (no font preload, no render pipeline).
    let registry = if role.renders() {
        papermake::preload_fonts();
        let render_retention_days = env_u32("RENDER_RETENTION_DAYS", 30);
        let render_storage =
            S3BufferedRenderStorage::new(Arc::new(blob.clone()), Some(worker_id.clone()), 1000);
        Some(Registry::new(blob.clone(), render_storage).with_retention_days(render_retention_days))
    } else {
        None
    };

    info!(
        "papermake-worker started (id={}, role={:?}, interval {}s, lease {}s, max attempts {})",
        worker_id, role, interval, lease_secs, max_attempts
    );

    loop {
        // Render role: drain every claimable shard across all jobs (pending, plus
        // orphaned ones whose lease expired). Scale these workers to split a big
        // batch; each renders items to content-addressed keys (idempotent).
        if let Some(registry) = &registry {
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
        }

        // Maintenance role: aggregate analytics + prune. Independent of render
        // load, so stats refresh on schedule even while renderers are busy.
        if role.maintains() {
            let now = OffsetDateTime::now_utc();
            match aggregator::run(&blob, now, aggregator::DEFAULT_RECENT_N).await {
                Ok(summary) => info!(
                    "Aggregated: {} renders (24h), {} templates",
                    summary.totals.renders_24h,
                    summary.templates.len()
                ),
                Err(e) => error!("Aggregation failed: {}", e),
            }
            match retention::prune(&blob, now.date(), analytics_retention_days, job_retention_days)
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
        }

        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
}
