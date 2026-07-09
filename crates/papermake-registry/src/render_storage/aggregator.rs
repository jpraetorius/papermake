//! Analytics aggregator.
//!
//! A pure function over [`BlobStorage`]: read all raw NDJSON records, compute a
//! [`Summary`], and write it to `layout::SUMMARY_KEY`. Run by the worker on an
//! interval; testable offline with `MemoryStorage`.
//!
//! The whole raw set is re-scanned each run (idempotent). Retention pruning of
//! `analytics/raw/` bounds that set; incremental watermarking is a later
//! optimization.

use time::OffsetDateTime;

use super::layout;
use super::summary::{Summary, compute_summary};
use super::types::{RenderRecord, RenderStorageError};
use crate::storage::BlobStorage;

/// Default number of recent renders retained per template and globally.
pub const DEFAULT_RECENT_N: usize = 50;

/// Read raw NDJSON records from `analytics/raw/`.
pub async fn load_raw_records<B: BlobStorage + ?Sized>(
    blob: &B,
) -> Result<Vec<RenderRecord>, RenderStorageError> {
    let keys = blob
        .list_keys(layout::RAW_PREFIX)
        .await
        .map_err(|e| RenderStorageError::Query(e.to_string()))?;

    let mut records = Vec::new();
    for key in keys {
        let bytes = blob
            .get(&key)
            .await
            .map_err(|e| RenderStorageError::Query(e.to_string()))?;
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let record: RenderRecord = serde_json::from_str(line)?;
            records.push(record);
        }
    }
    Ok(records)
}

/// Aggregate all raw records and write `summary.json`. Returns the summary.
pub async fn run<B: BlobStorage + ?Sized>(
    blob: &B,
    now: OffsetDateTime,
    recent_n: usize,
) -> Result<Summary, RenderStorageError> {
    let records = load_raw_records(blob).await?;
    let summary = compute_summary(&records, recent_n, now);
    let bytes = serde_json::to_vec(&summary)?;
    blob.put(layout::SUMMARY_KEY, bytes)
        .await
        .map_err(|e| RenderStorageError::Query(e.to_string()))?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::BlobStorage;
    use crate::storage::blob_storage::MemoryStorage;
    use time::macros::datetime;

    fn ndjson(records: &[RenderRecord]) -> Vec<u8> {
        let mut s = String::new();
        for r in records {
            s.push_str(&serde_json::to_string(r).unwrap());
            s.push('\n');
        }
        s.into_bytes()
    }

    fn rec(name: &str, ts: OffsetDateTime, success: bool, duration_ms: u32) -> RenderRecord {
        RenderRecord {
            render_id: format!("{}-{}-{}", name, ts.unix_timestamp(), duration_ms),
            timestamp: ts,
            template_ref: format!("{}:latest", name),
            template_name: name.to_string(),
            template_tag: "latest".to_string(),
            manifest_hash: "sha256:m".to_string(),
            data_hash: "sha256:d".to_string(),
            pdf_hash: "sha256:p".to_string(),
            success,
            duration_ms,
            pdf_size_bytes: 100,
            error: None,
        }
    }

    #[tokio::test]
    async fn test_aggregator_over_multiple_raw_files() {
        let blob = MemoryStorage::new();
        let now = datetime!(2026-07-09 12:00 UTC);

        // Two raw files from two instances, partitioned by render date.
        let batch_a = vec![
            rec("invoice", datetime!(2026-07-09 10:00 UTC), true, 100),
            rec("invoice", datetime!(2026-07-09 11:00 UTC), true, 200),
        ];
        let batch_b = vec![rec("letter", datetime!(2026-07-09 09:30 UTC), true, 300)];
        blob.put(
            &layout::raw_key(now.date(), "inst-a", 1, 0),
            ndjson(&batch_a),
        )
        .await
        .unwrap();
        blob.put(
            &layout::raw_key(now.date(), "inst-b", 2, 0),
            ndjson(&batch_b),
        )
        .await
        .unwrap();

        let summary = run(&blob, now, 50).await.unwrap();

        // Totals reflect all three records.
        assert_eq!(summary.totals.renders_24h, 3);
        assert_eq!(summary.templates.len(), 2);
        assert_eq!(summary.templates[0].template_name, "invoice");
        assert_eq!(summary.templates[0].total_renders, 2);

        // summary.json is persisted and re-readable.
        let persisted = blob.get(layout::SUMMARY_KEY).await.unwrap();
        let reloaded: Summary = serde_json::from_slice(&persisted).unwrap();
        assert_eq!(reloaded.totals.renders_24h, 3);
    }

    #[tokio::test]
    async fn test_aggregator_idempotent() {
        let blob = MemoryStorage::new();
        let now = datetime!(2026-07-09 12:00 UTC);
        blob.put(
            &layout::raw_key(now.date(), "inst-a", 1, 0),
            ndjson(&[rec("invoice", datetime!(2026-07-09 10:00 UTC), true, 100)]),
        )
        .await
        .unwrap();

        let s1 = run(&blob, now, 50).await.unwrap();
        let s2 = run(&blob, now, 50).await.unwrap();
        assert_eq!(s1.totals.renders_24h, s2.totals.renders_24h);
        assert_eq!(s1.templates.len(), s2.templates.len());
    }

    #[tokio::test]
    async fn test_aggregator_empty() {
        let blob = MemoryStorage::new();
        let now = datetime!(2026-07-09 12:00 UTC);
        let summary = run(&blob, now, 50).await.unwrap();
        assert_eq!(summary.totals.renders_24h, 0);
        assert!(summary.recent.is_empty());
    }
}
