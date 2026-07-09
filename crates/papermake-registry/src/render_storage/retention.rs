//! Output retention & housekeeping.
//!
//! Expiry lives in the key space: each expiring render's id is recorded under
//! `expiry/dt=<expiry-date>/…` at flush time. Pruning lists only the
//! day-partitions that are due and deletes their artifacts — cost is
//! O(items expiring today), not O(all outputs).

use time::{Date, Duration};

use super::layout;
use super::types::RenderStorageError;
use crate::address::ContentAddress;
use crate::storage::BlobStorage;

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
}

/// Prune expired outputs and old analytics raw.
///
/// - Deletes `renders/{id}/{meta.json,pdf,data}` for every render_id in an
///   expiry partition whose date is `<= today`, then deletes those consumed
///   expiry files.
/// - Deletes `analytics/raw/dt=<old>/…` older than `analytics_retention_days`
///   (the persisted `summary.json` keeps the rollups, so history survives).
///
/// `today` is passed in for deterministic tests.
pub async fn prune<B: BlobStorage + ?Sized>(
    blob: &B,
    today: Date,
    analytics_retention_days: u32,
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

    let mut artifact_keys = Vec::with_capacity(due_render_ids.len() * 3);
    for id in &due_render_ids {
        artifact_keys.push(ContentAddress::render_meta_key(id));
        artifact_keys.push(ContentAddress::render_pdf_key(id));
        artifact_keys.push(ContentAddress::render_data_key(id));
    }
    blob.delete_many(&artifact_keys)
        .await
        .map_err(|e| RenderStorageError::Query(e.to_string()))?;
    blob.delete_many(&consumed_expiry_files)
        .await
        .map_err(|e| RenderStorageError::Query(e.to_string()))?;
    stats.renders_pruned = due_render_ids.len();
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

    Ok(stats)
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

    async fn write_artifacts(blob: &MemoryStorage, id: &str, with_pdf: bool) {
        blob.put(&ContentAddress::render_meta_key(id), b"{}".to_vec())
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
        write_artifacts(&blob, "due-ok", true).await;
        write_artifacts(&blob, "due-failed", false).await;
        write_expiry(
            &blob,
            date!(2026 - 07 - 08),
            "inst-1",
            &["due-ok", "due-failed"],
        )
        .await;

        // Not yet due (tomorrow) — must survive.
        write_artifacts(&blob, "future", true).await;
        write_expiry(&blob, date!(2026 - 07 - 10), "inst-1", &["future"]).await;

        let stats = prune(&blob, today, 90).await.unwrap();
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

        let stats = prune(&blob, today, 90).await.unwrap();
        assert_eq!(stats.raw_files_deleted, 1);
        assert_eq!(blob.list_keys(layout::RAW_PREFIX).await.unwrap().len(), 1);
    }
}
