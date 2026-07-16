//! Output retention & housekeeping.
//!
//! Expiry lives in the key space: each expiring render's id is recorded under
//! `expiry/dt=<expiry-date>/…` at flush time. Pruning lists only the
//! day-partitions that are due and deletes their artifacts — cost is
//! O(items expiring today), not O(all outputs).

use time::{Date, Duration};

use super::layout;
use super::types::{RenderRecord, RenderStorageError};
use crate::address::ContentAddress;
use crate::batch::BatchJob;
use crate::storage::BlobStorage;
use crate::storage::blob_storage::StorageError;

/// Resolve the effective retention for a render, in days.
///
/// Precedence: per-render override → per-template default → global default.
/// A resolved value of `0` means "keep forever".
pub fn effective_retain_days(
    per_render: Option<u32>,
    per_template: Option<u32>,
    global: u32,
) -> u32 {
    per_render.or(per_template).unwrap_or(global)
}

/// Compute the expiry date for a render given its render date and effective
/// retention. Returns `None` for "keep forever" (`retain_days == 0`).
pub fn expiry_date(render_date: Date, retain_days: u32) -> Option<Date> {
    if retain_days == 0 {
        None
    } else {
        render_date.checked_add(Duration::days(retain_days as i64))
    }
}

/// Outcome of a prune pass (for logging/tests).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PruneStats {
    /// Number of distinct render_ids whose artifacts were pruned.
    pub renders_pruned: usize,
    /// Number of consumed expiry-index files.
    pub expiry_files_consumed: usize,
    /// Number of old analytics-raw files deleted.
    pub raw_files_deleted: usize,
    /// Number of stale batch-job documents deleted.
    pub jobs_pruned: usize,
}

/// Prune expired outputs and old analytics raw.
///
/// - Deletes `renders/{id}/{meta.json,pdf,data}` for every render_id in an
///   expiry partition whose date is `<= today`, then deletes those consumed
///   expiry files.
/// - Deletes `analytics/raw/dt=<old>/…` older than `analytics_retention_days`
///   (the persisted `summary.json` keeps the rollups, so history survives).
/// - Deletes `jobs/{id}/…` batch-job documents whose `updated_at` is older than
///   `job_retention_days` (status trackers accrue one per submission). `0`
///   disables job pruning (keep forever).
///
/// `today` is passed in for deterministic tests.
pub async fn prune<B: BlobStorage + ?Sized>(
    blob: &B,
    today: Date,
    analytics_retention_days: u32,
    job_retention_days: u32,
) -> Result<PruneStats, RenderStorageError> {
    let mut stats = PruneStats::default();

    // 1. Expiry-driven artifact pruning — only due day-partitions are touched.
    let expiry_keys = blob
        .list_keys(layout::EXPIRY_PREFIX)
        .await
        .map_err(|e| RenderStorageError::Query(e.to_string()))?;

    let mut due_render_ids: Vec<String> = Vec::new();
    let mut consumed_expiry_files: Vec<String> = Vec::new();
    for key in &expiry_keys {
        match layout::parse_dt(key) {
            Some(dt) if dt <= today => {
                let bytes = blob
                    .get(key)
                    .await
                    .map_err(|e| RenderStorageError::Query(e.to_string()))?;
                let text = String::from_utf8_lossy(&bytes);
                for line in text.lines() {
                    let id = line.trim();
                    if !id.is_empty() {
                        due_render_ids.push(id.to_string());
                    }
                }
                consumed_expiry_files.push(key.clone());
            }
            _ => {}
        }
    }

    // A render can appear in more than one expiry partition (e.g. re-rendered
    // with a longer retention); dedupe so we read each meta.json once.
    due_render_ids.sort_unstable();
    due_render_ids.dedup();

    // The expiry index is a hint, not the source of truth: it is written once at
    // render time and never revisited, so a render re-rendered with a longer or
    // "keep forever" retention overwrites its meta.json while leaving the old
    // entry behind. Consult each render's *current* meta.json and only prune
    // those whose recorded expiry is actually due. The stale entries are still
    // consumed below so they are not re-read every cycle.
    let mut artifact_keys = Vec::with_capacity(due_render_ids.len() * 3);
    for id in &due_render_ids {
        if expiry_entry_is_due(blob, id, today).await? {
            artifact_keys.push(ContentAddress::render_meta_key(id));
            artifact_keys.push(ContentAddress::render_pdf_key(id));
            artifact_keys.push(ContentAddress::render_data_key(id));
            stats.renders_pruned += 1;
        }
    }
    blob.delete_many(&artifact_keys)
        .await
        .map_err(|e| RenderStorageError::Query(e.to_string()))?;
    blob.delete_many(&consumed_expiry_files)
        .await
        .map_err(|e| RenderStorageError::Query(e.to_string()))?;
    stats.expiry_files_consumed = consumed_expiry_files.len();

    // 2. Analytics-raw pruning — independent, usually short retention.
    let raw_cutoff = today - Duration::days(analytics_retention_days as i64);
    let raw_keys = blob
        .list_keys(layout::RAW_PREFIX)
        .await
        .map_err(|e| RenderStorageError::Query(e.to_string()))?;
    let old_raw: Vec<String> = raw_keys
        .into_iter()
        .filter(|k| matches!(layout::parse_dt(k), Some(dt) if dt < raw_cutoff))
        .collect();
    blob.delete_many(&old_raw)
        .await
        .map_err(|e| RenderStorageError::Query(e.to_string()))?;
    stats.raw_files_deleted = old_raw.len();

    // 3. Batch-job pruning — a job's metadata + shard subtree (descriptors,
    //    inputs, results) accrues per submission and nothing else removes it.
    //    Drop whole jobs created before the cutoff (short-lived by design; a job
    //    older than the window is done or abandoned). `0` = keep forever.
    if job_retention_days > 0 {
        let job_cutoff = today - Duration::days(job_retention_days as i64);
        let job_keys = blob
            .list_keys(layout::JOBS_PREFIX)
            .await
            .map_err(|e| RenderStorageError::Query(e.to_string()))?;
        let mut stale: Vec<String> = Vec::new();
        for key in job_keys.iter().filter(|k| k.ends_with("/job.json")) {
            let bytes = blob
                .get(key)
                .await
                .map_err(|e| RenderStorageError::Query(e.to_string()))?;
            if let Ok(job) = serde_json::from_slice::<BatchJob>(&bytes)
                && job.created_at.date() < job_cutoff
            {
                // Delete the entire jobs/{id}/ subtree plus any pending-shard
                // markers (kept in a sibling keyspace).
                let subtree = blob
                    .list_keys(&layout::job_prefix(&job.job_id))
                    .await
                    .map_err(|e| RenderStorageError::Query(e.to_string()))?;
                stale.extend(subtree);
                let markers = blob
                    .list_keys(&layout::pending_job_prefix(&job.job_id))
                    .await
                    .map_err(|e| RenderStorageError::Query(e.to_string()))?;
                stale.extend(markers);
                stats.jobs_pruned += 1;
            }
        }
        blob.delete_many(&stale)
            .await
            .map_err(|e| RenderStorageError::Query(e.to_string()))?;
    }

    Ok(stats)
}

/// Decide whether a due expiry-index entry should actually prune its render,
/// using the render's *current* `meta.json` as the source of truth.
///
/// - Meta present with an expiry that is unset ("keep forever") or still in the
///   future: the retention was extended after this entry was written — keep it.
/// - Meta present with an expiry `<= today`: genuinely due — prune.
/// - Meta absent: the render is already gone; report due so the (idempotent)
///   delete cleans up any orphaned `pdf`/`data` blobs.
/// - Meta present but unreadable: keep it, rather than risk deleting a record we
///   cannot verify.
///
/// Transient backend errors propagate so the caller retries on the next cycle
/// instead of silently keeping (leak) or deleting (data loss).
async fn expiry_entry_is_due<B: BlobStorage + ?Sized>(
    blob: &B,
    render_id: &str,
    today: Date,
) -> Result<bool, RenderStorageError> {
    let bytes = match blob.get(&ContentAddress::render_meta_key(render_id)).await {
        Ok(bytes) => bytes,
        Err(StorageError::NotFound(_)) => return Ok(true),
        Err(e) => return Err(RenderStorageError::Query(e.to_string())),
    };
    match serde_json::from_slice::<RenderRecord>(&bytes) {
        Ok(record) => Ok(match record.expiry_date {
            None => false,
            Some(expiry) => expiry <= today,
        }),
        Err(e) => {
            tracing::warn!(
                render_id,
                error = %e,
                "skipping prune of render with unreadable meta.json",
            );
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::BlobStorage;
    use crate::storage::blob_storage::MemoryStorage;
    use time::macros::date;

    #[test]
    fn test_effective_retain_precedence() {
        // Per-render override wins.
        assert_eq!(effective_retain_days(Some(1), Some(7), 30), 1);
        // Then per-template default.
        assert_eq!(effective_retain_days(None, Some(7), 30), 7);
        // Then global default.
        assert_eq!(effective_retain_days(None, None, 30), 30);
        // 0 (keep forever) is a real value, not "unset".
        assert_eq!(effective_retain_days(Some(0), Some(7), 30), 0);
    }

    #[test]
    fn test_expiry_date_keep_forever() {
        let render_date = date!(2026 - 07 - 09);
        assert_eq!(expiry_date(render_date, 0), None);
        assert_eq!(expiry_date(render_date, 30), Some(date!(2026 - 08 - 08)));
    }

    async fn write_expiry(blob: &MemoryStorage, dt: Date, instance: &str, ids: &[&str]) {
        let body = ids.join("\n");
        let key = layout::expiry_key(dt, instance, 1, 0);
        blob.put(&key, body.into_bytes()).await.unwrap();
    }

    fn record_with_expiry(id: &str, success: bool, expiry_date: Option<Date>) -> RenderRecord {
        RenderRecord {
            render_id: id.to_string(),
            timestamp: date!(2026 - 07 - 01).midnight().assume_utc(),
            template_ref: "invoice:latest".to_string(),
            template_name: "invoice".to_string(),
            template_tag: "latest".to_string(),
            manifest_hash: "sha256:test".to_string(),
            data_hash: "sha256:data".to_string(),
            pdf_hash: if success {
                "sha256:pdf".to_string()
            } else {
                String::new()
            },
            success,
            duration_ms: 1,
            pdf_size_bytes: if success { 4 } else { 0 },
            error: (!success).then(|| "boom".to_string()),
            expiry_date,
        }
    }

    /// Write a render's `meta.json` (a real [`RenderRecord`] carrying
    /// `expiry_date`), `data`, and — for a successful render — `pdf`.
    async fn write_artifacts(
        blob: &MemoryStorage,
        id: &str,
        with_pdf: bool,
        expiry_date: Option<Date>,
    ) {
        let record = record_with_expiry(id, with_pdf, expiry_date);
        blob.put(
            &ContentAddress::render_meta_key(id),
            serde_json::to_vec(&record).unwrap(),
        )
        .await
        .unwrap();
        blob.put(&ContentAddress::render_data_key(id), b"{}".to_vec())
            .await
            .unwrap();
        if with_pdf {
            blob.put(&ContentAddress::render_pdf_key(id), b"%PDF".to_vec())
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn test_prune_deletes_due_leaves_future() {
        let blob = MemoryStorage::new();
        let today = date!(2026 - 07 - 09);

        // Due (yesterday) — one success (with pdf) and one failed (no pdf).
        write_artifacts(&blob, "due-ok", true, Some(date!(2026 - 07 - 08))).await;
        write_artifacts(&blob, "due-failed", false, Some(date!(2026 - 07 - 08))).await;
        write_expiry(
            &blob,
            date!(2026 - 07 - 08),
            "inst-1",
            &["due-ok", "due-failed"],
        )
        .await;

        // Not yet due (tomorrow) — must survive.
        write_artifacts(&blob, "future", true, Some(date!(2026 - 07 - 10))).await;
        write_expiry(&blob, date!(2026 - 07 - 10), "inst-1", &["future"]).await;

        let stats = prune(&blob, today, 90, 7).await.unwrap();
        assert_eq!(stats.renders_pruned, 2);
        assert_eq!(stats.expiry_files_consumed, 1);

        // Due artifacts gone (including the failed one with no pdf — no error).
        assert!(
            !blob
                .exists(&ContentAddress::render_meta_key("due-ok"))
                .await
                .unwrap()
        );
        assert!(
            !blob
                .exists(&ContentAddress::render_meta_key("due-failed"))
                .await
                .unwrap()
        );
        // Consumed expiry partition gone; future partition intact.
        assert_eq!(
            blob.list_keys(layout::EXPIRY_PREFIX).await.unwrap().len(),
            1
        );
        // Future artifacts intact.
        assert!(
            blob.exists(&ContentAddress::render_pdf_key("future"))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_prune_respects_current_meta_over_stale_expiry_index() {
        let blob = MemoryStorage::new();
        let today = date!(2026 - 07 - 09);

        // All three have a stale expiry-index entry for 07-08 (yesterday), but
        // their current meta.json disagrees for two of them:
        //   - `due`:      still expires 07-08                    → prune
        //   - `pinned`:   re-rendered "keep forever" (None)      → keep
        //   - `extended`: re-rendered with retention out to 07-20 → keep
        write_artifacts(&blob, "due", true, Some(date!(2026 - 07 - 08))).await;
        write_artifacts(&blob, "pinned", true, None).await;
        write_artifacts(&blob, "extended", true, Some(date!(2026 - 07 - 20))).await;
        write_expiry(
            &blob,
            date!(2026 - 07 - 08),
            "inst-1",
            &["due", "pinned", "extended"],
        )
        .await;

        let stats = prune(&blob, today, 90, 7).await.unwrap();

        // Only the genuinely-due render is pruned.
        assert_eq!(stats.renders_pruned, 1);
        assert_eq!(stats.expiry_files_consumed, 1);
        assert!(
            !blob
                .exists(&ContentAddress::render_meta_key("due"))
                .await
                .unwrap()
        );

        // Pinned and extended survive despite the stale index entry.
        for survivor in ["pinned", "extended"] {
            assert!(
                blob.exists(&ContentAddress::render_meta_key(survivor))
                    .await
                    .unwrap(),
                "{survivor} meta should survive",
            );
            assert!(
                blob.exists(&ContentAddress::render_pdf_key(survivor))
                    .await
                    .unwrap(),
                "{survivor} pdf should survive",
            );
        }

        // The stale expiry partition is still consumed so it is not re-scanned.
        assert!(
            blob.list_keys(layout::EXPIRY_PREFIX)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn test_prune_old_analytics_raw() {
        let blob = MemoryStorage::new();
        let today = date!(2026 - 07 - 09);

        // Old raw (100 days ago) vs recent raw (today) with retention 90.
        blob.put(
            &layout::raw_key(date!(2026 - 03 - 31), "inst-1", 1, 0),
            b"{}\n".to_vec(),
        )
        .await
        .unwrap();
        blob.put(&layout::raw_key(today, "inst-1", 2, 0), b"{}\n".to_vec())
            .await
            .unwrap();

        let stats = prune(&blob, today, 90, 7).await.unwrap();
        assert_eq!(stats.raw_files_deleted, 1);
        assert_eq!(blob.list_keys(layout::RAW_PREFIX).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_prune_stale_jobs() {
        use crate::batch::BatchJob;
        let blob = MemoryStorage::new();
        let today = date!(2026 - 07 - 09);

        let mk = |id: &str, created: Date| BatchJob {
            job_id: id.to_string(),
            reference: "invoice:latest".to_string(),
            total: 1,
            retain_days: None,
            pdf_standards: Vec::new(),
            shard_size: 500,
            num_shards: 1,
            created_at: created.midnight().assume_utc(),
        };

        // Stale (10 days old) + fresh (today); job retention 7 days. Write each
        // job's metadata plus a representative file in its shard subtree.
        for j in [mk("stale", date!(2026 - 06 - 29)), mk("fresh", today)] {
            blob.put(&layout::job_key(&j.job_id), serde_json::to_vec(&j).unwrap())
                .await
                .unwrap();
            blob.put(&layout::shard_inputs_key(&j.job_id, 0), b"[]".to_vec())
                .await
                .unwrap();
            blob.put(&layout::shard_key(&j.job_id, 0), b"{}".to_vec())
                .await
                .unwrap();
        }

        let stats = prune(&blob, today, 90, 7).await.unwrap();
        assert_eq!(stats.jobs_pruned, 1);
        // Stale job's whole subtree gone; fresh job intact.
        assert!(
            blob.list_keys(&layout::job_prefix("stale"))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            !blob
                .list_keys(&layout::job_prefix("fresh"))
                .await
                .unwrap()
                .is_empty()
        );

        // Retention 0 keeps everything.
        let stats0 = prune(&blob, today, 90, 0).await.unwrap();
        assert_eq!(stats0.jobs_pruned, 0);
    }
}
