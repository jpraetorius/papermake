//! Storage abstraction for registry data

pub mod blob_storage;

// Re-export for convenience
pub use blob_storage::BlobStorage;

// S3 implementation
#[cfg(feature = "s3")]
pub mod s3_storage;
