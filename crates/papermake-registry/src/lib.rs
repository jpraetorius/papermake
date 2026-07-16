//! # Papermake Registry
//!
//! A content-addressed template registry for papermake over S3-compatible
//! object storage, plus the render/analytics/retention machinery built on it:
//! - Template files and manifests are hashed and stored immutably; identical
//!   content is deduplicated.
//! - Tags (`refs/<name>/<tag>`) are mutable pointers to an immutable manifest
//!   hash, so a tag names an exact template version.
//! - Synchronous rendering (with a bounded pool) and sharded, multi-worker
//!   batch rendering, both writing content-addressed render outputs.
//! - Eventually-consistent render analytics and time-based output retention.
//!
//! ## Core Concepts
//!
//! - **Blobs & manifests** are immutable, content-addressed, and deduplicated.
//! - **Tags** are the only mutable pointers; publishing moves a tag to a new
//!   manifest hash.
//! - **Renders** are content-addressed from `(manifest hash, data, options)`, so
//!   re-rendering the same request is idempotent.
//!
//! ## Example Usage
//!
//! ```rust,no_run
//! use papermake_registry::*;
//! use papermake_registry::bundle::{TemplateBundle, TemplateMetadata};
//! use papermake_registry::storage::blob_storage::MemoryStorage;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Create a registry with memory storage
//! let storage = MemoryStorage::new();
//! let registry = Registry::new_storage_only(storage);
//!
//! // Create a template bundle
//! let metadata = TemplateMetadata::new("Invoice Template", "alice@company.com");
//! let main_content = b"= Invoice\nFor: #data.customer_name".to_vec();
//! let bundle = TemplateBundle::new(main_content, metadata);
//!
//! // Publish the template
//! let manifest_hash = registry.publish(
//!     bundle,
//!     "alice/invoice-template",
//!     "latest"
//! ).await?;
//!
//! println!("Published template with manifest hash: {}", manifest_hash);
//!
//! // Resolve the template back
//! let resolved_hash = registry.resolve("alice/invoice-template:latest").await?;
//! assert_eq!(manifest_hash, resolved_hash);
//! # Ok(())
//! # }
//! ```

pub mod address;
pub mod batch;
pub mod bundle;
pub mod error;
pub mod manifest;
pub mod reference;
pub mod registry;
pub mod render_storage;
pub mod storage;

pub use bundle::TemplateInfo;
pub use error::RegistryError;
// Re-export the PDF export knobs so server callers don't need a direct
// papermake dependency.
pub use papermake::{PdfStandard, RenderOptions};
pub use registry::Registry;
pub use render_storage::{AnalyticsQuery, AnalyticsResult, RenderRecord, RenderStorage};
pub use storage::BlobStorage;

#[cfg(feature = "s3")]
pub use storage::s3_storage::S3Storage;

pub use render_storage::s3_buffered::S3BufferedRenderStorage;
