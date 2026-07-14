//! S3-compatible storage implementation
//!
//! This module provides an S3-compatible storage implementation of the BlobStorage trait.
//! It works with AWS S3 and S3-compatible object storage.

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use minio::s3::{
    client::{MinioClient, MinioClientBuilder},
    creds::StaticProvider,
    http::BaseUrl,
    segmented_bytes::SegmentedBytes,
    types::{S3Api, ToStream},
};
use std::str::FromStr;
use std::time::Duration;

use crate::{BlobStorage, storage::blob_storage::StorageError};

/// Per-attempt request deadline. The S3 client sets no request timeout, so
/// without this a stalled backend hangs an op indefinitely (the worker then
/// goes silent mid-shard). Overridable via `S3_OP_TIMEOUT_SECONDS`.
const DEFAULT_OP_TIMEOUT_SECS: u64 = 20;
/// Max attempts per op. All our ops are idempotent (content-addressed writes,
/// pure reads/lists), so retrying transient failures is always safe.
/// Overridable via `S3_MAX_ATTEMPTS`.
const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// S3-compatible storage implementation.
#[derive(Clone)]
pub struct S3Storage {
    client: MinioClient,
    bucket: String,
    /// Per-attempt request deadline (see [`DEFAULT_OP_TIMEOUT_SECS`]).
    op_timeout: Duration,
    /// Max attempts per op (see [`DEFAULT_MAX_ATTEMPTS`]).
    max_attempts: u32,
}

impl S3Storage {
    /// Create a new S3 storage instance
    pub fn new(client: MinioClient, bucket: impl Into<String>) -> Self {
        Self {
            client,
            bucket: bucket.into(),
            op_timeout: Duration::from_secs(DEFAULT_OP_TIMEOUT_SECS),
            max_attempts: DEFAULT_MAX_ATTEMPTS,
        }
    }

    /// Create S3 storage from environment variables
    ///
    /// Expects:
    /// - S3_ACCESS_KEY_ID
    /// - S3_SECRET_ACCESS_KEY
    /// - S3_ENDPOINT_URL (for S3-compatible services)
    /// - S3_BUCKET
    /// - S3_REGION (optional)
    pub fn from_env() -> Result<Self, StorageError> {
        let bucket = std::env::var("S3_BUCKET").map_err(|_| {
            StorageError::Backend("S3_BUCKET environment variable not set".to_string())
        })?;

        let access_key = std::env::var("S3_ACCESS_KEY_ID").map_err(|_| {
            StorageError::Backend("S3_ACCESS_KEY_ID environment variable not set".to_string())
        })?;

        let secret_key = std::env::var("S3_SECRET_ACCESS_KEY").map_err(|_| {
            StorageError::Backend("S3_SECRET_ACCESS_KEY environment variable not set".to_string())
        })?;

        let endpoint_url = std::env::var("S3_ENDPOINT_URL").map_err(|_| {
            StorageError::Backend("S3_ENDPOINT_URL environment variable not set".to_string())
        })?;

        // Create base URL for endpoint
        let base_url = BaseUrl::from_str(&endpoint_url)
            .map_err(|e| StorageError::Backend(format!("Invalid S3_ENDPOINT_URL: {}", e)))?;

        // Create credentials provider
        let creds_provider = StaticProvider::new(&access_key, &secret_key, None);

        // Create client
        let client = MinioClientBuilder::new(base_url)
            .provider(Some(creds_provider))
            .build()
            .map_err(|e| StorageError::Backend(format!("Failed to create S3 client: {}", e)))?;

        let mut storage = Self::new(client, bucket);
        if let Some(secs) = std::env::var("S3_OP_TIMEOUT_SECONDS")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            storage.op_timeout = Duration::from_secs(secs);
        }
        if let Some(n) = std::env::var("S3_MAX_ATTEMPTS")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            storage.max_attempts = n;
        }

        Ok(storage)
    }

    /// Run an idempotent S3 op with a per-attempt timeout and bounded
    /// retry+backoff. Without this a stalled backend hangs the op forever and
    /// the caller (e.g. the render worker mid-shard) goes silent; here each
    /// attempt is deadlined and every retry is logged, so trouble is loud and
    /// transient blips self-heal. Terminal errors (NotFound, validation) are
    /// returned immediately — only transient failures are retried.
    async fn with_retry<T, F, Fut>(&self, op: &str, target: &str, f: F) -> Result<T, String>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, String>>,
    {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match tokio::time::timeout(self.op_timeout, f()).await {
                Ok(Ok(v)) => return Ok(v),
                Ok(Err(msg)) => {
                    if attempt >= self.max_attempts || !is_retryable_msg(&msg) {
                        return Err(msg);
                    }
                    tracing::warn!(op, target, attempt, error = %msg, "S3 op failed; retrying");
                }
                Err(_elapsed) => {
                    let secs = self.op_timeout.as_secs();
                    if attempt >= self.max_attempts {
                        tracing::warn!(
                            op,
                            target,
                            attempt,
                            timeout_s = secs,
                            "S3 op timed out; giving up"
                        );
                        return Err(format!("timed out after {secs}s"));
                    }
                    tracing::warn!(
                        op,
                        target,
                        attempt,
                        timeout_s = secs,
                        "S3 op timed out; retrying"
                    );
                }
            }
            let backoff = Duration::from_millis(100u64.saturating_mul(1u64 << attempt.min(5)));
            tokio::time::sleep(backoff).await;
        }
    }

    /// Ensure bucket exists (create if it doesn't)
    pub async fn ensure_bucket(&self) -> Result<(), StorageError> {
        self.with_retry("ensure_bucket", &self.bucket, || async {
            if self.bucket_exists_raw().await? {
                return Ok(());
            }

            match self.create_bucket_raw().await {
                Ok(()) => {
                    tracing::info!(bucket = %self.bucket, "created S3 bucket");
                    Ok(())
                }
                Err(msg) if is_bucket_already_exists_msg(&msg) => {
                    // Another process may have created it between the exists
                    // check and our create call. Confirm we can see it before
                    // treating the race as success.
                    if self.bucket_exists_raw().await? {
                        Ok(())
                    } else {
                        Err(msg)
                    }
                }
                Err(msg) => Err(msg),
            }
        })
        .await
        .map_err(|e| {
            StorageError::Backend(format!("Failed to ensure bucket '{}': {}", self.bucket, e))
        })
    }

    /// Ensure the bucket exists, retrying for a bounded startup window while the
    /// object store container is still coming up.
    pub async fn wait_for_bucket(
        &self,
        max_attempts: u32,
        retry_delay: Duration,
    ) -> Result<(), StorageError> {
        let max_attempts = max_attempts.max(1);
        for attempt in 1..=max_attempts {
            match self.ensure_bucket().await {
                Ok(()) => return Ok(()),
                Err(e) if attempt == max_attempts => return Err(e),
                Err(e) => {
                    tracing::warn!(
                        bucket = %self.bucket,
                        attempt,
                        max_attempts,
                        retry_delay_s = retry_delay.as_secs(),
                        error = %e,
                        "S3 bucket not ready; retrying",
                    );
                    tokio::time::sleep(retry_delay).await;
                }
            }
        }
        unreachable!("max_attempts is clamped to at least one")
    }

    async fn bucket_exists_raw(&self) -> Result<bool, String> {
        self.client
            .bucket_exists(&self.bucket)
            .map_err(|e| e.to_string())?
            .build()
            .send()
            .await
            .map(|response| response.exists())
            .map_err(|e| e.to_string())
    }

    async fn create_bucket_raw(&self) -> Result<(), String> {
        self.client
            .create_bucket(&self.bucket)
            .map_err(|e| e.to_string())?
            .build()
            .send()
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Validate S3 key format
    fn validate_key(&self, key: &str) -> Result<(), StorageError> {
        if key.is_empty() || key.len() > 1024 {
            return Err(StorageError::InvalidKey(
                "Key must be between 1 and 1024 characters".into(),
            ));
        }

        if key.starts_with('/') || key.ends_with('/') {
            return Err(StorageError::InvalidKey(
                "Key cannot start or end with '/'".into(),
            ));
        }

        Ok(())
    }

    /// List files with a given prefix
    pub async fn list_files(&self, prefix: &str) -> Result<Vec<String>, StorageError> {
        // The whole paginated listing is one retried attempt: a stall on any
        // page (the failure mode we see under load) retries the list from the
        // start. Listing is idempotent, so that's safe.
        self.with_retry("list", prefix, || async {
            let mut keys = Vec::new();
            let mut stream = self
                .client
                .list_objects(&self.bucket)
                .map_err(|e| e.to_string())?
                .prefix(Some(prefix.to_string()))
                .recursive(true)
                .build()
                .to_stream()
                .await;
            while let Some(result) = stream.next().await {
                let response = result.map_err(|e| e.to_string())?;
                for entry in response.contents {
                    keys.push(entry.name);
                }
            }
            Ok(keys)
        })
        .await
        .map_err(|e| {
            StorageError::Backend(format!(
                "Failed to list files with prefix '{}': {}",
                prefix, e
            ))
        })
    }
}

/// Whether an error message is worth retrying: transient network/backend
/// failures (connect/timeout/IO, and 5xx/internalerror/slowdown responses).
/// Terminal client errors (NotFound, validation) are never retried, so a
/// genuinely missing object (e.g. a not-yet-written `summary.json`) returns
/// immediately instead of burning the full backoff.
fn is_retryable_msg(msg: &str) -> bool {
    let s = msg.to_ascii_lowercase();
    if is_not_found_msg(msg) {
        return false;
    }
    s.contains("internalerror")
        || s.contains("io error")
        || s.contains("timeout")
        || s.contains("timed out")
        || s.contains("slowdown")
        || s.contains("connect")
        || s.contains("503")
        || s.contains("500")
}

fn is_not_found_msg(msg: &str) -> bool {
    let s = msg.to_ascii_lowercase();
    s.contains("nosuchkey")
        || s.contains("no such key")
        || s.contains("notfound")
        || s.contains("not found")
        || s.contains("404")
}

fn is_bucket_already_exists_msg(msg: &str) -> bool {
    let s = msg.to_ascii_lowercase();
    s.contains("bucketalreadyownedbyyou")
        || s.contains("bucketalreadyexists")
        || s.contains("bucket already exists")
}

#[async_trait]
impl BlobStorage for S3Storage {
    async fn put(&self, key: &str, data: Vec<u8>) -> Result<(), StorageError> {
        self.validate_key(key)?;

        self.with_retry("put", key, || async {
            let bytes = SegmentedBytes::from(Bytes::from(data.clone()));
            self.client
                .put_object(&self.bucket, key, bytes)
                .map_err(|e| e.to_string())?
                .build()
                .send()
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| StorageError::Backend(format!("Failed to put file '{}': {}", key, e)))
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, StorageError> {
        let started = std::time::Instant::now();
        self.validate_key(key)?;

        tracing::debug!(bucket = %self.bucket, key = %key, "s3 get request sending");

        // Send + read content within one retried attempt (both are network I/O
        // that can stall). NotFound is not retryable, so a genuinely missing
        // object returns immediately.
        let bytes = self
            .with_retry("get", key, || async {
                let response = self
                    .client
                    .get_object(&self.bucket, key)
                    .map_err(|e| e.to_string())?
                    .build()
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                let content = response
                    .content()
                    .map_err(|e| e.to_string())?
                    .to_segmented_bytes()
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(content.to_bytes().to_vec())
            })
            .await
            .map_err(|e| {
                if is_not_found_msg(&e) {
                    StorageError::NotFound(key.to_string())
                } else {
                    StorageError::Backend(format!("Failed to get file '{}': {}", key, e))
                }
            })?;

        tracing::debug!(
            bucket = %self.bucket,
            key = %key,
            bytes = bytes.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "s3 get content read",
        );

        Ok(bytes)
    }

    async fn exists(&self, key: &str) -> Result<bool, StorageError> {
        self.validate_key(key)?;

        match self
            .with_retry("exists", key, || async {
                self.client
                    .stat_object(&self.bucket, key)
                    .map_err(|e| e.to_string())?
                    .build()
                    .send()
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            })
            .await
        {
            Ok(()) => Ok(true),
            Err(e) => {
                // A missing object is the expected "false", not an error.
                if is_not_found_msg(&e) {
                    Ok(false)
                } else {
                    Err(StorageError::Backend(format!(
                        "Failed to check existence of file '{}': {}",
                        key, e
                    )))
                }
            }
        }
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.validate_key(key)?;

        self.with_retry("delete", key, || async {
            self.client
                .delete_object(&self.bucket, key)
                .map_err(|e| e.to_string())?
                .build()
                .send()
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| StorageError::Backend(format!("Failed to delete file '{}': {}", key, e)))
    }

    async fn list_keys(&self, prefix: &str) -> Result<Vec<String>, StorageError> {
        self.list_files(prefix).await
    }
}

/// Utility functions for generating S3 keys for different types of content
impl S3Storage {
    /// Generate key for content-addressable blob storage
    pub fn blob_key(hash: &str) -> String {
        format!("blobs/sha256/{}", hash)
    }

    /// Generate key for manifest storage
    pub fn manifest_key(hash: &str) -> String {
        format!("manifests/sha256/{}", hash)
    }

    /// Generate key for mutable reference storage
    pub fn ref_key(namespace: &str, tag: &str) -> String {
        format!("refs/{}/{}", namespace, tag)
    }

    /// Generate key prefix for listing templates in a namespace
    pub fn namespace_prefix(namespace: &str) -> String {
        format!("refs/{}/", namespace)
    }

    /// Generate key prefix for listing all references
    pub fn refs_prefix() -> String {
        "refs/".to_string()
    }

    /// Generate key prefix for listing all blobs
    pub fn blobs_prefix() -> String {
        "blobs/".to_string()
    }

    /// Generate key prefix for listing all manifests
    pub fn manifests_prefix() -> String {
        "manifests/".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_generation() {
        assert_eq!(S3Storage::blob_key("abc123"), "blobs/sha256/abc123");

        assert_eq!(S3Storage::manifest_key("def456"), "manifests/sha256/def456");

        assert_eq!(
            S3Storage::ref_key("john/invoice", "latest"),
            "refs/john/invoice/latest"
        );

        assert_eq!(
            S3Storage::ref_key("invoice", "v1.0.0"),
            "refs/invoice/v1.0.0"
        );
    }

    #[test]
    fn test_prefix_generation() {
        assert_eq!(
            S3Storage::namespace_prefix("john/invoice"),
            "refs/john/invoice/"
        );

        assert_eq!(S3Storage::refs_prefix(), "refs/");
        assert_eq!(S3Storage::blobs_prefix(), "blobs/");
        assert_eq!(S3Storage::manifests_prefix(), "manifests/");
    }

    #[test]
    fn test_key_validation() {
        let client = MinioClientBuilder::new(BaseUrl::from_str("http://localhost:9000").unwrap())
            .build()
            .unwrap();
        let storage = S3Storage::new(client, "test-bucket");

        // Valid keys
        assert!(storage.validate_key("valid/key.txt").is_ok());
        assert!(storage.validate_key("blobs/sha256/abc123").is_ok());
        assert!(storage.validate_key("refs/john/invoice/latest").is_ok());

        // Invalid keys
        assert!(storage.validate_key("").is_err());
        assert!(storage.validate_key("/starts-with-slash").is_err());
        assert!(storage.validate_key("ends-with-slash/").is_err());
        assert!(storage.validate_key(&"x".repeat(1025)).is_err());
    }

    #[test]
    fn test_s3_retry_classification() {
        assert!(is_retryable_msg("InternalError: backend overloaded"));
        assert!(is_retryable_msg("503 SlowDown"));
        assert!(is_retryable_msg("error trying to connect"));
        assert!(!is_retryable_msg("NoSuchKey: object not found"));
        assert!(!is_retryable_msg("404 Not Found"));
    }

    #[test]
    fn test_bucket_already_exists_classification() {
        assert!(is_bucket_already_exists_msg("BucketAlreadyOwnedByYou"));
        assert!(is_bucket_already_exists_msg("bucket already exists"));
        assert!(!is_bucket_already_exists_msg("AccessDenied"));
    }

    #[tokio::test]
    async fn test_s3_storage_from_env_missing_vars() {
        // Clear environment variables to test error handling
        unsafe {
            std::env::remove_var("S3_BUCKET");
            std::env::remove_var("S3_ACCESS_KEY_ID");
            std::env::remove_var("S3_SECRET_ACCESS_KEY");
            std::env::remove_var("S3_ENDPOINT_URL");
        }

        let result = S3Storage::from_env();
        assert!(result.is_err());

        match result {
            Err(StorageError::Backend(msg)) => {
                assert!(msg.contains("S3_BUCKET"));
            }
            _ => panic!("Expected Backend error for missing S3_BUCKET"),
        }
    }
}
