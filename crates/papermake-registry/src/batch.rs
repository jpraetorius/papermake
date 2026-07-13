//! Async batch-render jobs.
//!
//! A job renders one template over many inputs in the background; its state is
//! a single JSON document in blob storage (`jobs/{job_id}/job.json`) that the
//! client polls. Rendered PDFs are fetched individually by `render_id` (each
//! item records its own id, so results map back to inputs by index or by a
//! caller-supplied `key`).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// One input in a batch: the data to render plus an optional caller-chosen key
/// that is echoed back on the corresponding result item.
#[derive(Debug, Clone)]
pub struct BatchInput {
    pub data: serde_json::Value,
    pub key: Option<String>,
}

/// Overall job state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Running,
    Completed,
    /// The server running this job was restarted mid-run; it will not resume.
    /// Any items already rendered keep their `render_id`.
    Interrupted,
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

/// The polled job document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchJob {
    pub job_id: String,
    pub reference: String,
    pub status: JobStatus,
    pub total: usize,
    pub done: usize,
    pub failed: usize,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    pub items: Vec<BatchItem>,
}
