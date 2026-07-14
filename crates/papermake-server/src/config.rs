//! Server configuration management

use crate::error::{ApiError, Result};
use serde::{Deserialize, Serialize};

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

    /// CORS allowed origins
    pub cors_origins: Vec<String>,

    /// Whether to enable debug logging
    pub debug: bool,

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
            cors_origins: std::env::var("CORS_ORIGINS")
                .unwrap_or_else(|_| "*".to_string())
                .split(',')
                .map(|s| s.trim().to_string())
                .collect(),
            debug: std::env::var("DEBUG")
                .map(|s| s.to_lowercase() == "true")
                .unwrap_or(false),
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
            cors_origins: vec!["*".to_string()],
            debug: false,
            instance_id: None,
            flush_interval_seconds: 30,
            flush_max_records: 1000,
            render_retention_days: 30,
            shard_size: 500,
        }
    }
}
