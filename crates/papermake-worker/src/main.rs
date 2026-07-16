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
        let value = std::env::var("WORKER_ROLE").ok();
        Self::from_value(value.as_deref())
    }

    fn from_value(value: Option<&str>) -> Self {
        match value
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

fn default_interval_seconds(role: Role) -> u64 {
    match role {
        Role::Render => 5,
        Role::Maintenance => 30,
        Role::All => 10,
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    let value = std::env::var(key).ok();
    env_u64_value(key, value.as_deref(), default)
}

fn env_u64_value(key: &str, value: Option<&str>, default: u64) -> u64 {
    match value {
        Some(v) => v.parse().unwrap_or_else(|_| {
            warn!("Invalid {key}={v:?}; using default {default}");
            default
        }),
        None => default,
    }
}

fn env_u32(key: &str, default: u32) -> u32 {
    let value = std::env::var(key).ok();
    env_u32_value(key, value.as_deref(), default)
}

fn env_u32_value(key: &str, value: Option<&str>, default: u32) -> u32 {
    match value {
        Some(v) => v.parse().unwrap_or_else(|_| {
            warn!("Invalid {key}={v:?}; using default {default}");
            default
        }),
        None => default,
    }
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

    let blob = match S3Storage::from_env() {
        Ok(blob) => blob,
        Err(e) => {
            error!("Invalid S3 configuration: {e}");
            std::process::exit(1);
        }
    };
    // Wait for the bucket before doing anything: Compose can only wait for the
    // object-store container to start, not for the S3 API to be ready. The
    // worker owns the bounded readiness/create-bucket wait so polling/listing
    // never starts against a non-existent bucket.
    if let Err(e) = blob.wait_for_bucket(30, Duration::from_secs(2)).await {
        error!("Giving up ensuring S3 bucket: {e}");
        std::process::exit(1);
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

    // Poll/act cadence, defaulted by role (env overrides): render polls fast to
    // pick up new jobs quickly; maintenance aggregates/prunes on a slower beat.
    // Clamp to >= 1s: a 0 interval would busy-loop the poll against S3.
    let interval = env_u64("WORKER_INTERVAL_SECONDS", default_interval_seconds(role)).max(1);
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

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

    loop {
        if *shutdown_rx.borrow() {
            break;
        }

        // Render role: drain every claimable shard across all jobs (pending, plus
        // orphaned ones whose lease expired). Scale these workers to split a big
        // batch; each renders items to content-addressed keys (idempotent).
        if let Some(registry) = &registry {
            loop {
                if *shutdown_rx.borrow() {
                    break;
                }

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
                            .run_claimed_shard(meta, shard, inputs, lease_secs, || {
                                *shutdown_rx.borrow()
                            })
                            .await
                        {
                            error!("Job {} shard {} failed: {}", job_id, shard_index, e);
                        }
                        // Persist analytics records staged during the shard.
                        if let Err(e) = registry.render_storage().flush().await {
                            error!(
                                "Analytics flush after job {} shard {} failed: {}",
                                job_id, shard_index, e
                            );
                        }
                        if *shutdown_rx.borrow() {
                            break;
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

        if *shutdown_rx.borrow() {
            break;
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
        }

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(interval)) => {}
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }

    info!("Shutdown signal received; worker exiting");
    if let Some(registry) = &registry
        && let Err(e) = registry.render_storage().flush().await
    {
        warn!("Final analytics flush failed during shutdown: {}", e);
    }
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_parsing_accepts_documented_aliases() {
        for value in [Some("render"), Some("renderer"), Some(" RENDER ")] {
            assert_eq!(Role::from_value(value), Role::Render);
        }

        for value in [Some("maintenance"), Some("maint"), Some(" MAINT ")] {
            assert_eq!(Role::from_value(value), Role::Maintenance);
        }

        for value in [None, Some(""), Some("all"), Some("both"), Some(" BOTH ")] {
            assert_eq!(Role::from_value(value), Role::All);
        }
    }

    #[test]
    fn unknown_roles_default_to_all_responsibilities() {
        let role = Role::from_value(Some("unknown"));

        assert_eq!(role, Role::All);
        assert!(role.renders());
        assert!(role.maintains());
    }

    #[test]
    fn roles_advertise_their_responsibilities() {
        assert!(Role::Render.renders());
        assert!(!Role::Render.maintains());

        assert!(!Role::Maintenance.renders());
        assert!(Role::Maintenance.maintains());

        assert!(Role::All.renders());
        assert!(Role::All.maintains());
    }

    #[test]
    fn role_defaults_use_render_fast_maintenance_slow_and_all_between() {
        assert_eq!(default_interval_seconds(Role::Render), 5);
        assert_eq!(default_interval_seconds(Role::Maintenance), 30);
        assert_eq!(default_interval_seconds(Role::All), 10);
    }

    #[test]
    fn numeric_env_helpers_accept_valid_values_and_fall_back_otherwise() {
        assert_eq!(env_u64_value("TEST", Some("12"), 5), 12);
        assert_eq!(env_u64_value("TEST", Some("not-a-number"), 5), 5);
        assert_eq!(env_u64_value("TEST", None, 5), 5);

        assert_eq!(env_u32_value("TEST", Some("7"), 3), 7);
        assert_eq!(env_u32_value("TEST", Some("-1"), 3), 3);
        assert_eq!(env_u32_value("TEST", None, 3), 3);
    }
}
