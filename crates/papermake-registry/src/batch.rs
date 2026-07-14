//! Async batch-render jobs (sharded, multi-worker).
//!
//! A batch job is durable and worker-processed:
//! - the server **enqueues** it: writes immutable metadata `jobs/{id}/job.json`
//!   and splits the inputs into fixed-size **shards**, each with its own
//!   `jobs/{id}/shards/{k}/inputs.json` + `shard.json` (status `pending`);
//! - **any number of workers** each drain claimable shards: a shard is claimed
//!   with an owner + lease, its items rendered, then marked `done` with a
//!   `results.json`. Different shards are independent keys, so workers never
//!   contend, and one big batch is split across all of them;
//! - if a worker dies, its shard's lease expires and another worker reclaims it,
//!   **resuming** only items whose (content-addressed) output doesn't yet exist.
//!
//! No compare-and-set is needed: render output is content-addressed and thus
//! idempotent, so a rare double-claim only wastes CPU — it never corrupts. The
//! optimistic claim (write owner, re-read) plus leases keep duplication rare.
//!
//! Overall job status/counts are **derived** by aggregating the shard
//! descriptors (no single contended document); PDFs are fetched per `render_id`.

use papermake::PdfStandard;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// One input in a batch: the data to render plus an optional caller-chosen key
/// echoed back on the corresponding result item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchInput {
    pub data: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// Overall (derived) job state, aggregated from the shard descriptors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// No shard claimed yet.
    Queued,
    /// At least one shard is being rendered (or already done) but not all are terminal.
    Running,
    /// Every shard is terminal (with at least one rendered).
    Completed,
    /// Every shard was abandoned (poison / unrenderable template).
    Failed,
}

/// Per-shard state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShardStatus {
    /// Awaiting a worker.
    Pending,
    /// Claimed by a worker and rendering (see `owner`/`lease_expires_at`).
    Running,
    /// All the shard's items processed.
    Done,
    /// Abandoned after too many claims (poison), or the template couldn't load.
    Failed,
}

/// Per-item state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    Pending,
    Success,
    Failed,
}

/// Result of one input in the batch (persisted in a shard's `results.json`).
/// `render_id` is content-addressed; fetch its PDF at `renders/{render_id}/pdf`.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BatchItem {
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub render_id: Option<String>,
    pub status: ItemStatus,
}

/// Immutable job metadata (`jobs/{job_id}/job.json`), written once at enqueue.
/// Progress and status live in the per-shard descriptors and are derived on read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchJob {
    pub job_id: String,
    pub reference: String,
    pub total: usize,
    /// Per-batch retention override (days) captured at submit time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retain_days: Option<u32>,
    /// PDF export standards applied to every render in the batch (empty = plain
    /// PDF 1.7). Captured at submit time so all shards render consistently.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pdf_standards: Vec<PdfStandard>,
    pub shard_size: usize,
    pub num_shards: usize,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// One shard's descriptor (`jobs/{job_id}/shards/{index}/shard.json`). Written
/// only by the shard's current owner, so shards never contend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Shard {
    pub job_id: String,
    pub index: usize,
    /// First global item index this shard covers.
    pub start: usize,
    /// Number of items in this shard.
    pub len: usize,
    pub status: ShardStatus,
    /// Worker currently processing (if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// When the current owner's lease expires; past this, the shard is reclaimable.
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub lease_expires_at: Option<OffsetDateTime>,
    pub done: usize,
    pub failed: usize,
    /// How many times a worker has claimed this shard (poison guard).
    #[serde(default)]
    pub attempts: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// Aggregated, read-time view of a job (status + counts derived from shards).
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct JobView {
    pub job_id: String,
    pub reference: String,
    pub status: JobStatus,
    pub total: usize,
    pub done: usize,
    pub failed: usize,
    pub num_shards: usize,
    /// Shards in a terminal state (`Done` or `Failed`).
    pub shards_terminal: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retain_days: Option<u32>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl JobView {
    /// Aggregate a job's shard descriptors into an overall view.
    pub fn aggregate(meta: &BatchJob, shards: &[Shard]) -> Self {
        let done = shards.iter().map(|s| s.done).sum();
        let failed = shards.iter().map(|s| s.failed).sum();
        let terminal = |s: &Shard| matches!(s.status, ShardStatus::Done | ShardStatus::Failed);
        let shards_terminal = shards.iter().filter(|s| terminal(s)).count();

        let status = if meta.num_shards == 0 {
            JobStatus::Completed
        } else if shards.len() == meta.num_shards && shards.iter().all(terminal) {
            if shards.iter().all(|s| s.status == ShardStatus::Failed) {
                JobStatus::Failed
            } else {
                JobStatus::Completed
            }
        } else if shards
            .iter()
            .any(|s| matches!(s.status, ShardStatus::Running | ShardStatus::Done))
        {
            JobStatus::Running
        } else {
            JobStatus::Queued
        };

        Self {
            job_id: meta.job_id.clone(),
            reference: meta.reference.clone(),
            status,
            total: meta.total,
            done,
            failed,
            num_shards: meta.num_shards,
            shards_terminal,
            retain_days: meta.retain_days,
            created_at: meta.created_at,
        }
    }
}
