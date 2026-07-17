//! Buffered-S3 render store.
//!
//! Implements [`RenderStorage`] over any [`BlobStorage`]. Records are staged in
//! an in-memory buffer (write-only) and flushed to S3 as NDJSON on an interval
//! or size threshold. Analytics queries are answered **only** from the S3
//! aggregate (`summary.json`) — never from the buffer — so every instance sees
//! one globally-consistent view.
//!
//! By-id artifact/record reads (`get_render`) go straight to the render's
//! `meta.json` blob and are therefore immediate, independent of the flush.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use time::OffsetDateTime;
use tokio::sync::RwLock;

use super::RenderStorage;
use super::layout;
use super::summary::Summary;
use super::types::{DurationPoint, RenderRecord, RenderStorageError, TemplateStats, VolumePoint};
use crate::address::ContentAddress;
use crate::storage::BlobStorage;
use crate::storage::blob_storage::StorageError;

/// Default buffer size threshold that triggers an eager flush.
pub const DEFAULT_FLUSH_MAX_RECORDS: usize = 1000;

/// Default multiple of `flush_max_records` the staging buffer may hold before it
/// starts dropping the oldest records. Re-staging on a failed flush would
/// otherwise grow the buffer without bound during a sustained backend outage.
pub const DEFAULT_BUFFER_BACKLOG_FACTOR: usize = 10;

/// Buffered render store backed by blob storage.
pub struct S3BufferedRenderStorage<B: BlobStorage> {
    blob: Arc<B>,
    /// Write-only staging buffer. Never a read source for analytics.
    buffer: RwLock<Vec<RenderRecord>>,
    /// Identifies this instance so flushed object keys never collide.
    instance_id: String,
    /// Flush eagerly once the buffer reaches this many records.
    flush_max_records: usize,
    /// Hard cap on staged records; the oldest are dropped past this so a
    /// sustained flush outage cannot grow the buffer without bound.
    max_buffered_records: usize,
    /// Monotonic sequence to disambiguate multiple flushes within one millisecond.
    seq: AtomicU64,
}

impl<B: BlobStorage> S3BufferedRenderStorage<B> {
    /// Create a new buffered store.
    ///
    /// `instance_id` defaults to a generated uuid when `None`.
    pub fn new(blob: Arc<B>, instance_id: Option<String>, flush_max_records: usize) -> Self {
        let flush_max_records = flush_max_records.max(1);
        Self {
            blob,
            buffer: RwLock::new(Vec::new()),
            instance_id: instance_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            flush_max_records,
            max_buffered_records: flush_max_records.saturating_mul(DEFAULT_BUFFER_BACKLOG_FACTOR),
            seq: AtomicU64::new(0),
        }
    }

    /// Override the cap on staged records (default:
    /// `flush_max_records * DEFAULT_BUFFER_BACKLOG_FACTOR`). Once re-staging
    /// after a failed flush would exceed this, the oldest records are dropped —
    /// their analytics/expiry entries are lost, but the process stays up rather
    /// than growing the buffer without bound through a backend outage.
    pub fn with_max_buffered_records(mut self, max: usize) -> Self {
        self.max_buffered_records = max.max(1);
        self
    }

    /// This instance's identifier (used in flushed object keys).
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Drain the buffer and write staged records to S3.
    ///
    /// Writes two partitionings of the drained records:
    /// - one **analytics-raw** NDJSON object (partitioned by render date, for
    ///   aggregation);
    /// - one **expiry-index** NDJSON object per distinct expiry date
    ///   (partitioned by expiry date, for pruning) — records with no
    ///   `expiry_date` ("keep forever") contribute no index entry.
    ///
    /// A no-op (nothing written) when the buffer is empty. Called by the
    /// background flush task on interval/threshold and on graceful shutdown.
    pub async fn flush(&self) -> Result<(), RenderStorageError> {
        let drained: Vec<RenderRecord> = {
            let mut buf = self.buffer.write().await;
            if buf.is_empty() {
                return Ok(());
            }
            std::mem::take(&mut *buf)
        };

        // If any put fails, put the drained records back so they are retried on
        // the next flush. The expiry-index entries they carry are the *only*
        // mechanism that ever prunes these renders' PDFs and input data, so
        // dropping them silently converts a batch of expiring renders into
        // "keep forever". Retrying is safe: keys embed instance_id/millis/seq
        // (so a partially-written flush never overwrites) and the aggregator
        // dedupes by render_id.
        if let Err(e) = self.write_drained(&drained).await {
            let mut buf = self.buffer.write().await;
            // Re-stage ahead of anything staged since the take, preserving order.
            let staged_since = std::mem::replace(&mut *buf, drained);
            buf.extend(staged_since);
            // Bound memory: a backend that keeps rejecting flushes would
            // otherwise let the buffer grow one batch per store forever.
            self.enforce_buffer_cap(&mut buf);
            return Err(e);
        }
        Ok(())
    }

    /// Drop the oldest staged records if the buffer is over its cap, keeping the
    /// most recent `max_buffered_records`. The dropped records' analytics and
    /// expiry-index entries are lost (logged), which is the deliberate trade for
    /// surviving a sustained flush outage without unbounded memory growth.
    fn enforce_buffer_cap(&self, buf: &mut Vec<RenderRecord>) {
        if buf.len() > self.max_buffered_records {
            let dropped = buf.len() - self.max_buffered_records;
            buf.drain(0..dropped);
            tracing::warn!(
                dropped,
                retained = self.max_buffered_records,
                "analytics buffer over capacity during a flush outage; dropped oldest staged records",
            );
        }
    }

    /// Write one analytics-raw object plus the per-expiry-date index objects for
    /// `drained` to blob storage. On any error the caller re-stages the records.
    async fn write_drained(&self, drained: &[RenderRecord]) -> Result<(), RenderStorageError> {
        let now = OffsetDateTime::now_utc();
        let unix_millis = (now.unix_timestamp_nanos() / 1_000_000) as u128;

        // Analytics raw (full records, partitioned by render date).
        let mut by_render_date: std::collections::BTreeMap<time::Date, Vec<&RenderRecord>> =
            std::collections::BTreeMap::new();
        for record in drained {
            by_render_date
                .entry(record.timestamp.date())
                .or_default()
                .push(record);
        }
        for (render_date, records) in by_render_date {
            let mut body = String::new();
            for record in records {
                body.push_str(&serde_json::to_string(record)?);
                body.push('\n');
            }
            let raw_seq = self.seq.fetch_add(1, Ordering::Relaxed);
            let raw_key = layout::raw_key(render_date, &self.instance_id, unix_millis, raw_seq);
            self.blob
                .put(&raw_key, body.into_bytes())
                .await
                .map_err(|e| RenderStorageError::Query(e.to_string()))?;
        }

        // Expiry index (render_ids grouped by expiry date).
        let mut by_expiry: std::collections::BTreeMap<time::Date, Vec<&str>> =
            std::collections::BTreeMap::new();
        for record in drained {
            if let Some(dt) = record.expiry_date {
                by_expiry.entry(dt).or_default().push(&record.render_id);
            }
        }
        for (expiry, ids) in by_expiry {
            let mut idx_body = String::new();
            for id in ids {
                idx_body.push_str(id);
                idx_body.push('\n');
            }
            let seq = self.seq.fetch_add(1, Ordering::Relaxed);
            let key = layout::expiry_key(expiry, &self.instance_id, unix_millis, seq);
            self.blob
                .put(&key, idx_body.into_bytes())
                .await
                .map_err(|e| RenderStorageError::Query(e.to_string()))?;
        }
        Ok(())
    }

    /// Number of records currently staged (test/observability helper).
    pub async fn buffered_len(&self) -> usize {
        self.buffer.read().await.len()
    }

    /// Load the aggregated summary, or an empty summary if none exists yet.
    async fn load_summary(&self) -> Result<Summary, RenderStorageError> {
        match self.blob.get(layout::SUMMARY_KEY).await {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(StorageError::NotFound(_)) => Ok(Summary::empty(OffsetDateTime::now_utc())),
            Err(e) => Err(RenderStorageError::Query(e.to_string())),
        }
    }
}

#[async_trait]
impl<B: BlobStorage + 'static> RenderStorage for S3BufferedRenderStorage<B> {
    async fn store_render(&self, record: RenderRecord) -> Result<(), RenderStorageError> {
        let should_flush = {
            let mut buf = self.buffer.write().await;
            buf.push(record);
            buf.len() >= self.flush_max_records
        };
        if should_flush {
            self.flush().await?;
        }
        Ok(())
    }

    async fn get_render(
        &self,
        render_id: &str,
    ) -> Result<Option<RenderRecord>, RenderStorageError> {
        // Direct read of the render's meta.json — immediate, by-id, flush-independent.
        match self
            .blob
            .get(&ContentAddress::render_meta_key(render_id))
            .await
        {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(StorageError::NotFound(_)) => Ok(None),
            Err(e) => Err(RenderStorageError::Query(e.to_string())),
        }
    }

    async fn list_recent_renders(
        &self,
        limit: u32,
    ) -> Result<Vec<RenderRecord>, RenderStorageError> {
        let summary = self.load_summary().await?;
        Ok(summary.recent.into_iter().take(limit as usize).collect())
    }

    async fn list_template_renders(
        &self,
        template_name: &str,
        limit: u32,
    ) -> Result<Vec<RenderRecord>, RenderStorageError> {
        let summary = self.load_summary().await?;
        Ok(summary
            .templates
            .into_iter()
            .find(|t| t.template_name == template_name)
            .map(|t| t.recent.into_iter().take(limit as usize).collect())
            .unwrap_or_default())
    }

    async fn render_volume_over_time(
        &self,
        days: u32,
    ) -> Result<Vec<VolumePoint>, RenderStorageError> {
        let summary = self.load_summary().await?;
        let cutoff = (OffsetDateTime::now_utc() - time::Duration::days(days as i64)).date();
        Ok(summary
            .volume_by_day
            .into_iter()
            .filter(|v| v.date >= cutoff)
            .collect())
    }

    async fn total_renders_per_template(&self) -> Result<Vec<TemplateStats>, RenderStorageError> {
        let summary = self.load_summary().await?;
        Ok(summary
            .templates
            .into_iter()
            .map(|t| TemplateStats {
                template_name: t.template_name,
                total_renders: t.total_renders,
            })
            .collect())
    }

    async fn average_duration_over_time(
        &self,
        days: u32,
    ) -> Result<Vec<DurationPoint>, RenderStorageError> {
        let summary = self.load_summary().await?;
        let cutoff = (OffsetDateTime::now_utc() - time::Duration::days(days as i64)).date();
        Ok(summary
            .duration_by_day
            .into_iter()
            .filter(|d| d.date >= cutoff)
            .collect())
    }

    async fn summary(&self) -> Result<Summary, RenderStorageError> {
        self.load_summary().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_storage::summary::{Summary, TemplateSummary, Totals};
    use crate::storage::blob_storage::MemoryStorage;
    use std::sync::atomic::AtomicBool;
    use time::macros::datetime;

    /// Blob storage that can be toggled to fail every `put`, delegating all
    /// other operations to an inner in-memory store.
    struct TogglePutStorage {
        inner: MemoryStorage,
        fail_puts: AtomicBool,
    }

    impl TogglePutStorage {
        fn new() -> Self {
            Self {
                inner: MemoryStorage::new(),
                fail_puts: AtomicBool::new(false),
            }
        }

        fn set_fail_puts(&self, fail: bool) {
            self.fail_puts.store(fail, Ordering::Relaxed);
        }
    }

    #[async_trait]
    impl BlobStorage for TogglePutStorage {
        async fn put(&self, key: &str, data: Vec<u8>) -> Result<(), StorageError> {
            if self.fail_puts.load(Ordering::Relaxed) {
                return Err(StorageError::Backend("injected put failure".to_string()));
            }
            self.inner.put(key, data).await
        }
        async fn get(&self, key: &str) -> Result<Vec<u8>, StorageError> {
            self.inner.get(key).await
        }
        async fn exists(&self, key: &str) -> Result<bool, StorageError> {
            self.inner.exists(key).await
        }
        async fn delete(&self, key: &str) -> Result<(), StorageError> {
            self.inner.delete(key).await
        }
        async fn list_keys(&self, prefix: &str) -> Result<Vec<String>, StorageError> {
            self.inner.list_keys(prefix).await
        }
    }

    fn record(name: &str) -> RenderRecord {
        RenderRecord::success(
            format!("{}:latest", name),
            name.to_string(),
            "latest".to_string(),
            "sha256:m".to_string(),
            "sha256:d".to_string(),
            "sha256:p".to_string(),
            100,
            1024,
        )
    }

    #[tokio::test]
    async fn test_instance_id_reports_explicit_or_generated_id() {
        let explicit = S3BufferedRenderStorage::new(
            Arc::new(MemoryStorage::new()),
            Some("inst-1".to_string()),
            1000,
        );
        assert_eq!(explicit.instance_id(), "inst-1");

        let generated = S3BufferedRenderStorage::new(Arc::new(MemoryStorage::new()), None, 1000);
        assert!(!generated.instance_id().is_empty());
    }

    #[tokio::test]
    async fn test_store_then_flush_roundtrip() {
        let blob = Arc::new(MemoryStorage::new());
        let store = S3BufferedRenderStorage::new(blob.clone(), Some("inst-1".to_string()), 1000);

        store.store_render(record("invoice")).await.unwrap();
        store.store_render(record("letter")).await.unwrap();
        assert_eq!(store.buffered_len().await, 2);

        store.flush().await.unwrap();
        assert_eq!(store.buffered_len().await, 0);

        // Exactly one NDJSON object under the raw prefix for this instance.
        let keys = blob.list_keys(layout::RAW_PREFIX).await.unwrap();
        assert_eq!(keys.len(), 1);
        assert!(keys[0].contains("/inst-1/"));

        let bytes = blob.get(&keys[0]).await.unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let parsed: Vec<RenderRecord> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].template_name, "invoice");
    }

    #[tokio::test]
    async fn test_flush_partitions_raw_records_by_render_date() {
        let blob = Arc::new(MemoryStorage::new());
        let store = S3BufferedRenderStorage::new(blob.clone(), Some("inst-1".to_string()), 1000);
        let d1 = time::macros::date!(2026 - 07 - 09);
        let d2 = time::macros::date!(2026 - 07 - 10);

        let mut r1 = record("invoice");
        r1.render_id = "r1".to_string();
        r1.timestamp = time::macros::datetime!(2026-07-09 23:59:00 UTC);
        let mut r2 = record("letter");
        r2.render_id = "r2".to_string();
        r2.timestamp = time::macros::datetime!(2026-07-10 00:01:00 UTC);

        store.store_render(r1).await.unwrap();
        store.store_render(r2).await.unwrap();
        store.flush().await.unwrap();

        let d1_keys = blob.list_keys(&layout::raw_date_prefix(d1)).await.unwrap();
        let d2_keys = blob.list_keys(&layout::raw_date_prefix(d2)).await.unwrap();
        assert_eq!(d1_keys.len(), 1);
        assert_eq!(d2_keys.len(), 1);
        assert_eq!(blob.list_keys(layout::RAW_PREFIX).await.unwrap().len(), 2);

        let d1_body = String::from_utf8(blob.get(&d1_keys[0]).await.unwrap()).unwrap();
        let d2_body = String::from_utf8(blob.get(&d2_keys[0]).await.unwrap()).unwrap();
        assert!(d1_body.contains("\"render_id\":\"r1\""));
        assert!(!d1_body.contains("\"render_id\":\"r2\""));
        assert!(d2_body.contains("\"render_id\":\"r2\""));
    }

    #[tokio::test]
    async fn test_flush_writes_expiry_index_grouped_by_expiry_date() {
        let blob = Arc::new(MemoryStorage::new());
        let store = S3BufferedRenderStorage::new(blob.clone(), Some("inst-1".to_string()), 1000);

        // Two records expiring on the same day, one on another, one "forever".
        let d1 = time::macros::date!(2026 - 08 - 01);
        let d2 = time::macros::date!(2026 - 08 - 02);
        let mut r1 = record("invoice");
        r1.render_id = "r1".to_string();
        r1.expiry_date = Some(d1);
        let mut r2 = record("invoice");
        r2.render_id = "r2".to_string();
        r2.expiry_date = Some(d1);
        let mut r3 = record("letter");
        r3.render_id = "r3".to_string();
        r3.expiry_date = Some(d2);
        let mut r4 = record("forever");
        r4.render_id = "r4".to_string();
        r4.expiry_date = None; // keep forever -> no index entry

        for r in [r1, r2, r3, r4] {
            store.store_render(r).await.unwrap();
        }
        store.flush().await.unwrap();

        // One expiry file per distinct expiry date (d1, d2) — none for "forever".
        let d1_keys = blob
            .list_keys(&layout::expiry_date_prefix(d1))
            .await
            .unwrap();
        let d2_keys = blob
            .list_keys(&layout::expiry_date_prefix(d2))
            .await
            .unwrap();
        assert_eq!(d1_keys.len(), 1);
        assert_eq!(d2_keys.len(), 1);
        let all_expiry = blob.list_keys(layout::EXPIRY_PREFIX).await.unwrap();
        assert_eq!(all_expiry.len(), 2);

        // d1 file carries both render_ids.
        let body = String::from_utf8(blob.get(&d1_keys[0]).await.unwrap()).unwrap();
        let ids: Vec<&str> = body.lines().collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"r1") && ids.contains(&"r2"));
    }

    #[tokio::test]
    async fn test_failed_flush_restages_records_and_retries() {
        let blob = Arc::new(TogglePutStorage::new());
        let store = S3BufferedRenderStorage::new(blob.clone(), Some("inst-1".to_string()), 1000);

        let mut r = record("invoice");
        r.render_id = "r1".to_string();
        r.expiry_date = Some(time::macros::date!(2026 - 08 - 01));
        store.store_render(r).await.unwrap();

        // A flush that fails must keep the record buffered, not drop it — losing
        // it would drop the only expiry-index entry that ever prunes its PDF.
        blob.set_fail_puts(true);
        assert!(store.flush().await.is_err());
        assert_eq!(store.buffered_len().await, 1);
        assert!(blob.list_keys(layout::RAW_PREFIX).await.unwrap().is_empty());
        assert!(
            blob.list_keys(layout::EXPIRY_PREFIX)
                .await
                .unwrap()
                .is_empty()
        );

        // Once storage recovers, the retained record flushes successfully,
        // including its expiry-index entry.
        blob.set_fail_puts(false);
        store.flush().await.unwrap();
        assert_eq!(store.buffered_len().await, 0);
        assert_eq!(blob.list_keys(layout::RAW_PREFIX).await.unwrap().len(), 1);
        assert_eq!(
            blob.list_keys(layout::EXPIRY_PREFIX).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn test_failed_flush_preserves_records_staged_during_flush() {
        let blob = Arc::new(TogglePutStorage::new());
        let store = S3BufferedRenderStorage::new(blob.clone(), Some("inst-1".to_string()), 1000);

        let mut r1 = record("invoice");
        r1.render_id = "r1".to_string();
        store.store_render(r1).await.unwrap();

        blob.set_fail_puts(true);
        assert!(store.flush().await.is_err());

        // A record staged after the failed flush must not be lost when the
        // drained batch is re-staged.
        let mut r2 = record("letter");
        r2.render_id = "r2".to_string();
        store.store_render(r2).await.unwrap();
        assert_eq!(store.buffered_len().await, 2);

        blob.set_fail_puts(false);
        store.flush().await.unwrap();

        let keys = blob.list_keys(layout::RAW_PREFIX).await.unwrap();
        assert_eq!(keys.len(), 1);
        let text = String::from_utf8(blob.get(&keys[0]).await.unwrap()).unwrap();
        let ids: Vec<String> = text
            .lines()
            .map(|l| serde_json::from_str::<RenderRecord>(l).unwrap().render_id)
            .collect();
        assert_eq!(ids, vec!["r1".to_string(), "r2".to_string()]);
    }

    #[tokio::test]
    async fn test_buffer_is_capped_during_a_flush_outage() {
        let blob = Arc::new(TogglePutStorage::new());
        // Threshold 1 => every store triggers a (failing) flush that re-stages;
        // cap 3 => the buffer must never hold more than the 3 most recent.
        let store = S3BufferedRenderStorage::new(blob.clone(), Some("inst-1".to_string()), 1)
            .with_max_buffered_records(3);
        blob.set_fail_puts(true);

        for i in 0..6 {
            let mut r = record("invoice");
            r.render_id = format!("r{i}");
            // The flush error is expected while the backend is down.
            let _ = store.store_render(r).await;
        }

        // Without the cap the buffer would hold all 6; with it, only 3.
        assert_eq!(store.buffered_len().await, 3);

        // Once the backend recovers, the retained records flush — and they are
        // the most recent three (the oldest were dropped to bound memory).
        blob.set_fail_puts(false);
        store.flush().await.unwrap();
        let keys = blob.list_keys(layout::RAW_PREFIX).await.unwrap();
        assert_eq!(keys.len(), 1);
        let text = String::from_utf8(blob.get(&keys[0]).await.unwrap()).unwrap();
        let ids: Vec<String> = text
            .lines()
            .map(|l| serde_json::from_str::<RenderRecord>(l).unwrap().render_id)
            .collect();
        assert_eq!(
            ids,
            vec!["r3".to_string(), "r4".to_string(), "r5".to_string()]
        );
    }

    #[tokio::test]
    async fn test_flush_empty_is_noop() {
        let blob = Arc::new(MemoryStorage::new());
        let store = S3BufferedRenderStorage::new(blob.clone(), Some("inst-1".to_string()), 1000);
        store.flush().await.unwrap();
        assert!(blob.list_keys(layout::RAW_PREFIX).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_auto_flush_on_threshold() {
        let blob = Arc::new(MemoryStorage::new());
        // Threshold of 2 → the second store_render triggers a flush.
        let store = S3BufferedRenderStorage::new(blob.clone(), Some("inst-1".to_string()), 2);
        store.store_render(record("invoice")).await.unwrap();
        assert_eq!(store.buffered_len().await, 1);
        store.store_render(record("letter")).await.unwrap();
        assert_eq!(store.buffered_len().await, 0);
        assert_eq!(blob.list_keys(layout::RAW_PREFIX).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_analytics_read_from_summary_not_buffer() {
        let blob = Arc::new(MemoryStorage::new());
        let store = S3BufferedRenderStorage::new(blob.clone(), Some("inst-1".to_string()), 1000);

        // A record staged in the buffer must NOT appear in analytics reads.
        store.store_render(record("buffered-only")).await.unwrap();
        assert!(store.list_recent_renders(10).await.unwrap().is_empty());

        // Write a summary directly; reads reflect it.
        let now = datetime!(2026-07-09 12:00 UTC);
        let mut rec_a = record("invoice");
        rec_a.render_id = "a".to_string();
        rec_a.timestamp = now;
        let summary = Summary {
            generated_at: now,
            volume_by_day: vec![VolumePoint {
                date: now.date(),
                renders: 1,
                failures: 0,
            }],
            duration_by_day: vec![DurationPoint {
                date: now.date(),
                avg_duration_ms: 100.0,
                p90_duration_ms: 100,
                p95_duration_ms: 100,
                p99_duration_ms: 100,
            }],
            duration_histogram: Vec::new(),
            templates: vec![TemplateSummary {
                template_name: "invoice".to_string(),
                total_renders: 1,
                by_tag: Vec::new(),
                recent: vec![rec_a.clone()],
            }],
            recent: vec![rec_a.clone()],
            totals: Totals {
                renders_24h: 1,
                success_rate_24h: 1.0,
                p90_latency_ms_24h: 100,
            },
        };
        blob.put(layout::SUMMARY_KEY, serde_json::to_vec(&summary).unwrap())
            .await
            .unwrap();

        let recent = store.list_recent_renders(10).await.unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].render_id, "a");

        let per_tpl = store.list_template_renders("invoice", 10).await.unwrap();
        assert_eq!(per_tpl.len(), 1);
        assert!(
            store
                .list_template_renders("missing", 10)
                .await
                .unwrap()
                .is_empty()
        );

        let stats = store.total_renders_per_template().await.unwrap();
        assert_eq!(stats[0].total_renders, 1);
    }

    #[tokio::test]
    async fn test_get_render_reads_meta_json() {
        let blob = Arc::new(MemoryStorage::new());
        let store = S3BufferedRenderStorage::new(blob.clone(), Some("inst-1".to_string()), 1000);

        assert!(store.get_render("nope").await.unwrap().is_none());

        let mut rec = record("invoice");
        rec.render_id = "rid-1".to_string();
        blob.put(
            &ContentAddress::render_meta_key("rid-1"),
            serde_json::to_vec(&rec).unwrap(),
        )
        .await
        .unwrap();

        let got = store.get_render("rid-1").await.unwrap().unwrap();
        assert_eq!(got.render_id, "rid-1");
        assert_eq!(got.template_name, "invoice");
    }
}
