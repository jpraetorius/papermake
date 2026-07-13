//! Async batch-render jobs.
//!
//! A batch job is durable and worker-processed:
//! - the server **enqueues** it (writes `jobs/{id}/inputs.json` + a `queued`
//!   `jobs/{id}/job.json`) and returns immediately;
//! - a single active worker **claims** it (sets `running` with an owner + lease),
//!   renders each input, and heartbeats the lease as it goes;
//! - if that worker dies, the lease expires and the next active worker reclaims
//!   the job and **resumes** the remaining items (those without a `render_id`).
//!
//! Because RustFS lacks atomic conditional writes, correctness relies on there
//! being a single active worker at a time (see the worker deployment); the lease
//! is what makes orphaned jobs recoverable rather than stuck.
//!
//! Clients poll `job.json`; rendered PDFs are fetched per `render_id`.

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

/// Overall job state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Submitted, awaiting a worker.
    Queued,
    /// Claimed by a worker and rendering (see `owner`/`lease_expires_at`).
    Running,
    /// All items processed.
    Completed,
    /// Abandoned after too many failed attempts (poison job).
    Failed,
}

/// Per-item state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    Pending,
    Success,
    Failed,
}

/// Result of one input in the batch. `render_id` is set once the item has been
/// rendered; fetch its PDF at `renders/{render_id}/pdf`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchItem {
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub render_id: Option<String>,
    pub status: ItemStatus,
}

/// The polled job document (`jobs/{job_id}/job.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchJob {
    pub job_id: String,
    pub reference: String,
    pub status: JobStatus,
    pub total: usize,
    pub done: usize,
    pub failed: usize,
    /// Per-batch retention override (days) captured at submit time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retain_days: Option<u32>,
    /// Worker currently processing (if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// When the current owner's lease expires; past this, the job is reclaimable.
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub lease_expires_at: Option<OffsetDateTime>,
    /// How many times a worker has claimed this job (poison-job guard).
    #[serde(default)]
    pub attempts: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    pub items: Vec<BatchItem>,
}
