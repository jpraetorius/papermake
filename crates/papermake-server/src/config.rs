//! Server configuration management

use crate::error::{ApiError, Result};
use serde::{Deserialize, Serialize};

/// Default maximum HTTP request body size: 50 MiB.
pub const DEFAULT_REQUEST_BODY_LIMIT_BYTES: usize = 50 * 1024 * 1024;

/// Server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Host to bind to
    pub host: String,

    /// Port to bind to
    pub port: u16,

    /// Maximum number of concurrent render jobs
    pub max_concurrent_renders: usize,

    /// Timeout for render jobs in seconds
    pub render_timeout_seconds: u64,

    /// Maximum accepted HTTP request body size in bytes.
    pub request_body_limit_bytes: usize,

    /// Stable identifier for this render instance (used in flushed S3 keys).
    pub instance_id: Option<String>,

    /// Interval between background analytics flushes, in seconds.
    pub flush_interval_seconds: u64,

    /// Flush eagerly once the buffer reaches this many records.
    pub flush_max_records: usize,

    /// Global default output retention in days (`0` = keep forever).
    pub render_retention_days: u32,

    /// Items per batch shard (the unit of work a worker claims).
    pub shard_size: usize,
}

impl ServerConfig {
    /// Load configuration from environment variables
    pub fn from_env() -> Result<Self> {
        let request_body_limit_bytes = std::env::var("REQUEST_BODY_LIMIT_BYTES")
            .unwrap_or_else(|_| DEFAULT_REQUEST_BODY_LIMIT_BYTES.to_string())
            .parse()
            .map_err(|_| ApiError::Config("Invalid REQUEST_BODY_LIMIT_BYTES value".to_string()))?;

        if request_body_limit_bytes == 0 {
            return Err(ApiError::Config(
                "REQUEST_BODY_LIMIT_BYTES must be greater than 0".to_string(),
            ));
        }

        Ok(Self {
            host: std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            port: std::env::var("PORT")
                .unwrap_or_else(|_| "3000".to_string())
                .parse()
                .map_err(|_| ApiError::Config("Invalid PORT value".to_string()))?,
            max_concurrent_renders: std::env::var("MAX_CONCURRENT_RENDERS")
                .unwrap_or_else(|_| "10".to_string())
                .parse()
                .map_err(|_| {
                    ApiError::Config("Invalid MAX_CONCURRENT_RENDERS value".to_string())
                })?,
            render_timeout_seconds: std::env::var("RENDER_TIMEOUT_SECONDS")
                .unwrap_or_else(|_| "300".to_string()) // 5 minutes default
                .parse()
                .map_err(|_| {
                    ApiError::Config("Invalid RENDER_TIMEOUT_SECONDS value".to_string())
                })?,
            request_body_limit_bytes,
            instance_id: std::env::var("PAPERMAKE_INSTANCE_ID").ok(),
            flush_interval_seconds: std::env::var("FLUSH_INTERVAL_SECONDS")
                .unwrap_or_else(|_| "30".to_string())
                .parse()
                .map_err(|_| {
                    ApiError::Config("Invalid FLUSH_INTERVAL_SECONDS value".to_string())
                })?,
            flush_max_records: std::env::var("FLUSH_MAX_RECORDS")
                .unwrap_or_else(|_| "1000".to_string())
                .parse()
                .map_err(|_| ApiError::Config("Invalid FLUSH_MAX_RECORDS value".to_string()))?,
            render_retention_days: std::env::var("RENDER_RETENTION_DAYS")
                .unwrap_or_else(|_| "30".to_string())
                .parse()
                .map_err(|_| ApiError::Config("Invalid RENDER_RETENTION_DAYS value".to_string()))?,
            shard_size: std::env::var("SHARD_SIZE")
                .unwrap_or_else(|_| "500".to_string())
                .parse()
                .map_err(|_| ApiError::Config("Invalid SHARD_SIZE value".to_string()))?,
        })
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 3000,
            max_concurrent_renders: 10,
            render_timeout_seconds: 300,
            request_body_limit_bytes: DEFAULT_REQUEST_BODY_LIMIT_BYTES,
            instance_id: None,
            flush_interval_seconds: 30,
            flush_max_records: 1000,
            render_retention_days: 30,
            shard_size: 500,
        }
    }
}
