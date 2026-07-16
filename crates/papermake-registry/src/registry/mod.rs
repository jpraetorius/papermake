use papermake::{Font, RenderOptions};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use time;
use tokio::sync::Semaphore;

/// Process-global cache of parsed template fonts, keyed by the font blob's
/// content hash (immutable) so identical fonts are parsed once and reused
/// across renders and templates. Soft-capped to bound memory.
static TEMPLATE_FONT_CACHE: LazyLock<Mutex<HashMap<String, Arc<Vec<Font>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Max distinct font blobs to keep parsed in memory before clearing the cache.
const TEMPLATE_FONT_CACHE_CAP: usize = 512;

/// Max distinct immutable blobs/manifests to keep cached before clearing.
const BLOB_CACHE_CAP: usize = 1024;

/// Max distinct `refs/<name>/<tag>` entries to keep cached before clearing.
const REF_CACHE_CAP: usize = 4096;

/// How long a resolved tag → manifest-hash mapping stays cached. Tags are
/// mutable, so this is deliberately short: a republish on another instance
/// becomes visible within this window; a republish on this instance updates the
/// entry immediately.
const REF_CACHE_TTL: Duration = Duration::from_secs(5);

/// Whether a bundle path is a font file Typst can use (TTF/OTF/TTC).
fn is_font_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".ttf") || lower.ends_with(".otf") || lower.ends_with(".ttc")
}

use crate::{
    address::ContentAddress,
    batch::{BatchInput, BatchItem, BatchJob, ItemStatus, JobView, Shard, ShardStatus},
    bundle::{TemplateBundle, TemplateInfo},
    error::{RegistryError, StorageError},
    manifest::Manifest,
    reference::Reference,
    render_storage::{
        AnalyticsQuery, AnalyticsResult, RenderRecord, RenderStorage, RenderStorageError, layout,
    },
    storage::BlobStorage,
};

/// Prepared, reusable context for a batch: everything resolved once so each
/// input only swaps data on the warm world.
pub(crate) struct BatchCtx {
    reference: String,
    template_name: String,
    template_tag: String,
    manifest_hash: String,
    retain_days: u32,
    entrypoint_content: String,
    file_system: Arc<dyn papermake::RenderFileSystem>,
    /// Template-bundled font faces, kept so a warm world can be rebuilt after a
    /// render error without re-hydrating the bundle. `Font` is `Arc`-backed, so
    /// cloning is cheap.
    fonts: Vec<Font>,
    /// PDF export options applied to every item in the batch.
    options: RenderOptions,
}

impl BatchCtx {
    /// Build a fresh warm Typst world for this batch, registering the template's
    /// bundled fonts. Used for the first item and to rebuild the world after a
    /// render error consumes it.
    fn build_world(&self) -> papermake::PapermakeWorld {
        papermake::PapermakeWorld::with_file_system_and_fonts(
            self.entrypoint_content.clone(),
            "{}".to_string(),
            self.file_system.clone(),
            self.fonts.clone(),
        )
    }
}

/// A resolved template ready to render: its manifest hash + parsed manifest,
/// the entrypoint source, and the hydrated file system and bundled fonts.
pub(crate) struct LoadedTemplate {
    manifest_hash: String,
    manifest: Manifest,
    entrypoint_content: String,
    file_system: Arc<dyn papermake::RenderFileSystem>,
    fonts: Vec<Font>,
}

/// Stable, order-independent tag describing a render's PDF export options, used
/// to discriminate content-addressed render ids. Empty for plain PDF 1.7 (so
/// plain renders keep their historical ids); non-empty (e.g. `"a-2b"`) yields a
/// distinct id so PDF/A output never collides with the plain render of the same
/// `(template, data)`.
fn render_options_tag(options: &RenderOptions) -> String {
    if options.pdf_standards.is_empty() {
        return String::new();
    }
    let mut names: Vec<&'static str> = options.pdf_standards.iter().map(|s| s.as_str()).collect();
    names.sort_unstable();
    names.join("+")
}

/// Default global output retention when neither the render nor the template
/// specifies one. Overridable per-registry via [`Registry::with_retention_days`]
/// (the server sets it from `RENDER_RETENTION_DAYS`).
pub const DEFAULT_RENDER_RETENTION_DAYS: u32 = 30;

/// Default maximum number of concurrent Typst render tasks.
pub const DEFAULT_MAX_CONCURRENT_RENDERS: usize = 10;

/// Default wall-clock timeout for a render, including queue wait.
///
/// Kept deliberately short: a timed-out render cannot be cancelled (Typst
/// compilation runs on a blocking thread that holds its render slot until it
/// finishes on its own), so a long timeout lets a slow or non-terminating
/// template tie up a slot for that whole window. See [`Registry::track_leaked_render`].
pub const DEFAULT_RENDER_TIMEOUT_SECONDS: u64 = 60;

/// Default number of items per batch shard (the unit of work a worker claims).
pub const DEFAULT_SHARD_SIZE: usize = 500;

/// Build the immutable-blob cache (bounded LRU over content-addressed objects).
fn new_blob_cache() -> moka::future::Cache<String, Arc<Vec<u8>>> {
    moka::future::Cache::new(BLOB_CACHE_CAP as u64)
}

/// Build the ref cache (bounded, short TTL because tags are mutable).
fn new_ref_cache() -> moka::future::Cache<String, String> {
    moka::future::Cache::builder()
        .max_capacity(REF_CACHE_CAP as u64)
        .time_to_live(REF_CACHE_TTL)
        .build()
}

/// Core registry for template publishing and resolution
pub struct Registry<S: BlobStorage, R: RenderStorage> {
    storage: Arc<S>,
    /// Analytics store: renders are staged here for aggregation. Always present
    /// — a registry created without an explicit store gets an in-process
    /// [`MemoryRenderStorage`](crate::render_storage::MemoryRenderStorage).
    render_storage: Arc<R>,
    /// Global default output retention (days); `0` = keep forever.
    default_retention_days: u32,
    /// Limits CPU-bound Typst work running on the blocking thread pool.
    render_semaphore: Arc<Semaphore>,
    /// Number of render slots (`render_semaphore` capacity), retained so the
    /// leaked-slot circuit breaker can tell when the whole pool is stuck.
    max_concurrent_renders: usize,
    /// Count of render tasks that timed out but could not be cancelled and are
    /// still running while holding their render slot. When this reaches
    /// [`Self::max_concurrent_renders`] the pool is exhausted and new renders
    /// are rejected fast instead of queueing until they also time out.
    leaked_renders: Arc<AtomicUsize>,
    /// Wall-clock timeout for acquiring a render slot and waiting for Typst.
    render_timeout: Duration,
    /// Items per shard when enqueuing a batch (the unit of work workers claim).
    batch_shard_size: usize,
    /// Cache of immutable content-addressed objects (`blobs/…`, `manifests/…`)
    /// keyed by storage key. Safe to keep indefinitely because the content at a
    /// given key never changes; size-bounded LRU eviction.
    blob_cache: moka::future::Cache<String, Arc<Vec<u8>>>,
    /// Cache of resolved `refs/<name>/<tag>` → manifest hash. Tags are mutable,
    /// so entries expire after [`REF_CACHE_TTL`] and are updated/removed on
    /// publish/delete.
    ref_cache: moka::future::Cache<String, String>,
}

/// Result of a render operation with tracking
#[derive(Debug, Serialize)]
pub struct RenderResult {
    /// UUIDv7 for the render operation
    pub render_id: String,
    /// Generated PDF bytes
    pub pdf_bytes: Vec<u8>,
    /// SHA-256 hash of the PDF
    pub pdf_hash: String,
    /// Render duration in milliseconds
    pub duration_ms: u32,
}

/// Outcome of deleting a tagged version, including asset garbage collection.
#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct DeleteSummary {
    /// The manifest was still referenced by another tag, so it (and its assets)
    /// were kept.
    pub manifest_kept: bool,
    /// The now-orphaned manifest was deleted.
    pub manifest_deleted: bool,
    /// Number of asset blobs deleted (those no longer referenced anywhere).
    pub blobs_deleted: usize,
}

// Implementation for Registry with blob storage only
impl<S: BlobStorage + 'static, R: RenderStorage> Registry<S, R> {
    /// Create a new registry with the given storage backend
    pub fn new(storage: S, render_storage: R) -> Self {
        Self {
            storage: Arc::new(storage),
            render_storage: Arc::new(render_storage),
            default_retention_days: DEFAULT_RENDER_RETENTION_DAYS,
            render_semaphore: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_RENDERS)),
            max_concurrent_renders: DEFAULT_MAX_CONCURRENT_RENDERS,
            leaked_renders: Arc::new(AtomicUsize::new(0)),
            render_timeout: Duration::from_secs(DEFAULT_RENDER_TIMEOUT_SECONDS),
            batch_shard_size: DEFAULT_SHARD_SIZE,
            blob_cache: new_blob_cache(),
            ref_cache: new_ref_cache(),
        }
    }
}

// Implementation for Registry with both blob and render storage
impl<S: BlobStorage + 'static, R: RenderStorage + 'static> Registry<S, R> {
    /// Create a new registry with both blob and render storage
    pub fn new_with_render_storage(storage: S, render_storage: R) -> Self {
        Self {
            storage: Arc::new(storage),
            render_storage: Arc::new(render_storage),
            default_retention_days: DEFAULT_RENDER_RETENTION_DAYS,
            render_semaphore: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_RENDERS)),
            max_concurrent_renders: DEFAULT_MAX_CONCURRENT_RENDERS,
            leaked_renders: Arc::new(AtomicUsize::new(0)),
            render_timeout: Duration::from_secs(DEFAULT_RENDER_TIMEOUT_SECONDS),
            batch_shard_size: DEFAULT_SHARD_SIZE,
            blob_cache: new_blob_cache(),
            ref_cache: new_ref_cache(),
        }
    }

    /// Create a new registry with only blob storage (no render tracking)
    pub fn new_blob_only(storage: S) -> Registry<S, crate::render_storage::MemoryRenderStorage> {
        Registry {
            storage: Arc::new(storage),
            render_storage: Arc::new(crate::render_storage::MemoryRenderStorage::new()),
            default_retention_days: DEFAULT_RENDER_RETENTION_DAYS,
            render_semaphore: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_RENDERS)),
            max_concurrent_renders: DEFAULT_MAX_CONCURRENT_RENDERS,
            leaked_renders: Arc::new(AtomicUsize::new(0)),
            render_timeout: Duration::from_secs(DEFAULT_RENDER_TIMEOUT_SECONDS),
            batch_shard_size: DEFAULT_SHARD_SIZE,
            blob_cache: new_blob_cache(),
            ref_cache: new_ref_cache(),
        }
    }

    /// Set the global default output retention (days); `0` = keep forever.
    pub fn with_retention_days(mut self, days: u32) -> Self {
        self.default_retention_days = days;
        self
    }

    /// Set the batch shard size (items per shard). Larger shards = fewer
    /// descriptors but coarser parallelism/reclaim granularity.
    pub fn with_shard_size(mut self, shard_size: usize) -> Self {
        self.batch_shard_size = shard_size.max(1);
        self
    }

    /// Set render concurrency and timeout limits. The timeout covers waiting
    /// for a render slot plus waiting for the blocking Typst task to finish.
    pub fn with_render_limits(
        mut self,
        max_concurrent_renders: usize,
        render_timeout: Duration,
    ) -> Self {
        self.max_concurrent_renders = max_concurrent_renders.max(1);
        self.render_semaphore = Arc::new(Semaphore::new(self.max_concurrent_renders));
        self.render_timeout = if render_timeout.is_zero() {
            Duration::from_secs(1)
        } else {
            render_timeout
        };
        self
    }

    /// Shared handle to the render storage. Lets the server run the background
    /// flush loop (and flush-on-shutdown) against the same buffer that
    /// `render_and_store` stages into.
    pub fn render_storage(&self) -> Arc<R> {
        self.render_storage.clone()
    }
}

// Implementation for backward compatibility with existing tests
impl<S: BlobStorage + 'static> Registry<S, crate::render_storage::MemoryRenderStorage> {
    /// Create a new registry with only blob storage (backward compatibility)
    pub fn new_storage_only(storage: S) -> Self {
        Self {
            storage: Arc::new(storage),
            render_storage: Arc::new(crate::render_storage::MemoryRenderStorage::new()),
            default_retention_days: DEFAULT_RENDER_RETENTION_DAYS,
            render_semaphore: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_RENDERS)),
            max_concurrent_renders: DEFAULT_MAX_CONCURRENT_RENDERS,
            leaked_renders: Arc::new(AtomicUsize::new(0)),
            render_timeout: Duration::from_secs(DEFAULT_RENDER_TIMEOUT_SECONDS),
            batch_shard_size: DEFAULT_SHARD_SIZE,
            blob_cache: new_blob_cache(),
            ref_cache: new_ref_cache(),
        }
    }
}

mod batch;
mod history;
mod publish;
mod render;
mod templates;

#[cfg(test)]
mod tests;
