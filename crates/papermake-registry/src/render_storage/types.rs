use serde::{Deserialize, Serialize};
use time::{Date, OffsetDateTime};
use uuid::Uuid;

/// Record of a template render operation
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RenderRecord {
    /// UUIDv7 for time-sortable, distributed-friendly IDs
    pub render_id: String,
    /// Timestamp when the render was initiated
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    /// Template reference used for rendering (e.g., "invoice:latest")
    pub template_ref: String,
    /// Template name extracted from reference
    pub template_name: String,
    /// Template tag extracted from reference
    pub template_tag: String,
    /// SHA-256 hash of the template manifest
    pub manifest_hash: String,
    /// SHA-256 hash of the input data
    pub data_hash: String,
    /// SHA-256 hash of the generated PDF
    pub pdf_hash: String,
    /// Whether the render was successful
    pub success: bool,
    /// Render duration in milliseconds
    pub duration_ms: u32,
    /// Size of the generated PDF in bytes
    pub pdf_size_bytes: u32,
    /// Error message if render failed
    pub error: Option<String>,
    /// Date this render's outputs expire (for audit/visibility). `None` means
    /// "keep forever" — no expiry-index entry is written for it.
    #[serde(default)]
    pub expiry_date: Option<Date>,
}

impl RenderRecord {
    /// Create a new successful render record
    #[allow(clippy::too_many_arguments)]
    pub fn success(
        template_ref: String,
        template_name: String,
        template_tag: String,
        manifest_hash: String,
        data_hash: String,
        pdf_hash: String,
        duration_ms: u32,
        pdf_size_bytes: u32,
    ) -> Self {
        Self {
            render_id: Uuid::now_v7().to_string(),
            timestamp: OffsetDateTime::now_utc(),
            template_ref,
            template_name,
            template_tag,
            manifest_hash,
            data_hash,
            pdf_hash,
            success: true,
            duration_ms,
            pdf_size_bytes,
            error: None,
            expiry_date: None,
        }
    }

    /// Create a new failed render record
    pub fn failure(
        template_ref: String,
        template_name: String,
        template_tag: String,
        manifest_hash: String,
        data_hash: String,
        error: String,
        duration_ms: u32,
    ) -> Self {
        Self {
            render_id: Uuid::now_v7().to_string(),
            timestamp: OffsetDateTime::now_utc(),
            template_ref,
            template_name,
            template_tag,
            manifest_hash,
            data_hash,
            pdf_hash: String::new(),
            success: false,
            duration_ms,
            pdf_size_bytes: 0,
            error: Some(error),
            expiry_date: None,
        }
    }
}

/// Analytics data point for render volume over time
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct VolumePoint {
    pub date: Date,
    /// Total renders on this day (success + failure).
    pub renders: u64,
    /// Subset of `renders` that failed.
    #[serde(default)]
    pub failures: u64,
}

/// Analytics data for template render statistics
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TemplateStats {
    pub template_name: String,
    pub total_renders: u64,
}

/// Analytics data point for render duration over time (successful renders).
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DurationPoint {
    pub date: Date,
    pub avg_duration_ms: f64,
    /// 90th-percentile duration on this day.
    #[serde(default)]
    pub p90_duration_ms: u32,
    /// 95th-percentile duration on this day.
    #[serde(default)]
    pub p95_duration_ms: u32,
    /// 99th-percentile duration on this day.
    #[serde(default)]
    pub p99_duration_ms: u32,
}

/// Query types for analytics
#[derive(Debug, Clone)]
pub enum AnalyticsQuery {
    VolumeOverTime { days: u32 },
    TemplateStats,
    DurationOverTime { days: u32 },
}

/// Result types for analytics queries
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub enum AnalyticsResult {
    Volume(Vec<VolumePoint>),
    Templates(Vec<TemplateStats>),
    Duration(Vec<DurationPoint>),
}

/// Error types for render storage operations
#[derive(Debug, thiserror::Error)]
pub enum RenderStorageError {
    #[error("Database connection error: {0}")]
    Connection(String),

    #[error("Query execution error: {0}")]
    Query(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Render record not found: {0}")]
    NotFound(String),

    #[error("Render failed, no PDF available: {0}")]
    RenderFailed(String),

    #[error("Invalid query parameters: {0}")]
    InvalidQuery(String),
}
