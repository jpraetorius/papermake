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

    /// Maximum number of inputs accepted in a single batch submission.
    pub max_batch_inputs: usize,

    /// Maximum serialized size (bytes) of a single batch item's `data`.
    pub max_batch_item_bytes: usize,
}

impl ServerConfig {
    /// Load configuration from environment variables
    pub fn from_env() -> Result<Self> {
        Self::from_env_values(|key| std::env::var(key))
    }

    pub(crate) fn from_env_values(
        mut env: impl FnMut(&str) -> std::result::Result<String, std::env::VarError>,
    ) -> Result<Self> {
        let request_body_limit_bytes = env("REQUEST_BODY_LIMIT_BYTES")
            .unwrap_or_else(|_| DEFAULT_REQUEST_BODY_LIMIT_BYTES.to_string())
            .parse()
            .map_err(|_| ApiError::Config("Invalid REQUEST_BODY_LIMIT_BYTES value".to_string()))?;

        if request_body_limit_bytes == 0 {
            return Err(ApiError::Config(
                "REQUEST_BODY_LIMIT_BYTES must be greater than 0".to_string(),
            ));
        }

        Ok(Self {
            host: env("HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            port: env("PORT")
                .unwrap_or_else(|_| "3000".to_string())
                .parse()
                .map_err(|_| ApiError::Config("Invalid PORT value".to_string()))?,
            max_concurrent_renders: env("MAX_CONCURRENT_RENDERS")
                .unwrap_or_else(|_| "10".to_string())
                .parse()
                .map_err(|_| {
                    ApiError::Config("Invalid MAX_CONCURRENT_RENDERS value".to_string())
                })?,
            render_timeout_seconds: env("RENDER_TIMEOUT_SECONDS")
                .unwrap_or_else(|_| "60".to_string()) // short: timed-out renders can't be cancelled
                .parse()
                .map_err(|_| {
                    ApiError::Config("Invalid RENDER_TIMEOUT_SECONDS value".to_string())
                })?,
            request_body_limit_bytes,
            instance_id: env("PAPERMAKE_INSTANCE_ID").ok(),
            // Clamp to >= 1s: a 0 interval would busy-loop the flush task
            // against S3.
            flush_interval_seconds: env("FLUSH_INTERVAL_SECONDS")
                .unwrap_or_else(|_| "30".to_string())
                .parse::<u64>()
                .map_err(|_| ApiError::Config("Invalid FLUSH_INTERVAL_SECONDS value".to_string()))?
                .max(1),
            flush_max_records: env("FLUSH_MAX_RECORDS")
                .unwrap_or_else(|_| "1000".to_string())
                .parse()
                .map_err(|_| ApiError::Config("Invalid FLUSH_MAX_RECORDS value".to_string()))?,
            render_retention_days: env("RENDER_RETENTION_DAYS")
                .unwrap_or_else(|_| "30".to_string())
                .parse()
                .map_err(|_| ApiError::Config("Invalid RENDER_RETENTION_DAYS value".to_string()))?,
            shard_size: env("SHARD_SIZE")
                .unwrap_or_else(|_| "500".to_string())
                .parse()
                .map_err(|_| ApiError::Config("Invalid SHARD_SIZE value".to_string()))?,
            max_batch_inputs: env("MAX_BATCH_INPUTS")
                .unwrap_or_else(|_| "100000".to_string())
                .parse()
                .map_err(|_| ApiError::Config("Invalid MAX_BATCH_INPUTS value".to_string()))?,
            max_batch_item_bytes: env("MAX_BATCH_ITEM_BYTES")
                .unwrap_or_else(|_| "1048576".to_string()) // 1 MiB
                .parse()
                .map_err(|_| ApiError::Config("Invalid MAX_BATCH_ITEM_BYTES value".to_string()))?,
        })
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 3000,
            max_concurrent_renders: 10,
            render_timeout_seconds: 60,
            request_body_limit_bytes: DEFAULT_REQUEST_BODY_LIMIT_BYTES,
            instance_id: None,
            flush_interval_seconds: 30,
            flush_max_records: 1000,
            render_retention_days: 30,
            shard_size: 500,
            max_batch_inputs: 100_000,
            max_batch_item_bytes: 1024 * 1024,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::env::VarError;

    use super::*;

    fn config_from(pairs: &[(&str, &str)]) -> Result<ServerConfig> {
        ServerConfig::from_env_values(|key| {
            pairs
                .iter()
                .find_map(|(candidate, value)| (*candidate == key).then(|| (*value).to_string()))
                .ok_or(VarError::NotPresent)
        })
    }

    #[test]
    fn from_env_values_uses_documented_defaults_when_values_are_absent() {
        let config = config_from(&[]).unwrap();
        let default = ServerConfig::default();

        assert_eq!(config.host, default.host);
        assert_eq!(config.port, default.port);
        assert_eq!(
            config.max_concurrent_renders,
            default.max_concurrent_renders
        );
        assert_eq!(
            config.render_timeout_seconds,
            default.render_timeout_seconds
        );
        assert_eq!(
            config.request_body_limit_bytes,
            default.request_body_limit_bytes
        );
        assert_eq!(config.instance_id, default.instance_id);
        assert_eq!(
            config.flush_interval_seconds,
            default.flush_interval_seconds
        );
        assert_eq!(config.flush_max_records, default.flush_max_records);
        assert_eq!(config.render_retention_days, default.render_retention_days);
        assert_eq!(config.shard_size, default.shard_size);
    }

    #[test]
    fn from_env_values_applies_all_supported_overrides() {
        let config = config_from(&[
            ("HOST", "127.0.0.1"),
            ("PORT", "8080"),
            ("MAX_CONCURRENT_RENDERS", "3"),
            ("RENDER_TIMEOUT_SECONDS", "9"),
            ("REQUEST_BODY_LIMIT_BYTES", "1024"),
            ("PAPERMAKE_INSTANCE_ID", "server-a"),
            ("FLUSH_INTERVAL_SECONDS", "7"),
            ("FLUSH_MAX_RECORDS", "11"),
            ("RENDER_RETENTION_DAYS", "0"),
            ("SHARD_SIZE", "13"),
            ("MAX_BATCH_INPUTS", "17"),
            ("MAX_BATCH_ITEM_BYTES", "2048"),
        ])
        .unwrap();

        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 8080);
        assert_eq!(config.max_concurrent_renders, 3);
        assert_eq!(config.render_timeout_seconds, 9);
        assert_eq!(config.request_body_limit_bytes, 1024);
        assert_eq!(config.instance_id.as_deref(), Some("server-a"));
        assert_eq!(config.flush_interval_seconds, 7);
        assert_eq!(config.flush_max_records, 11);
        assert_eq!(config.render_retention_days, 0);
        assert_eq!(config.shard_size, 13);
        assert_eq!(config.max_batch_inputs, 17);
        assert_eq!(config.max_batch_item_bytes, 2048);
    }

    #[test]
    fn from_env_values_rejects_invalid_numeric_values() {
        for key in [
            "PORT",
            "MAX_CONCURRENT_RENDERS",
            "RENDER_TIMEOUT_SECONDS",
            "REQUEST_BODY_LIMIT_BYTES",
            "FLUSH_INTERVAL_SECONDS",
            "FLUSH_MAX_RECORDS",
            "RENDER_RETENTION_DAYS",
            "SHARD_SIZE",
        ] {
            let error = config_from(&[(key, "not-a-number")]).unwrap_err();
            assert!(matches!(error, ApiError::Config(_)));
        }
    }

    #[test]
    fn from_env_values_rejects_zero_request_body_limit() {
        let error = config_from(&[("REQUEST_BODY_LIMIT_BYTES", "0")]).unwrap_err();

        assert!(matches!(error, ApiError::Config(_)));
    }

    #[test]
    fn flush_interval_is_clamped_to_at_least_one_second() {
        // A 0 interval would busy-loop the flush task; it is floored to 1.
        let config = config_from(&[("FLUSH_INTERVAL_SECONDS", "0")]).unwrap();
        assert_eq!(config.flush_interval_seconds, 1);
    }
}
