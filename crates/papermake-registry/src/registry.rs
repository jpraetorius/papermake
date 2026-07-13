use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use time;
use tokio::sync::Semaphore;

use crate::{
    address::ContentAddress,
    batch::{BatchInput, BatchItem, BatchJob, ItemStatus, JobStatus},
    bundle::{TemplateBundle, TemplateInfo},
    error::{RegistryError, StorageError},
    manifest::Manifest,
    reference::Reference,
    render_storage::{
        AnalyticsQuery, AnalyticsResult, RenderRecord, RenderStorage, RenderStorageError,
    },
    storage::BlobStorage,
};

/// Prepared, reusable context for a batch: everything resolved once so each
/// input only swaps data on the warm world.
struct BatchCtx {
    reference: String,
    template_name: String,
    template_tag: String,
    manifest_hash: String,
    retain_days: u32,
    entrypoint_content: String,
    file_system: Arc<dyn papermake::RenderFileSystem>,
}

/// Default global output retention when neither the render nor the template
/// specifies one. Overridable per-registry via [`Registry::with_retention_days`]
/// (the server sets it from `RENDER_RETENTION_DAYS`).
pub const DEFAULT_RENDER_RETENTION_DAYS: u32 = 30;

/// Default maximum number of concurrent Typst render tasks.
pub const DEFAULT_MAX_CONCURRENT_RENDERS: usize = 10;

/// Default wall-clock timeout for a render, including queue wait.
pub const DEFAULT_RENDER_TIMEOUT_SECONDS: u64 = 300;

/// Core registry for template publishing and resolution
pub struct Registry<S: BlobStorage, R: RenderStorage> {
    storage: Arc<S>,
    render_storage: Option<Arc<R>>,
    /// Global default output retention (days); `0` = keep forever.
    default_retention_days: u32,
    /// Limits CPU-bound Typst work running on the blocking thread pool.
    render_semaphore: Arc<Semaphore>,
    /// Wall-clock timeout for acquiring a render slot and waiting for Typst.
    render_timeout: Duration,
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
            render_storage: Some(Arc::new(render_storage)),
            default_retention_days: DEFAULT_RENDER_RETENTION_DAYS,
            render_semaphore: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_RENDERS)),
            render_timeout: Duration::from_secs(DEFAULT_RENDER_TIMEOUT_SECONDS),
        }
    }
}

// Implementation for Registry with both blob and render storage
impl<S: BlobStorage + 'static, R: RenderStorage + 'static> Registry<S, R> {
    /// Create a new registry with both blob and render storage
    pub fn new_with_render_storage(storage: S, render_storage: R) -> Self {
        Self {
            storage: Arc::new(storage),
            render_storage: Some(Arc::new(render_storage)),
            default_retention_days: DEFAULT_RENDER_RETENTION_DAYS,
            render_semaphore: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_RENDERS)),
            render_timeout: Duration::from_secs(DEFAULT_RENDER_TIMEOUT_SECONDS),
        }
    }

    /// Create a new registry with only blob storage (no render tracking)
    pub fn new_blob_only(storage: S) -> Registry<S, crate::render_storage::MemoryRenderStorage> {
        Registry {
            storage: Arc::new(storage),
            render_storage: None,
            default_retention_days: DEFAULT_RENDER_RETENTION_DAYS,
            render_semaphore: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_RENDERS)),
            render_timeout: Duration::from_secs(DEFAULT_RENDER_TIMEOUT_SECONDS),
        }
    }

    /// Set the global default output retention (days); `0` = keep forever.
    pub fn with_retention_days(mut self, days: u32) -> Self {
        self.default_retention_days = days;
        self
    }

    /// Set render concurrency and timeout limits. The timeout covers waiting
    /// for a render slot plus waiting for the blocking Typst task to finish.
    pub fn with_render_limits(
        mut self,
        max_concurrent_renders: usize,
        render_timeout: Duration,
    ) -> Self {
        self.render_semaphore = Arc::new(Semaphore::new(max_concurrent_renders.max(1)));
        self.render_timeout = if render_timeout.is_zero() {
            Duration::from_secs(1)
        } else {
            render_timeout
        };
        self
    }

    /// Shared handle to the render storage, if configured. Lets the server run
    /// the background flush loop (and flush-on-shutdown) against the same buffer
    /// that `render_and_store` stages into.
    pub fn render_storage(&self) -> Option<Arc<R>> {
        self.render_storage.clone()
    }
}

// Implementation for backward compatibility with existing tests
impl<S: BlobStorage + 'static> Registry<S, crate::render_storage::MemoryRenderStorage> {
    /// Create a new registry with only blob storage (backward compatibility)
    pub fn new_storage_only(storage: S) -> Self {
        Self {
            storage: Arc::new(storage),
            render_storage: None,
            default_retention_days: DEFAULT_RENDER_RETENTION_DAYS,
            render_semaphore: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_RENDERS)),
            render_timeout: Duration::from_secs(DEFAULT_RENDER_TIMEOUT_SECONDS),
        }
    }
}

// Shared implementation for all registry types
impl<S: BlobStorage + 'static, R: RenderStorage + 'static> Registry<S, R> {
    /// Publish a template bundle to the registry
    ///
    /// This method implements the "store files → create manifest → update refs" workflow:
    /// 1. Validates the template bundle
    /// 2. Stores all files as content-addressed blobs
    /// 3. Creates a manifest mapping file paths to their hashes
    /// 4. Stores the manifest as a content-addressed blob
    /// 5. Updates the reference (tag) to point to the manifest hash
    ///
    /// Returns the manifest hash for content-addressable access
    pub async fn publish(
        &self,
        bundle: TemplateBundle,
        namespace: &str,
        tag: &str,
    ) -> Result<String, RegistryError> {
        // Step 1: Validate the bundle
        bundle.validate().map_err(|e| {
            RegistryError::Template(crate::error::TemplateError::invalid(e.to_string()))
        })?;

        // Step 2: Store individual files as blobs
        let mut file_hashes = BTreeMap::new();

        // Store main.typ
        let main_hash = ContentAddress::hash(bundle.main_typ());
        let main_blob_key = ContentAddress::blob_key(&main_hash);
        self.storage
            .put(&main_blob_key, bundle.main_typ().to_vec())
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;
        file_hashes.insert("main.typ".to_string(), main_hash);

        // Store additional files
        for (file_path, file_content) in bundle.files() {
            let file_hash = ContentAddress::hash(file_content);
            let file_blob_key = ContentAddress::blob_key(&file_hash);
            self.storage
                .put(&file_blob_key, file_content.clone())
                .await
                .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;
            file_hashes.insert(file_path.clone(), file_hash);
        }

        // Step 3: Create manifest
        let manifest = Manifest::new(file_hashes, bundle.metadata().clone()).map_err(|e| {
            RegistryError::ContentAddressing(crate::error::ContentAddressingError::manifest_error(
                e.to_string(),
            ))
        })?;

        // Step 4: Store manifest
        let manifest_bytes = manifest.to_bytes().map_err(|e| {
            RegistryError::ContentAddressing(crate::error::ContentAddressingError::manifest_error(
                e.to_string(),
            ))
        })?;
        let manifest_hash = ContentAddress::hash(&manifest_bytes);
        let manifest_key = ContentAddress::manifest_key(&manifest_hash);
        self.storage
            .put(&manifest_key, manifest_bytes)
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;

        // Step 5: Update reference (tag)
        let ref_key = ContentAddress::ref_key(namespace, tag);
        self.storage
            .put(&ref_key, manifest_hash.as_bytes().to_vec())
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;

        // Return the manifest hash for content-addressable access
        Ok(manifest_hash)
    }

    /// Delete a single tagged version: `refs/{name}/{tag}`.
    ///
    /// Content-addressed assets are garbage-collected, but only what nothing
    /// else references:
    /// - the version's **manifest** is removed unless another tag still points
    ///   to it (identical publishes dedup to the same manifest);
    /// - each **asset blob** is removed only if no *surviving* manifest still
    ///   references it (e.g. a shared `logo.png` is kept).
    ///
    /// Returns a [`DeleteSummary`]. Errors with a not-found template error if
    /// the version doesn't exist.
    ///
    /// Note: this is a mark-sweep over the ref/manifest set; it is not locked
    /// against a concurrent `publish`. Fine at this scale — surface a lock if
    /// heavy concurrent writes are ever expected.
    pub async fn delete_version(
        &self,
        name: &str,
        tag: &str,
    ) -> Result<DeleteSummary, RegistryError> {
        let ref_key = ContentAddress::ref_key(name, tag);

        // Resolve the ref → manifest hash (not-found if the version is missing).
        let manifest_hash_bytes = self.storage.get(&ref_key).await.map_err(|e| match e {
            crate::storage::blob_storage::StorageError::NotFound(_) => RegistryError::Template(
                crate::error::TemplateError::not_found(format!("{}:{}", name, tag)),
            ),
            other => RegistryError::Storage(StorageError::backend(other.to_string())),
        })?;
        let manifest_hash = String::from_utf8(manifest_hash_bytes).map_err(|e| {
            RegistryError::Storage(StorageError::backend(format!(
                "Invalid UTF-8 in stored manifest hash: {}",
                e
            )))
        })?;

        // Drop the tag pointer, then GC anything it orphaned.
        self.storage
            .delete(&ref_key)
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;

        self.gc_orphaned(&manifest_hash).await
    }

    /// Garbage-collect a manifest (and its asset blobs) that a just-deleted ref
    /// may have orphaned — keeping anything still referenced by a surviving tag.
    async fn gc_orphaned(
        &self,
        deleted_manifest_hash: &str,
    ) -> Result<DeleteSummary, RegistryError> {
        use std::collections::HashSet;
        let mut summary = DeleteSummary::default();

        // Manifest hashes still referenced by remaining tags.
        let ref_keys = self
            .storage
            .list_keys("refs/")
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;
        let mut live_manifests: HashSet<String> = HashSet::new();
        for rk in &ref_keys {
            if let Ok(bytes) = self.storage.get(rk).await
                && let Ok(h) = String::from_utf8(bytes)
            {
                live_manifests.insert(h);
            }
        }

        // Still referenced by another tag → keep manifest and all its assets.
        if live_manifests.contains(deleted_manifest_hash) {
            summary.manifest_kept = true;
            return Ok(summary);
        }

        // Enumerate the orphaned manifest's asset blobs (empty if already gone).
        let orphan_key = ContentAddress::manifest_key(deleted_manifest_hash);
        let orphan_blobs: Vec<String> = match self.storage.get(&orphan_key).await {
            Ok(bytes) => Manifest::from_bytes(&bytes)
                .map(|m| m.files.values().cloned().collect())
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };

        // Blobs still referenced by any surviving manifest.
        let mut live_blobs: HashSet<String> = HashSet::new();
        for mh in &live_manifests {
            if let Ok(bytes) = self.storage.get(&ContentAddress::manifest_key(mh)).await
                && let Ok(m) = Manifest::from_bytes(&bytes)
            {
                live_blobs.extend(m.files.values().cloned());
            }
        }

        // Delete now-unreferenced asset blobs.
        let mut blob_keys: Vec<String> = orphan_blobs
            .into_iter()
            .filter(|h| !live_blobs.contains(h))
            .map(|h| ContentAddress::blob_key(&h))
            .collect();
        blob_keys.sort();
        blob_keys.dedup();
        summary.blobs_deleted = blob_keys.len();
        self.storage
            .delete_many(&blob_keys)
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;

        // Delete the orphaned manifest itself.
        self.storage
            .delete(&orphan_key)
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;
        summary.manifest_deleted = true;

        Ok(summary)
    }

    /// Resolve a template reference to its manifest hash
    ///
    /// This method implements the "tag → manifest hash lookup" workflow:
    /// 1. Parses the reference string (namespace/name:tag[@hash])
    /// 2. Looks up the reference in storage to get the manifest hash
    /// 3. Optionally verifies the hash if provided in the reference
    /// 4. Returns the manifest hash for content-addressable access
    ///
    /// # Examples
    /// - `"invoice:latest"` → resolves official template
    /// - `"john/invoice:v1.0.0"` → resolves user template
    /// - `"john/invoice:latest@sha256:abc123"` → resolves with hash verification
    pub async fn resolve(&self, reference: &str) -> Result<String, RegistryError> {
        // Step 1: Parse the reference
        let parsed_ref = Reference::parse(reference)?;

        // Step 2: Build the namespace/tag path for storage lookup
        let namespace_path = match &parsed_ref.namespace {
            Some(ns) => format!("{}/{}", ns, parsed_ref.name),
            None => parsed_ref.name.clone(),
        };
        let tag = parsed_ref.tag_or_default();
        let ref_key = ContentAddress::ref_key(&namespace_path, tag);

        // Step 3: Look up the manifest hash from storage
        let manifest_hash_bytes = self.storage.get(&ref_key).await.map_err(|e| match e {
            crate::storage::blob_storage::StorageError::NotFound(_) => {
                RegistryError::Template(crate::error::TemplateError::not_found(reference))
            }
            _ => RegistryError::Storage(StorageError::backend(e.to_string())),
        })?;

        let manifest_hash = String::from_utf8(manifest_hash_bytes).map_err(|e| {
            RegistryError::Storage(StorageError::backend(format!(
                "Invalid UTF-8 in stored manifest hash: {}",
                e
            )))
        })?;

        // Step 4: Verify hash if provided in reference
        if let Some(expected_hash) = &parsed_ref.hash
            && &manifest_hash != expected_hash
        {
            return Err(RegistryError::Reference(
                crate::error::ReferenceError::hash_mismatch(
                    reference.to_string(),
                    expected_hash.clone(),
                    manifest_hash,
                ),
            ));
        }

        // Return the manifest hash
        Ok(manifest_hash)
    }

    /// Render a template to PDF using JSON data
    ///
    /// This method implements the end-to-end template rendering workflow:
    /// 1. Resolves the template reference to get the manifest hash
    /// 2. Loads the manifest from storage to get file mappings
    /// 3. Hydrates template files/assets into an in-memory file system
    /// 4. Uses papermake to render the template with the provided data
    ///
    /// # Arguments
    /// * `reference` - Template reference (e.g., "john/invoice:latest")
    /// * `data` - JSON data to inject into the template
    ///
    /// # Returns
    /// Returns the PDF bytes on successful rendering
    ///
    /// # Examples
    /// ```rust,no_run
    /// use papermake_registry::Registry;
    /// use papermake_registry::storage::blob_storage::MemoryStorage;
    /// use serde_json::json;
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let storage = MemoryStorage::new();
    /// let registry = Registry::new_storage_only(storage);
    ///
    /// let pdf_bytes = registry.render(
    ///     "john/invoice:latest",
    ///     &json!({
    ///         "customer_name": "Acme Corp",
    ///         "total": "$1,000.00"
    ///     })
    /// ).await?;
    ///
    /// println!("Generated PDF: {} bytes", pdf_bytes.len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn render(
        &self,
        reference: &str,
        data: &serde_json::Value,
    ) -> Result<Vec<u8>, RegistryError> {
        let started = std::time::Instant::now();
        tracing::info!(
            reference = %reference,
            "registry render started",
        );

        // Step 1: Resolve the template reference to get manifest hash
        let manifest_hash = self.resolve(reference).await?;
        tracing::debug!(
            reference = %reference,
            manifest_hash = %manifest_hash,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "registry render resolved reference",
        );

        // Step 2: Load the manifest from storage
        let manifest_key = ContentAddress::manifest_key(&manifest_hash);
        tracing::debug!(
            reference = %reference,
            manifest_key = %manifest_key,
            "registry render loading manifest",
        );
        let manifest_bytes = self.storage.get(&manifest_key).await.map_err(|e| {
            RegistryError::Storage(StorageError::backend(format!(
                "Failed to load manifest {}: {}",
                manifest_hash, e
            )))
        })?;

        let manifest = Manifest::from_bytes(&manifest_bytes).map_err(|e| {
            RegistryError::ContentAddressing(crate::error::ContentAddressingError::manifest_error(
                e.to_string(),
            ))
        })?;
        tracing::debug!(
            reference = %reference,
            files = manifest.files.len(),
            entrypoint = %manifest.entrypoint,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "registry render loaded manifest",
        );

        // Step 3: Get the entrypoint content
        let entrypoint_hash = manifest.entrypoint_hash().ok_or_else(|| {
            RegistryError::Template(crate::error::TemplateError::invalid(
                "Manifest missing entrypoint hash",
            ))
        })?;

        let entrypoint_key = ContentAddress::blob_key(entrypoint_hash);
        tracing::debug!(
            reference = %reference,
            entrypoint_key = %entrypoint_key,
            "registry render loading entrypoint",
        );
        let entrypoint_bytes = self.storage.get(&entrypoint_key).await.map_err(|e| {
            RegistryError::Storage(StorageError::backend(format!(
                "Failed to load entrypoint file: {}",
                e
            )))
        })?;

        let entrypoint_content = String::from_utf8(entrypoint_bytes).map_err(|e| {
            RegistryError::Template(crate::error::TemplateError::invalid(format!(
                "Entrypoint file is not valid UTF-8: {}",
                e
            )))
        })?;
        tracing::debug!(
            reference = %reference,
            entrypoint_bytes = entrypoint_content.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "registry render loaded entrypoint",
        );

        // Step 4: Hydrate template files/assets before entering Typst. Typst's
        // file callbacks are synchronous, so doing async blob reads from inside
        // them can block the Tokio runtime under load.
        let file_system = self.hydrate_file_system(reference, &manifest).await?;

        // Step 5: Render the template using papermake
        tracing::info!(
            reference = %reference,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "registry render invoking typst",
        );
        let render_result = self
            .render_typst_blocking(reference, entrypoint_content, file_system, data.clone())
            .await?;
        tracing::info!(
            reference = %reference,
            success = render_result.success,
            errors = render_result.errors.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "registry render typst returned",
        );

        // Check if rendering was successful
        if render_result.success {
            let pdf = render_result.pdf.ok_or_else(|| {
                RegistryError::Template(crate::error::TemplateError::invalid(
                    "Rendering succeeded but no PDF was generated",
                ))
            })?;
            tracing::info!(
                reference = %reference,
                pdf_size_bytes = pdf.len(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "registry render completed",
            );
            Ok(pdf)
        } else {
            // Collect error messages
            let error_messages: Vec<String> =
                render_result.errors.iter().map(|e| e.to_string()).collect();

            tracing::error!(
                reference = %reference,
                errors = %error_messages.join("; "),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "registry render failed",
            );

            Err(RegistryError::Template(
                crate::error::TemplateError::invalid(format!(
                    "Template rendering failed: {}",
                    error_messages.join("; ")
                )),
            ))
        }
    }

    /// Load every versioned template file into memory before entering Typst.
    ///
    /// Typst's `World::file` callback is synchronous. Keeping all blob I/O on
    /// the async side prevents a render from blocking a Tokio worker while it
    /// tries to re-enter the same runtime for S3 reads.
    async fn hydrate_file_system(
        &self,
        reference: &str,
        manifest: &Manifest,
    ) -> Result<Arc<dyn papermake::RenderFileSystem>, RegistryError> {
        const TEMPLATE_FILE_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

        let started = std::time::Instant::now();
        let mut file_system = papermake::InMemoryFileSystem::new();
        let mut total_bytes = 0usize;

        tracing::debug!(
            reference = %reference,
            files = manifest.files.len(),
            "registry render hydrating template file system",
        );

        for (path, file_hash) in &manifest.files {
            let file_started = std::time::Instant::now();
            let blob_key = ContentAddress::blob_key(file_hash);

            tracing::debug!(
                reference = %reference,
                path = %path,
                blob_key = %blob_key,
                "registry render loading template file",
            );

            let bytes =
                tokio::time::timeout(TEMPLATE_FILE_FETCH_TIMEOUT, self.storage.get(&blob_key))
                    .await
                    .map_err(|_| {
                        RegistryError::Storage(StorageError::backend(format!(
                            "Timed out after {:?} loading template file '{}' from {}",
                            TEMPLATE_FILE_FETCH_TIMEOUT, path, blob_key
                        )))
                    })?
                    .map_err(|e| {
                        RegistryError::Storage(StorageError::backend(format!(
                            "Failed to load template file '{}' from {}: {}",
                            path, blob_key, e
                        )))
                    })?;

            let file_bytes = bytes.len();
            total_bytes += file_bytes;

            // Typst asks for rooted virtual paths (e.g. `/assets/logo.svg`),
            // while manifests store bundle paths without a leading slash.
            file_system.add_file(path, bytes.clone());
            if let Some(unrooted_path) = path.strip_prefix('/') {
                file_system.add_file(unrooted_path, bytes);
            } else {
                file_system.add_file(format!("/{path}"), bytes);
            }

            tracing::debug!(
                reference = %reference,
                path = %path,
                bytes = file_bytes,
                elapsed_ms = file_started.elapsed().as_millis() as u64,
                "registry render loaded template file",
            );
        }

        tracing::debug!(
            reference = %reference,
            files = manifest.files.len(),
            total_bytes,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "registry render hydrated template file system",
        );

        Ok(Arc::new(file_system))
    }

    /// Run the CPU-bound Typst compile/PDF generation on Tokio's blocking pool.
    ///
    /// S3/template hydration is already complete before this is called, so the
    /// blocking task only performs synchronous Typst work over in-memory data.
    async fn render_typst_blocking(
        &self,
        reference: &str,
        entrypoint_content: String,
        file_system: Arc<dyn papermake::RenderFileSystem>,
        data: serde_json::Value,
    ) -> Result<papermake::RenderResult, RegistryError> {
        let started = std::time::Instant::now();
        let timeout = self.render_timeout;
        let timeout_seconds = timeout.as_secs().max(1);

        tracing::debug!(
            reference = %reference,
            timeout_seconds,
            "typst render waiting for blocking permit",
        );

        let permit = tokio::time::timeout(timeout, self.render_semaphore.clone().acquire_owned())
            .await
            .map_err(|_| RegistryError::RenderTimeout {
                seconds: timeout_seconds,
            })?
            .map_err(|_| {
                RegistryError::Template(crate::error::TemplateError::invalid(
                    "Render semaphore was closed",
                ))
            })?;

        let waited_ms = started.elapsed().as_millis() as u64;
        let remaining_timeout = timeout
            .checked_sub(started.elapsed())
            .unwrap_or_else(|| Duration::from_millis(1));
        let reference_for_task = reference.to_string();

        tracing::debug!(
            reference = %reference,
            waited_ms,
            remaining_timeout_ms = remaining_timeout.as_millis() as u64,
            "typst render acquired blocking permit",
        );

        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let task_started = std::time::Instant::now();
            tracing::info!(
                reference = %reference_for_task,
                "typst blocking render started",
            );

            let result = papermake::render_template(entrypoint_content, file_system, &data);

            match &result {
                Ok(render_result) => {
                    tracing::info!(
                        reference = %reference_for_task,
                        success = render_result.success,
                        errors = render_result.errors.len(),
                        elapsed_ms = task_started.elapsed().as_millis() as u64,
                        "typst blocking render finished",
                    );
                }
                Err(error) => {
                    tracing::error!(
                        reference = %reference_for_task,
                        error = %error,
                        elapsed_ms = task_started.elapsed().as_millis() as u64,
                        "typst blocking render failed",
                    );
                }
            }

            result
        });

        tokio::time::timeout(remaining_timeout, task)
            .await
            .map_err(|_| {
                tracing::error!(
                    reference = %reference,
                    timeout_seconds,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "typst blocking render timed out",
                );
                RegistryError::RenderTimeout {
                    seconds: timeout_seconds,
                }
            })?
            .map_err(|e| {
                RegistryError::Template(crate::error::TemplateError::invalid(format!(
                    "Render task failed: {}",
                    e
                )))
            })?
            .map_err(RegistryError::Compilation)
    }

    /// Run cached batch rendering on the blocking pool, returning the warmed
    /// world so the next batch item can continue reusing it.
    async fn render_typst_cached_blocking(
        &self,
        ctx: &BatchCtx,
        mut world: papermake::PapermakeWorld,
        data: serde_json::Value,
    ) -> Result<
        (
            papermake::PapermakeWorld,
            Result<papermake::RenderResult, papermake::PapermakeError>,
        ),
        RegistryError,
    > {
        let started = std::time::Instant::now();
        let timeout = self.render_timeout;
        let timeout_seconds = timeout.as_secs().max(1);

        tracing::debug!(
            reference = %ctx.reference,
            timeout_seconds,
            "cached typst render waiting for blocking permit",
        );

        let permit = tokio::time::timeout(timeout, self.render_semaphore.clone().acquire_owned())
            .await
            .map_err(|_| RegistryError::RenderTimeout {
                seconds: timeout_seconds,
            })?
            .map_err(|_| {
                RegistryError::Template(crate::error::TemplateError::invalid(
                    "Render semaphore was closed",
                ))
            })?;

        let waited_ms = started.elapsed().as_millis() as u64;
        let remaining_timeout = timeout
            .checked_sub(started.elapsed())
            .unwrap_or_else(|| Duration::from_millis(1));
        let reference = ctx.reference.clone();
        let entrypoint_content = ctx.entrypoint_content.clone();
        let file_system = ctx.file_system.clone();

        tracing::debug!(
            reference = %reference,
            waited_ms,
            remaining_timeout_ms = remaining_timeout.as_millis() as u64,
            "cached typst render acquired blocking permit",
        );

        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let task_started = std::time::Instant::now();
            tracing::info!(
                reference = %reference,
                "cached typst blocking render started",
            );

            let result = papermake::render_template_with_cache(
                entrypoint_content,
                file_system,
                data,
                Some(&mut world),
            );

            match &result {
                Ok(render_result) => {
                    tracing::info!(
                        reference = %reference,
                        success = render_result.success,
                        errors = render_result.errors.len(),
                        elapsed_ms = task_started.elapsed().as_millis() as u64,
                        "cached typst blocking render finished",
                    );
                }
                Err(error) => {
                    tracing::error!(
                        reference = %reference,
                        error = %error,
                        elapsed_ms = task_started.elapsed().as_millis() as u64,
                        "cached typst blocking render failed",
                    );
                }
            }

            (world, result)
        });

        tokio::time::timeout(remaining_timeout, task)
            .await
            .map_err(|_| {
                tracing::error!(
                    reference = %ctx.reference,
                    timeout_seconds,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "cached typst blocking render timed out",
                );
                RegistryError::RenderTimeout {
                    seconds: timeout_seconds,
                }
            })?
            .map_err(|e| {
                RegistryError::Template(crate::error::TemplateError::invalid(format!(
                    "Render task failed: {}",
                    e
                )))
            })
    }

    /// Render one template against many inputs, reusing a single warm Typst
    /// world for the whole batch.
    ///
    /// The template is resolved, its manifest/entrypoint loaded, and its world
    /// built **once**; each input then only swaps the injected data. Imports are
    /// fetched from blob storage once (cached in the world) and Typst's layout
    /// memoization stays warm across the batch — a large win when rendering the
    /// same template many times.
    ///
    /// Each render is persisted exactly like [`Registry::render_and_store`]
    /// (`renders/{id}/{meta.json,pdf,data}` + analytics record + retention), so
    /// PDFs are fetched afterwards by id. Returns the `render_id`s in input
    /// order; a failed input still gets an id (its meta records the failure).
    ///
    /// Note: Typst compilation is CPU-bound and runs inline here — a very large
    /// batch will occupy the calling task for a while; prefer running it off the
    /// request path (e.g. the worker) for big jobs.
    pub async fn batch_render(
        &self,
        reference: &str,
        inputs: &[serde_json::Value],
    ) -> Result<Vec<String>, RegistryError> {
        self.batch_render_with_retention(reference, inputs, None)
            .await
    }

    /// Like [`Registry::batch_render`] with a per-batch retention override.
    pub async fn batch_render_with_retention(
        &self,
        reference: &str,
        inputs: &[serde_json::Value],
        retain_override: Option<u32>,
    ) -> Result<Vec<String>, RegistryError> {
        let (ctx, mut world) = self.prepare_batch(reference, retain_override).await?;
        let mut render_ids = Vec::with_capacity(inputs.len());
        for data in inputs {
            let (render_id, _success) = self.render_one(&ctx, &mut world, data).await?;
            render_ids.push(render_id);
        }
        Ok(render_ids)
    }

    /// Create (and persist) a batch job document in `Running` state with one
    /// pending item per input. Returns the job so the caller can spawn
    /// [`Registry::run_batch_job`] and hand the `job_id` back to the client.
    pub async fn create_batch_job(
        &self,
        reference: &str,
        inputs: &[BatchInput],
    ) -> Result<BatchJob, RegistryError> {
        let now = time::OffsetDateTime::now_utc();
        let items = inputs
            .iter()
            .enumerate()
            .map(|(index, input)| BatchItem {
                index,
                key: input.key.clone(),
                render_id: None,
                status: ItemStatus::Pending,
            })
            .collect();
        let job = BatchJob {
            job_id: uuid::Uuid::now_v7().to_string(),
            reference: reference.to_string(),
            status: JobStatus::Running,
            total: inputs.len(),
            done: 0,
            failed: 0,
            created_at: now,
            updated_at: now,
            items,
        };
        self.put_job(&job).await?;
        Ok(job)
    }

    /// Run a batch job to completion, updating its persisted document as each
    /// input is rendered. Intended to be spawned as a background task; the final
    /// `Completed` document (with every `render_id`) is persisted so a client
    /// polling **after** completion still gets the full result.
    pub async fn run_batch_job(
        &self,
        mut job: BatchJob,
        inputs: Vec<BatchInput>,
        retain_override: Option<u32>,
    ) -> Result<(), RegistryError> {
        // Persist job state at most every N items to bound S3 writes on large
        // batches (the final state is always written).
        const FLUSH_EVERY: usize = 20;

        let (ctx, mut world) = match self.prepare_batch(&job.reference, retain_override).await {
            Ok(prepared) => prepared,
            Err(e) => {
                // The whole job couldn't start (e.g. bad reference) — record it.
                for item in &mut job.items {
                    item.status = ItemStatus::Failed;
                }
                job.failed = job.total;
                job.status = JobStatus::Completed;
                job.updated_at = time::OffsetDateTime::now_utc();
                self.put_job(&job).await?;
                return Err(e);
            }
        };

        for (i, input) in inputs.iter().enumerate() {
            match self.render_one(&ctx, &mut world, &input.data).await {
                Ok((render_id, success)) => {
                    job.items[i].render_id = Some(render_id);
                    job.items[i].status = if success {
                        ItemStatus::Success
                    } else {
                        ItemStatus::Failed
                    };
                    if success {
                        job.done += 1;
                    } else {
                        job.failed += 1;
                    }
                }
                Err(_) => {
                    // Storage error persisting this item — count it as failed and
                    // keep going so one bad item doesn't sink the whole batch.
                    job.items[i].status = ItemStatus::Failed;
                    job.failed += 1;
                }
            }
            job.updated_at = time::OffsetDateTime::now_utc();
            if (job.done + job.failed).is_multiple_of(FLUSH_EVERY) {
                let _ = self.put_job(&job).await;
            }
        }

        job.status = JobStatus::Completed;
        job.updated_at = time::OffsetDateTime::now_utc();
        self.put_job(&job).await?;
        Ok(())
    }

    /// Mark orphaned batch jobs as `Interrupted`.
    ///
    /// A batch runs in a background task tied to the process; if the server is
    /// restarted mid-run its job doc is left stuck in `Running`. Call this at
    /// startup with the boot time: any job still `Running` that was created
    /// before boot cannot have a live task, so it's flipped to `Interrupted`
    /// (already-rendered items keep their `render_id`). Returns how many were
    /// reaped.
    pub async fn reap_interrupted_jobs(
        &self,
        started_before: time::OffsetDateTime,
    ) -> Result<usize, RegistryError> {
        let keys = self
            .storage
            .list_keys(crate::render_storage::layout::JOBS_PREFIX)
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;

        let mut reaped = 0;
        for key in keys {
            if !key.ends_with("/job.json") {
                continue;
            }
            let Ok(bytes) = self.storage.get(&key).await else {
                continue;
            };
            let Ok(mut job) = serde_json::from_slice::<BatchJob>(&bytes) else {
                continue;
            };
            if job.status == JobStatus::Running && job.created_at < started_before {
                job.status = JobStatus::Interrupted;
                job.updated_at = time::OffsetDateTime::now_utc();
                self.put_job(&job).await?;
                reaped += 1;
            }
        }
        Ok(reaped)
    }

    /// Fetch a persisted batch job document (for polling). Not-found → error.
    pub async fn get_batch_job(&self, job_id: &str) -> Result<BatchJob, RegistryError> {
        let bytes = self
            .storage
            .get(&crate::render_storage::layout::job_key(job_id))
            .await
            .map_err(|e| Self::map_blob_not_found(e, job_id))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| RegistryError::RenderStorage(RenderStorageError::Serialization(e)))
    }

    /// Persist a batch job document.
    async fn put_job(&self, job: &BatchJob) -> Result<(), RegistryError> {
        let bytes = serde_json::to_vec(job)
            .map_err(|e| RegistryError::RenderStorage(RenderStorageError::Serialization(e)))?;
        self.storage
            .put(&crate::render_storage::layout::job_key(&job.job_id), bytes)
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))
    }

    /// Resolve + load a template and build one warm world, reused for a batch.
    async fn prepare_batch(
        &self,
        reference: &str,
        retain_override: Option<u32>,
    ) -> Result<(BatchCtx, papermake::PapermakeWorld), RegistryError> {
        let parsed_ref = Reference::parse(reference)?;
        let template_name = parsed_ref.name.clone();
        let template_tag = parsed_ref.tag.unwrap_or_else(|| "latest".to_string());

        let manifest_hash = self.resolve(reference).await?;
        let manifest_key = ContentAddress::manifest_key(&manifest_hash);
        let manifest_bytes = self.storage.get(&manifest_key).await.map_err(|e| {
            RegistryError::Storage(StorageError::backend(format!(
                "Failed to load manifest {}: {}",
                manifest_hash, e
            )))
        })?;
        let manifest = Manifest::from_bytes(&manifest_bytes).map_err(|e| {
            RegistryError::ContentAddressing(crate::error::ContentAddressingError::manifest_error(
                e.to_string(),
            ))
        })?;
        let per_template_retain = manifest.metadata.retain_days;
        let entrypoint_hash = manifest
            .entrypoint_hash()
            .ok_or_else(|| {
                RegistryError::Template(crate::error::TemplateError::invalid(
                    "Manifest missing entrypoint hash",
                ))
            })?
            .clone();
        let entrypoint_bytes = self
            .storage
            .get(&ContentAddress::blob_key(&entrypoint_hash))
            .await
            .map_err(|e| {
                RegistryError::Storage(StorageError::backend(format!(
                    "Failed to load entrypoint file: {}",
                    e
                )))
            })?;
        let entrypoint_content = String::from_utf8(entrypoint_bytes).map_err(|e| {
            RegistryError::Template(crate::error::TemplateError::invalid(format!(
                "Entrypoint file is not valid UTF-8: {}",
                e
            )))
        })?;

        let file_system = self.hydrate_file_system(reference, &manifest).await?;
        let world = papermake::PapermakeWorld::with_file_system(
            entrypoint_content.clone(),
            "{}".to_string(),
            file_system.clone(),
        );
        let retain_days = crate::render_storage::retention::effective_retain_days(
            retain_override,
            per_template_retain,
            self.default_retention_days,
        );

        Ok((
            BatchCtx {
                reference: reference.to_string(),
                template_name,
                template_tag,
                manifest_hash,
                retain_days,
                entrypoint_content,
                file_system,
            },
            world,
        ))
    }

    /// Render a single input against the warm world and persist its artifacts +
    /// analytics record (like `render_and_store`). Returns `(render_id, success)`.
    async fn render_one(
        &self,
        ctx: &BatchCtx,
        world: &mut papermake::PapermakeWorld,
        data: &serde_json::Value,
    ) -> Result<(String, bool), RegistryError> {
        let render_id = uuid::Uuid::now_v7().to_string();

        let data_bytes = serde_json::to_vec(data)?;
        let data_hash = ContentAddress::hash(&data_bytes);
        self.storage
            .put(&ContentAddress::render_data_key(&render_id), data_bytes)
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;

        let start_time = std::time::Instant::now();
        let placeholder_world = papermake::PapermakeWorld::with_file_system(
            ctx.entrypoint_content.clone(),
            "{}".to_string(),
            ctx.file_system.clone(),
        );
        let owned_world = std::mem::replace(world, placeholder_world);
        let outcome: Result<papermake::RenderResult, String> = match self
            .render_typst_cached_blocking(ctx, owned_world, data.clone())
            .await
        {
            Ok((updated_world, outcome)) => {
                *world = updated_world;
                outcome.map_err(|e| e.to_string())
            }
            Err(e) => {
                tracing::error!(
                    reference = %ctx.reference,
                    render_id = %render_id,
                    error = %e,
                    "cached typst render task failed",
                );
                Err(e.to_string())
            }
        };
        let duration_ms = start_time.elapsed().as_millis() as u32;

        let timestamp = time::OffsetDateTime::now_utc();
        let expiry_date =
            crate::render_storage::retention::expiry_date(timestamp.date(), ctx.retain_days);

        let (record, success) = match outcome {
            Ok(result) if result.success => {
                let pdf = result.pdf.unwrap_or_default();
                let pdf_hash = ContentAddress::hash(&pdf);
                let pdf_size_bytes = pdf.len() as u32;
                self.storage
                    .put(&ContentAddress::render_pdf_key(&render_id), pdf)
                    .await
                    .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;
                let record = RenderRecord {
                    render_id: render_id.clone(),
                    timestamp,
                    template_ref: ctx.reference.clone(),
                    template_name: ctx.template_name.clone(),
                    template_tag: ctx.template_tag.clone(),
                    manifest_hash: ctx.manifest_hash.clone(),
                    data_hash,
                    pdf_hash,
                    success: true,
                    duration_ms,
                    pdf_size_bytes,
                    error: None,
                    expiry_date,
                };
                (record, true)
            }
            other => {
                let error = match other {
                    Ok(result) => {
                        let msg = result
                            .errors
                            .iter()
                            .map(|e| e.to_string())
                            .collect::<Vec<_>>()
                            .join("; ");
                        if msg.is_empty() {
                            "template rendering failed".to_string()
                        } else {
                            msg
                        }
                    }
                    Err(e) => e,
                };
                let record = RenderRecord {
                    render_id: render_id.clone(),
                    timestamp,
                    template_ref: ctx.reference.clone(),
                    template_name: ctx.template_name.clone(),
                    template_tag: ctx.template_tag.clone(),
                    manifest_hash: ctx.manifest_hash.clone(),
                    data_hash,
                    pdf_hash: String::new(),
                    success: false,
                    duration_ms,
                    pdf_size_bytes: 0,
                    error: Some(error),
                    expiry_date,
                };
                (record, false)
            }
        };

        self.put_render_meta(&record).await?;
        if let Some(render_storage) = &self.render_storage {
            render_storage.store_render(record).await?;
        }
        Ok((render_id, success))
    }

    /// List all templates in the registry
    ///
    /// This method scans all references in storage and groups them by template
    /// to provide a comprehensive list of available templates with their metadata.
    ///
    /// # Returns
    /// Returns a vector of `TemplateInfo` structs containing:
    /// - Template name and namespace
    /// - Available tags
    /// - Latest manifest hash (from "latest" tag or newest tag)
    /// - Template metadata from the manifest
    ///
    /// # Examples
    /// ```rust,no_run
    /// use papermake_registry::Registry;
    /// use papermake_registry::storage::blob_storage::MemoryStorage;
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let storage = MemoryStorage::new();
    /// let registry = Registry::new_storage_only(storage);
    ///
    /// let templates = registry.list_templates().await?;
    /// for template in templates {
    ///     println!("Template: {} ({})", template.name, template.full_name());
    ///     println!("  Tags: {:?}", template.tags);
    ///     println!("  Author: {}", template.metadata.author);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_templates(&self) -> Result<Vec<TemplateInfo>, RegistryError> {
        // Step 1: List all reference keys with "refs/" prefix
        let ref_keys = self
            .storage
            .list_keys("refs/")
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;

        // Step 2: Parse reference keys to extract template information
        let mut templates_map: BTreeMap<String, (Vec<String>, Option<String>)> = BTreeMap::new();

        for ref_key in ref_keys {
            // Parse reference key: "refs/{namespace}/{tag}" or "refs/{namespace}/{name}/{tag}"
            if let Some(parsed) = Self::parse_ref_key(&ref_key) {
                let (namespace_path, tag) = parsed;

                // Add this tag to the template's tag list
                let entry = templates_map
                    .entry(namespace_path.clone())
                    .or_insert((Vec::new(), None));
                entry.0.push(tag.clone());

                // If this is the "latest" tag, remember it for getting metadata
                if tag == "latest" {
                    entry.1 = Some(ref_key.clone());
                }
            }
        }

        // Step 3: For each unique template, resolve metadata
        let mut template_infos = Vec::new();

        for (namespace_path, (mut tags, latest_ref_key)) in templates_map {
            // Sort tags for consistent output
            tags.sort();

            // Use "latest" tag if available, otherwise use the first tag alphabetically
            let ref_key_to_use = latest_ref_key.unwrap_or_else(|| {
                format!(
                    "refs/{}/{}",
                    namespace_path,
                    tags.first().unwrap_or(&"latest".to_string())
                )
            });

            // Get the manifest hash for this reference
            match self.storage.get(&ref_key_to_use).await {
                Ok(manifest_hash_bytes) => {
                    let manifest_hash = match String::from_utf8(manifest_hash_bytes) {
                        Ok(hash) => hash,
                        Err(_) => continue, // Skip invalid UTF-8
                    };

                    // Load the manifest to get metadata
                    let manifest_key = ContentAddress::manifest_key(&manifest_hash);
                    match self.storage.get(&manifest_key).await {
                        Ok(manifest_bytes) => {
                            match Manifest::from_bytes(&manifest_bytes) {
                                Ok(manifest) => {
                                    // Parse namespace and name from namespace_path
                                    let (namespace, name) =
                                        Self::parse_namespace_path(&namespace_path);

                                    let template_info = TemplateInfo::new(
                                        name,
                                        namespace,
                                        tags,
                                        manifest_hash,
                                        manifest.metadata.clone(),
                                    );

                                    template_infos.push(template_info);
                                }
                                Err(_) => {
                                    // Skip templates with invalid manifests
                                    continue;
                                }
                            }
                        }
                        Err(_) => {
                            // Skip templates with missing manifests
                            continue;
                        }
                    }
                }
                Err(_) => {
                    // Skip invalid references
                    continue;
                }
            }
        }

        // Sort templates by full name for consistent output
        template_infos.sort_by_key(|a| a.full_name());

        Ok(template_infos)
    }

    /// Parse a reference key to extract namespace/name path and tag
    ///
    /// Examples:
    /// - "refs/invoice/latest" -> Some(("invoice", "latest"))
    /// - "refs/john/invoice/v1.0.0" -> Some(("john/invoice", "v1.0.0"))
    /// - "invalid/key" -> None
    fn parse_ref_key(ref_key: &str) -> Option<(String, String)> {
        if !ref_key.starts_with("refs/") {
            return None;
        }

        let path = &ref_key[5..]; // Remove "refs/" prefix
        let parts: Vec<&str> = path.split('/').collect();

        if parts.len() < 2 {
            return None;
        }

        // Last part is always the tag
        let tag = parts.last().unwrap().to_string();

        // Everything else is the namespace path
        let namespace_path = parts[..parts.len() - 1].join("/");

        Some((namespace_path, tag))
    }

    /// Parse namespace path to extract namespace and name
    ///
    /// Examples:
    /// - "invoice" -> (None, "invoice")
    /// - "john/invoice" -> (Some("john"), "invoice")
    /// - "acme-corp/letterhead" -> (Some("acme-corp"), "letterhead")
    fn parse_namespace_path(namespace_path: &str) -> (Option<String>, String) {
        let parts: Vec<&str> = namespace_path.split('/').collect();

        if parts.len() == 1 {
            // No namespace, just name
            (None, parts[0].to_string())
        } else if parts.len() == 2 {
            // namespace/name
            (Some(parts[0].to_string()), parts[1].to_string())
        } else {
            // Multiple slashes - treat as namespace/name where namespace includes slashes
            let name = parts.last().unwrap().to_string();
            let namespace = parts[..parts.len() - 1].join("/");
            (Some(namespace), name)
        }
    }

    /// Render a template with comprehensive tracking and content-addressable storage
    ///
    /// This method implements the full render pipeline with tracking:
    /// 1. Parse template reference to extract name/tag
    /// 2. Hash and store input data as content-addressable blob
    /// 3. Measure render execution time
    /// 4. Call existing render logic (template resolution + compilation)
    /// 5. Hash and store PDF output as content-addressable blob
    /// 6. Generate UUIDv7 for distributed-friendly render tracking
    /// 7. Create and store RenderRecord with all metadata
    /// 8. Return RenderResult with tracking info
    ///
    /// # Arguments
    /// * `reference` - Template reference (e.g., "john/invoice:latest")
    /// * `data` - JSON data to inject into the template
    ///
    /// # Returns
    /// Returns `RenderResult` with render ID, PDF bytes, hash, and duration
    ///
    /// # Examples
    /// ```rust,no_run
    /// use papermake_registry::Registry;
    /// use papermake_registry::storage::blob_storage::MemoryStorage;
    /// use papermake_registry::render_storage::MemoryRenderStorage;
    /// use serde_json::json;
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let storage = MemoryStorage::new();
    /// let render_storage = MemoryRenderStorage::new();
    /// let registry = Registry::new(storage, render_storage);
    ///
    /// let result = registry.render_and_store(
    ///     "john/invoice:latest",
    ///     &json!({
    ///         "customer_name": "Acme Corp",
    ///         "total": "$1,000.00"
    ///     })
    /// ).await?;
    ///
    /// println!("Render ID: {}", result.render_id);
    /// println!("PDF size: {} bytes", result.pdf_bytes.len());
    /// println!("Duration: {}ms", result.duration_ms);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn render_and_store(
        &self,
        reference: &str,
        data: &serde_json::Value,
    ) -> Result<RenderResult, RegistryError> {
        self.render_and_store_with_retention(reference, data, None)
            .await
    }

    /// Like [`Registry::render_and_store`] but with a per-render retention
    /// override. Effective retention resolves as: per-render override →
    /// per-template default (manifest metadata) → global default. `0` means
    /// "keep forever" (no expiry-index entry).
    pub async fn render_and_store_with_retention(
        &self,
        reference: &str,
        data: &serde_json::Value,
        retain_override: Option<u32>,
    ) -> Result<RenderResult, RegistryError> {
        let overall_started = std::time::Instant::now();
        // Step 1: Parse template reference to extract name/tag
        let parsed_ref = Reference::parse(reference)?;
        let template_name = parsed_ref.name.clone();
        let template_tag = parsed_ref.tag.unwrap_or_else(|| "latest".to_string());

        // Step 2: Generate the render_id up front. All artifacts for this render
        // (input data, metadata, PDF) are keyed by render_id under `renders/{id}/`
        // so by-id lookups are a direct blob read (no record consulted, immediate
        // even before the analytics record is flushed).
        let render_id = uuid::Uuid::now_v7().to_string();
        tracing::info!(
            reference = %reference,
            render_id = %render_id,
            retain_override = ?retain_override,
            "registry render_and_store started",
        );

        // Step 3: Store the input data at `renders/{id}/data` (kept for both
        // success and failure so a failed input is inspectable). data_hash is
        // retained on the record as integrity metadata only.
        let data_bytes = serde_json::to_vec(data)?;
        let data_hash = ContentAddress::hash(&data_bytes);
        tracing::debug!(
            reference = %reference,
            render_id = %render_id,
            data_size_bytes = data_bytes.len(),
            "registry render_and_store writing input data",
        );
        self.storage
            .put(&ContentAddress::render_data_key(&render_id), data_bytes)
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;
        tracing::debug!(
            reference = %reference,
            render_id = %render_id,
            elapsed_ms = overall_started.elapsed().as_millis() as u64,
            "registry render_and_store wrote input data",
        );

        // Step 4: Measure total operation time including resolution.
        let start_time = std::time::Instant::now();

        // Step 5: Try to resolve and render - catch all failures so that even a
        // failed resolution is recorded as a failure render below.
        tracing::debug!(
            reference = %reference,
            render_id = %render_id,
            "registry render_and_store resolving and rendering",
        );
        let result: Result<(String, Vec<u8>), RegistryError> = async {
            let manifest_hash = self.resolve(reference).await?;
            let pdf_bytes = self.render(reference, data).await?;
            Ok((manifest_hash, pdf_bytes))
        }
        .await;

        let duration_ms = start_time.elapsed().as_millis() as u32;
        tracing::debug!(
            reference = %reference,
            render_id = %render_id,
            render_duration_ms = duration_ms,
            "registry render_and_store render stage returned",
        );

        // Step 6: Build the render record, persist artifacts + meta.json, stage
        // the analytics record.
        match result {
            Ok((manifest_hash, pdf_bytes)) => {
                let pdf_hash = ContentAddress::hash(&pdf_bytes);

                // Store the PDF at `renders/{id}/pdf`.
                self.storage
                    .put(
                        &ContentAddress::render_pdf_key(&render_id),
                        pdf_bytes.clone(),
                    )
                    .await
                    .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;
                tracing::debug!(
                    reference = %reference,
                    render_id = %render_id,
                    pdf_size_bytes = pdf_bytes.len(),
                    elapsed_ms = overall_started.elapsed().as_millis() as u64,
                    "registry render_and_store wrote pdf",
                );

                // Resolve effective retention: per-render → per-template → global.
                let timestamp = time::OffsetDateTime::now_utc();
                let per_template = self.template_retain_days(&manifest_hash).await;
                let retain_days = crate::render_storage::retention::effective_retain_days(
                    retain_override,
                    per_template,
                    self.default_retention_days,
                );
                let expiry_date =
                    crate::render_storage::retention::expiry_date(timestamp.date(), retain_days);

                let record = RenderRecord {
                    render_id: render_id.clone(),
                    timestamp,
                    template_ref: reference.to_string(),
                    template_name,
                    template_tag,
                    manifest_hash,
                    data_hash,
                    pdf_hash: pdf_hash.clone(),
                    success: true,
                    duration_ms,
                    pdf_size_bytes: pdf_bytes.len() as u32,
                    error: None,
                    expiry_date,
                };

                // Persist the record as `renders/{id}/meta.json` so by-id lookups
                // (get_render/get_render_pdf) resolve directly and immediately.
                self.put_render_meta(&record).await?;
                tracing::debug!(
                    reference = %reference,
                    render_id = %render_id,
                    elapsed_ms = overall_started.elapsed().as_millis() as u64,
                    "registry render_and_store wrote render meta",
                );

                // Stage the analytics record (write-only buffer; not a read source).
                if let Some(render_storage) = &self.render_storage {
                    render_storage.store_render(record).await?;
                    tracing::debug!(
                        reference = %reference,
                        render_id = %render_id,
                        elapsed_ms = overall_started.elapsed().as_millis() as u64,
                        "registry render_and_store staged analytics record",
                    );
                }

                tracing::info!(
                    reference = %reference,
                    render_id = %render_id,
                    pdf_hash = %pdf_hash,
                    render_duration_ms = duration_ms,
                    total_elapsed_ms = overall_started.elapsed().as_millis() as u64,
                    "registry render_and_store completed",
                );

                Ok(RenderResult {
                    render_id,
                    pdf_bytes,
                    pdf_hash,
                    duration_ms,
                })
            }
            Err(render_error) => {
                tracing::error!(
                    reference = %reference,
                    render_id = %render_id,
                    render_duration_ms = duration_ms,
                    total_elapsed_ms = overall_started.elapsed().as_millis() as u64,
                    error = %render_error,
                    "registry render_and_store failed",
                );
                // No manifest on failure → no per-template default available.
                let timestamp = time::OffsetDateTime::now_utc();
                let retain_days = crate::render_storage::retention::effective_retain_days(
                    retain_override,
                    None,
                    self.default_retention_days,
                );
                let expiry_date =
                    crate::render_storage::retention::expiry_date(timestamp.date(), retain_days);

                let record = RenderRecord {
                    render_id,
                    timestamp,
                    template_ref: reference.to_string(),
                    template_name,
                    template_tag,
                    manifest_hash: "unknown".to_string(), // Placeholder for failed resolution
                    data_hash,
                    pdf_hash: String::new(),
                    success: false,
                    duration_ms,
                    pdf_size_bytes: 0,
                    error: Some(render_error.to_string()),
                    expiry_date,
                };

                // Persist the failure meta.json (no PDF) so get_render_pdf can
                // distinguish "render failed" (4xx) from "unknown id" (404).
                self.put_render_meta(&record).await?;
                tracing::debug!(
                    reference = %reference,
                    elapsed_ms = overall_started.elapsed().as_millis() as u64,
                    "registry render_and_store wrote failure meta",
                );

                if let Some(render_storage) = &self.render_storage {
                    render_storage.store_render(record).await?;
                    tracing::debug!(
                        reference = %reference,
                        elapsed_ms = overall_started.elapsed().as_millis() as u64,
                        "registry render_and_store staged failure analytics record",
                    );
                }

                Err(render_error)
            }
        }
    }

    /// Best-effort lookup of a template's per-template default retention from
    /// its manifest metadata. Returns `None` on any load/parse issue (the caller
    /// then falls back to the global default).
    async fn template_retain_days(&self, manifest_hash: &str) -> Option<u32> {
        let manifest_key = ContentAddress::manifest_key(manifest_hash);
        let bytes = self.storage.get(&manifest_key).await.ok()?;
        let manifest = Manifest::from_bytes(&bytes).ok()?;
        manifest.metadata.retain_days
    }

    /// Fetch the entrypoint (`main.typ`) source for a template reference, for
    /// the editor. Resolves reference → manifest → entrypoint blob.
    pub async fn get_template_source(&self, reference: &str) -> Result<String, RegistryError> {
        let manifest_hash = self.resolve(reference).await?;
        let manifest_key = ContentAddress::manifest_key(&manifest_hash);
        let manifest_bytes = self.storage.get(&manifest_key).await.map_err(|e| {
            RegistryError::Storage(StorageError::backend(format!(
                "Failed to load manifest {}: {}",
                manifest_hash, e
            )))
        })?;
        let manifest = Manifest::from_bytes(&manifest_bytes).map_err(|e| {
            RegistryError::ContentAddressing(crate::error::ContentAddressingError::manifest_error(
                e.to_string(),
            ))
        })?;
        let entrypoint_hash = manifest.entrypoint_hash().ok_or_else(|| {
            RegistryError::Template(crate::error::TemplateError::invalid(
                "Manifest missing entrypoint hash",
            ))
        })?;
        let bytes = self
            .storage
            .get(&ContentAddress::blob_key(entrypoint_hash))
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;
        String::from_utf8(bytes).map_err(|e| {
            RegistryError::Template(crate::error::TemplateError::invalid(format!(
                "Entrypoint file is not valid UTF-8: {}",
                e
            )))
        })
    }

    /// Fetch the full aggregated analytics summary (for the SSR dashboard).
    pub async fn render_summary(
        &self,
    ) -> Result<crate::render_storage::summary::Summary, RegistryError> {
        let render_storage = self.render_storage.as_ref().ok_or_else(|| {
            RegistryError::RenderStorage(RenderStorageError::Connection(
                "No render storage configured".to_string(),
            ))
        })?;
        Ok(render_storage.summary().await?)
    }

    /// List recent renders for a specific template (from the aggregate).
    pub async fn list_template_renders(
        &self,
        template_name: &str,
        limit: u32,
    ) -> Result<Vec<RenderRecord>, RegistryError> {
        let render_storage = self.render_storage.as_ref().ok_or_else(|| {
            RegistryError::RenderStorage(RenderStorageError::Connection(
                "No render storage configured".to_string(),
            ))
        })?;
        Ok(render_storage
            .list_template_renders(template_name, limit)
            .await?)
    }

    /// Persist a render record as its `renders/{id}/meta.json` blob.
    async fn put_render_meta(&self, record: &RenderRecord) -> Result<(), RegistryError> {
        let meta_bytes = serde_json::to_vec(record)
            .map_err(|e| RegistryError::RenderStorage(RenderStorageError::Serialization(e)))?;
        self.storage
            .put(
                &ContentAddress::render_meta_key(&record.render_id),
                meta_bytes,
            )
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))
    }

    /// Get recent render records
    ///
    /// # Arguments
    /// * `limit` - Maximum number of records to return
    ///
    /// # Returns
    /// Returns a vector of recent `RenderRecord`s sorted by timestamp (newest first)
    ///
    /// # Errors
    /// Returns error if no render storage is configured or if query fails
    pub async fn list_recent_renders(
        &self,
        limit: u32,
    ) -> Result<Vec<RenderRecord>, RegistryError> {
        if let Some(render_storage) = &self.render_storage {
            Ok(render_storage.list_recent_renders(limit).await?)
        } else {
            Err(RegistryError::RenderStorage(
                RenderStorageError::Connection("No render storage configured".to_string()),
            ))
        }
    }

    /// Get render input data by render ID
    ///
    /// Retrieves the original JSON data used for a specific render operation
    /// using content-addressable storage.
    ///
    /// # Arguments
    /// * `render_id` - UUIDv7 render identifier
    ///
    /// # Returns
    /// Returns the original JSON data used for rendering
    ///
    /// # Errors
    /// Returns error if render not found, data not found, or deserialization fails
    pub async fn get_render_data(
        &self,
        render_id: &str,
    ) -> Result<serde_json::Value, RegistryError> {
        // Direct blob read by render_id — no record consulted, works immediately
        // after a render (before any analytics flush).
        let data_bytes = self
            .storage
            .get(&ContentAddress::render_data_key(render_id))
            .await
            .map_err(|e| Self::map_blob_not_found(e, render_id))?;

        let data: serde_json::Value = serde_json::from_slice(&data_bytes)?;
        Ok(data)
    }

    /// Map a blob storage error to a `RenderNotFound` when the key is missing,
    /// otherwise a generic storage error.
    fn map_blob_not_found(
        err: crate::storage::blob_storage::StorageError,
        render_id: &str,
    ) -> RegistryError {
        match err {
            crate::storage::blob_storage::StorageError::NotFound(_) => {
                RegistryError::RenderStorage(RenderStorageError::NotFound(render_id.to_string()))
            }
            other => RegistryError::Storage(StorageError::backend(other.to_string())),
        }
    }

    /// Get rendered PDF by render ID
    ///
    /// Retrieves the PDF output for a specific render operation
    /// using content-addressable storage.
    ///
    /// # Arguments
    /// * `render_id` - UUIDv7 render identifier
    ///
    /// # Returns
    /// Returns the PDF bytes for the rendered template
    ///
    /// # Errors
    /// Returns error if render not found, render failed, or PDF not found
    pub async fn get_render_pdf(&self, render_id: &str) -> Result<Vec<u8>, RegistryError> {
        // 1. Read the render's meta.json (direct blob read by render_id).
        //    Missing meta ⇒ unknown/pruned render ⇒ NotFound (404).
        let record = self.get_render_meta(render_id).await?;

        // 2. A failed render has no PDF ⇒ RenderFailed (4xx), carrying the error.
        if !record.success {
            let msg = record.error.unwrap_or_else(|| "render failed".to_string());
            return Err(RegistryError::RenderStorage(
                RenderStorageError::RenderFailed(msg),
            ));
        }

        // 3. Serve the PDF blob. A missing PDF (e.g. pruned) ⇒ NotFound (404).
        let pdf_bytes = self
            .storage
            .get(&ContentAddress::render_pdf_key(render_id))
            .await
            .map_err(|e| Self::map_blob_not_found(e, render_id))?;
        Ok(pdf_bytes)
    }

    /// Read a render's `meta.json` record directly by render_id.
    ///
    /// This is the durable, immediately-consistent by-id lookup (independent of
    /// the analytics flush). A missing meta blob maps to `RenderNotFound`.
    pub async fn get_render_meta(&self, render_id: &str) -> Result<RenderRecord, RegistryError> {
        let meta_bytes = self
            .storage
            .get(&ContentAddress::render_meta_key(render_id))
            .await
            .map_err(|e| Self::map_blob_not_found(e, render_id))?;
        let record: RenderRecord = serde_json::from_slice(&meta_bytes)
            .map_err(|e| RegistryError::RenderStorage(RenderStorageError::Serialization(e)))?;
        Ok(record)
    }

    /// Get render analytics based on query type
    ///
    /// Supports various analytics queries for render volume, template statistics,
    /// and performance metrics.
    ///
    /// # Arguments
    /// * `query` - The type of analytics query to perform
    ///
    /// # Returns
    /// Returns `AnalyticsResult` containing the requested analytics data
    ///
    /// # Examples
    /// ```rust,no_run
    /// use papermake_registry::{Registry, AnalyticsQuery};
    /// use papermake_registry::storage::blob_storage::MemoryStorage;
    /// use papermake_registry::render_storage::MemoryRenderStorage;
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let storage = MemoryStorage::new();
    /// let render_storage = MemoryRenderStorage::new();
    /// let registry = Registry::new(storage, render_storage);
    ///
    /// // Get render volume over last 30 days
    /// let volume_result = registry.get_render_analytics(
    ///     AnalyticsQuery::VolumeOverTime { days: 30 }
    /// ).await?;
    ///
    /// // Get template statistics
    /// let template_stats = registry.get_render_analytics(
    ///     AnalyticsQuery::TemplateStats
    /// ).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_render_analytics(
        &self,
        query: AnalyticsQuery,
    ) -> Result<AnalyticsResult, RegistryError> {
        let render_storage = self.render_storage.as_ref().ok_or_else(|| {
            RegistryError::RenderStorage(RenderStorageError::Connection(
                "No render storage configured".to_string(),
            ))
        })?;

        match query {
            AnalyticsQuery::VolumeOverTime { days } => {
                let volume = render_storage.render_volume_over_time(days).await?;
                Ok(AnalyticsResult::Volume(volume))
            }
            AnalyticsQuery::TemplateStats => {
                let stats = render_storage.total_renders_per_template().await?;
                Ok(AnalyticsResult::Templates(stats))
            }
            AnalyticsQuery::DurationOverTime { days } => {
                let duration = render_storage.average_duration_over_time(days).await?;
                Ok(AnalyticsResult::Duration(duration))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{S3Storage, bundle::TemplateMetadata, storage::blob_storage::MemoryStorage};

    fn create_test_bundle() -> TemplateBundle {
        let metadata = TemplateMetadata::new("Test Template", "test@example.com");
        let main_content = br#"#let data = json(bytes(sys.inputs.data))
= Test Template
Hello #data.name"#
            .to_vec();

        TemplateBundle::new(main_content, metadata)
            .add_file("assets/logo.png", b"fake_png_data".to_vec())
            .with_schema(
                br#"{"type": "object", "properties": {"name": {"type": "string"}}}"#.to_vec(),
            )
    }

    #[tokio::test]
    async fn test_registry_publish() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);
        let bundle = create_test_bundle();

        let manifest_hash = registry
            .publish(bundle, "test-user/test-template", "latest")
            .await
            .unwrap();

        assert!(manifest_hash.starts_with("sha256:"));
        assert_eq!(manifest_hash.len(), 71); // "sha256:" + 64 hex chars
    }

    #[tokio::test]
    async fn test_registry_publish_stores_all_components() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);
        let bundle = create_test_bundle();

        let manifest_hash = registry
            .publish(bundle, "test-user/test-template", "latest")
            .await
            .unwrap();

        // Check that all components were stored
        let storage_ref = &registry.storage;

        // Should have stored 3 blobs (main.typ, assets/logo.png, schema.json)
        // Plus 1 manifest, plus 1 reference
        // Total: 5 items
        assert_eq!(storage_ref.len(), 5);

        // Verify reference points to manifest hash
        let ref_key = ContentAddress::ref_key("test-user/test-template", "latest");
        let stored_manifest_hash = storage_ref.get(&ref_key).await.unwrap();
        assert_eq!(
            String::from_utf8(stored_manifest_hash).unwrap(),
            manifest_hash
        );
    }

    #[tokio::test]
    async fn test_registry_publish_content_addressable() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);

        // Create identical bundles
        let metadata1 = TemplateMetadata::new("Test Template", "test@example.com");
        let metadata2 = TemplateMetadata::new("Test Template", "test@example.com");
        let main_content = br#"#let data = json(bytes(sys.inputs.data))
= Test Template
Hello #data.name"#
            .to_vec();

        let bundle1 = TemplateBundle::new(main_content.clone(), metadata1)
            .add_file("assets/logo.png", b"fake_png_data".to_vec())
            .with_schema(
                br#"{"type": "object", "properties": {"name": {"type": "string"}}}"#.to_vec(),
            );

        let bundle2 = TemplateBundle::new(main_content, metadata2)
            .add_file("assets/logo.png", b"fake_png_data".to_vec())
            .with_schema(
                br#"{"type": "object", "properties": {"name": {"type": "string"}}}"#.to_vec(),
            );

        let hash1 = registry
            .publish(bundle1, "user1/template", "v1")
            .await
            .unwrap();

        let hash2 = registry
            .publish(bundle2, "user2/template", "v1")
            .await
            .unwrap();

        // Same content should produce same manifest hash
        // The namespace doesn't affect the manifest content, only where the reference is stored
        assert_eq!(hash1, hash2);
    }

    #[tokio::test]
    async fn test_registry_publish_invalid_bundle() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);

        // Create bundle with empty metadata (should fail validation)
        let metadata = TemplateMetadata::new("", "test@example.com");
        let bundle = TemplateBundle::new(b"test content".to_vec(), metadata);

        let result = registry
            .publish(bundle, "test-user/test-template", "latest")
            .await;

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RegistryError::Template(_)));
    }

    #[tokio::test]
    async fn test_registry_resolve_basic() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);
        let bundle = create_test_bundle();

        // First publish a template
        let manifest_hash = registry
            .publish(bundle, "john/invoice", "latest")
            .await
            .unwrap();

        // Then resolve it back
        let resolved_hash = registry.resolve("john/invoice:latest").await.unwrap();

        assert_eq!(manifest_hash, resolved_hash);
    }

    #[tokio::test]
    async fn test_registry_resolve_different_reference_formats() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);
        let bundle = create_test_bundle();

        // Publish template
        let manifest_hash = registry
            .publish(bundle, "john/invoice", "v1.0.0")
            .await
            .unwrap();

        // Test different ways to resolve the same template

        // With explicit tag
        let resolved1 = registry.resolve("john/invoice:v1.0.0").await.unwrap();
        assert_eq!(manifest_hash, resolved1);

        // Without namespace (should fail since we published with namespace)
        let result = registry.resolve("invoice:v1.0.0").await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RegistryError::Template(_)));
    }

    #[tokio::test]
    async fn test_registry_resolve_with_hash_verification() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);
        let bundle = create_test_bundle();

        // Publish template
        let manifest_hash = registry
            .publish(bundle, "john/invoice", "latest")
            .await
            .unwrap();

        // Resolve with correct hash verification
        let reference_with_hash = format!("john/invoice:latest@{}", manifest_hash);
        let resolved_hash = registry.resolve(&reference_with_hash).await.unwrap();
        assert_eq!(manifest_hash, resolved_hash);

        // Resolve with incorrect hash verification (should fail)
        let wrong_hash = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
        let reference_with_wrong_hash = format!("john/invoice:latest@{}", wrong_hash);
        let result = registry.resolve(&reference_with_wrong_hash).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RegistryError::Reference(_)));
    }

    #[tokio::test]
    async fn test_registry_resolve_default_tag() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);
        let bundle = create_test_bundle();

        // Publish with explicit "latest" tag
        let manifest_hash = registry
            .publish(bundle, "john/invoice", "latest")
            .await
            .unwrap();

        // Resolve without tag (should default to "latest")
        let resolved_hash = registry.resolve("john/invoice").await.unwrap();
        assert_eq!(manifest_hash, resolved_hash);
    }

    #[tokio::test]
    async fn test_registry_resolve_nonexistent_template() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);

        // Try to resolve a template that doesn't exist
        let result = registry.resolve("nonexistent/template:latest").await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RegistryError::Template(_)));
    }

    #[tokio::test]
    async fn test_registry_resolve_invalid_reference_format() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);

        // Try to resolve with invalid reference format
        let result = registry.resolve("").await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RegistryError::Reference(_)));

        // Try to resolve with hash only
        let result = registry.resolve("@sha256:abc123").await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RegistryError::Reference(_)));
    }

    #[tokio::test]
    async fn test_registry_resolve_official_template() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);
        let bundle = create_test_bundle();

        // Publish an official template (no namespace)
        let manifest_hash = registry.publish(bundle, "invoice", "latest").await.unwrap();

        // Resolve official template
        let resolved_hash = registry.resolve("invoice:latest").await.unwrap();
        assert_eq!(manifest_hash, resolved_hash);

        // Also test without explicit tag
        let resolved_hash2 = registry.resolve("invoice").await.unwrap();
        assert_eq!(manifest_hash, resolved_hash2);
    }

    #[tokio::test]
    async fn test_registry_publish_resolve_integration() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);

        // Create multiple templates with different namespaces and tags
        let metadata1 = TemplateMetadata::new("Invoice Template", "john@example.com");
        let metadata2 = TemplateMetadata::new("Invoice Template", "alice@example.com");
        let metadata3 = TemplateMetadata::new("Official Invoice", "admin@example.com");

        let content1 = b"Invoice v1 content".to_vec();
        let content2 = b"Invoice v2 content".to_vec();
        let content3 = b"Official invoice content".to_vec();

        let bundle1 = TemplateBundle::new(content1, metadata1);
        let bundle2 = TemplateBundle::new(content2, metadata2);
        let bundle3 = TemplateBundle::new(content3, metadata3);

        // Publish multiple versions and namespaces
        let hash1 = registry
            .publish(bundle1, "john/invoice", "v1.0.0")
            .await
            .unwrap();
        let hash2 = registry
            .publish(bundle2, "alice/invoice", "latest")
            .await
            .unwrap();
        let hash3 = registry
            .publish(bundle3, "invoice", "official")
            .await
            .unwrap();

        // Resolve each template
        let resolved1 = registry.resolve("john/invoice:v1.0.0").await.unwrap();
        let resolved2 = registry.resolve("alice/invoice:latest").await.unwrap();
        let resolved3 = registry.resolve("invoice:official").await.unwrap();

        assert_eq!(hash1, resolved1);
        assert_eq!(hash2, resolved2);
        assert_eq!(hash3, resolved3);

        // Test cross-namespace isolation (these should fail)
        assert!(registry.resolve("john/invoice:latest").await.is_err());
        assert!(registry.resolve("alice/invoice:v1.0.0").await.is_err());
        assert!(registry.resolve("invoice:v1.0.0").await.is_err());

        // Test with hash verification
        let reference_with_hash = format!("john/invoice:v1.0.0@{}", hash1);
        let verified_hash = registry.resolve(&reference_with_hash).await.unwrap();
        assert_eq!(hash1, verified_hash);
    }

    #[tokio::test]
    async fn test_registry_render_basic() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);
        let bundle = create_test_bundle();

        // Publish template
        let _manifest_hash = registry
            .publish(bundle, "john/invoice", "latest")
            .await
            .unwrap();

        // Render template
        let data = serde_json::json!({
            "name": "Test Customer"
        });

        let pdf_bytes = registry.render("john/invoice:latest", &data).await.unwrap();

        assert!(!pdf_bytes.is_empty());
        // PDF should start with PDF header
        assert!(pdf_bytes.starts_with(b"%PDF"));
    }

    #[tokio::test]
    async fn test_registry_render_nonexistent_template() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);

        let data = serde_json::json!({
            "name": "Test Customer"
        });

        let result = registry.render("nonexistent/template:latest", &data).await;

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RegistryError::Template(_)));
    }

    #[tokio::test]
    async fn test_registry_render_with_hash_verification() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);
        let bundle = create_test_bundle();

        // Publish template
        let manifest_hash = registry
            .publish(bundle, "john/invoice", "latest")
            .await
            .unwrap();

        // Render with hash verification
        let data = serde_json::json!({
            "name": "Test Customer"
        });

        let reference_with_hash = format!("john/invoice:latest@{}", manifest_hash);
        let pdf_bytes = registry.render(&reference_with_hash, &data).await.unwrap();

        assert!(!pdf_bytes.is_empty());
        assert!(pdf_bytes.starts_with(b"%PDF"));
    }

    #[tokio::test]
    async fn test_registry_render_with_wrong_hash() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);
        let bundle = create_test_bundle();

        // Publish template
        let _manifest_hash = registry
            .publish(bundle, "john/invoice", "latest")
            .await
            .unwrap();

        // Try to render with wrong hash
        let data = serde_json::json!({
            "name": "Test Customer"
        });

        let wrong_hash = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
        let reference_with_wrong_hash = format!("john/invoice:latest@{}", wrong_hash);

        let result = registry.render(&reference_with_wrong_hash, &data).await;

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RegistryError::Reference(_)));
    }

    #[tokio::test]
    async fn test_registry_render_template_with_imports() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);

        // Create a template with imports
        let metadata = TemplateMetadata::new("Template with Imports", "test@example.com");
        let main_content = br#"#import "header.typ": header

#header(data.title)

= Template Body
Content: #data.content"#
            .to_vec();

        let header_content = br#"#let header(title) = [
  = #title
  #line(length: 100%)
]"#
        .to_vec();

        let bundle =
            TemplateBundle::new(main_content, metadata).add_file("header.typ", header_content);

        // Publish template
        let _manifest_hash = registry
            .publish(bundle, "john/complex-template", "latest")
            .await
            .unwrap();

        // Render template
        let data = serde_json::json!({
            "title": "Invoice Template",
            "content": "This is a test invoice"
        });

        let pdf_bytes = registry
            .render("john/complex-template:latest", &data)
            .await
            .unwrap();

        assert!(!pdf_bytes.is_empty());
        assert!(pdf_bytes.starts_with(b"%PDF"));
    }

    #[tokio::test]
    async fn test_registry_render_template_with_asset_image() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);

        let metadata = TemplateMetadata::new("Template with Asset", "test@example.com");
        let main_content = br#"#set page(width: 80mm, height: 50mm)
#image("assets/logo.svg", width: 24mm)

= #data.title"#
            .to_vec();
        let logo = br##"<svg xmlns="http://www.w3.org/2000/svg" width="120" height="48" viewBox="0 0 120 48">
  <rect width="120" height="48" fill="#d4001a"/>
  <circle cx="96" cy="24" r="12" fill="#ffffff"/>
</svg>"##
            .to_vec();

        let bundle = TemplateBundle::new(main_content, metadata).add_file("assets/logo.svg", logo);

        registry
            .publish(bundle, "john/asset-template", "latest")
            .await
            .unwrap();

        let data = serde_json::json!({
            "title": "Asset render"
        });

        let pdf_bytes = registry
            .render("john/asset-template:latest", &data)
            .await
            .unwrap();

        assert!(!pdf_bytes.is_empty());
        assert!(pdf_bytes.starts_with(b"%PDF"));
    }

    #[tokio::test]
    async fn test_registry_render_different_data() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);
        let bundle = create_test_bundle();

        // Publish template
        let _manifest_hash = registry
            .publish(bundle, "john/invoice", "latest")
            .await
            .unwrap();

        // Render with different data sets
        let data1 = serde_json::json!({
            "name": "Customer A"
        });

        let data2 = serde_json::json!({
            "name": "Customer B"
        });

        let pdf1 = registry
            .render("john/invoice:latest", &data1)
            .await
            .unwrap();

        let pdf2 = registry
            .render("john/invoice:latest", &data2)
            .await
            .unwrap();

        // Both should be valid PDFs
        assert!(pdf1.starts_with(b"%PDF"));
        assert!(pdf2.starts_with(b"%PDF"));

        // PDFs should be different (different content)
        assert_ne!(pdf1, pdf2);
    }

    #[tokio::test]
    async fn test_registry_list_templates_empty() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);

        let templates = registry.list_templates().await.unwrap();
        assert!(templates.is_empty());
    }

    #[tokio::test]
    async fn test_registry_list_templates_single() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);
        let bundle = create_test_bundle();

        // Publish a template
        let _manifest_hash = registry
            .publish(bundle, "john/invoice", "latest")
            .await
            .unwrap();

        let templates = registry.list_templates().await.unwrap();
        assert_eq!(templates.len(), 1);

        let template = &templates[0];
        assert_eq!(template.name, "invoice");
        assert_eq!(template.namespace, Some("john".to_string()));
        assert_eq!(template.tags, vec!["latest"]);
        assert_eq!(template.metadata.name, "Test Template");
        assert_eq!(template.metadata.author, "test@example.com");
        assert_eq!(template.full_name(), "john/invoice");
    }

    // Integration test: requires a live S3 (RustFS) at localhost:9000.
    // Run with `cargo test -- --ignored` after `docker compose up -d rustfs`.
    #[ignore = "requires a live S3 at localhost:9000 (see docker-compose.yml)"]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_registry_list_templates_no_namespace() {
        unsafe {
            std::env::set_var("S3_ENDPOINT_URL", "http://localhost:9000");
            std::env::set_var("S3_ACCESS_KEY_ID", "minioadmin");
            std::env::set_var("S3_SECRET_ACCESS_KEY", "minioadmin");
            std::env::set_var("S3_BUCKET", "papermake-registry-test");
            std::env::set_var("S3_REGION", "us-east-1");
        }
        let storage = S3Storage::from_env().unwrap();
        storage.ensure_bucket().await.unwrap();
        let registry = Registry::new_storage_only(storage);
        let bundle = create_test_bundle();

        // Publish a template
        let _manifest_hash = registry.publish(bundle, "invoice", "latest").await.unwrap();

        let templates = registry.list_templates().await.unwrap();
        assert_eq!(templates.len(), 1);

        let template = &templates[0];
        assert_eq!(template.name, "invoice");
        assert_eq!(template.namespace, None);
        assert_eq!(template.tags, vec!["latest"]);
        assert_eq!(template.metadata.name, "Test Template");
        assert_eq!(template.metadata.author, "test@example.com");
        assert_eq!(template.full_name(), "invoice");
    }

    #[tokio::test]
    async fn test_registry_list_templates_multiple_tags() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);
        let bundle1 = create_test_bundle();
        let bundle2 = create_test_bundle();

        // Publish same template with different tags
        registry
            .publish(bundle1, "john/invoice", "latest")
            .await
            .unwrap();
        registry
            .publish(bundle2, "john/invoice", "v1.0.0")
            .await
            .unwrap();

        let templates = registry.list_templates().await.unwrap();
        assert_eq!(templates.len(), 1);

        let template = &templates[0];
        assert_eq!(template.name, "invoice");
        assert_eq!(template.namespace, Some("john".to_string()));

        // Tags should be sorted
        let mut expected_tags = vec!["latest", "v1.0.0"];
        expected_tags.sort();
        assert_eq!(template.tags, expected_tags);
    }

    #[tokio::test]
    async fn test_registry_list_templates_multiple_templates() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);

        // Create different bundles
        let metadata1 = TemplateMetadata::new("Invoice Template", "john@example.com");
        let metadata2 = TemplateMetadata::new("Letter Template", "alice@example.com");
        let metadata3 = TemplateMetadata::new("Official Invoice", "admin@example.com");

        let bundle1 = TemplateBundle::new(b"invoice content".to_vec(), metadata1);
        let bundle2 = TemplateBundle::new(b"letter content".to_vec(), metadata2);
        let bundle3 = TemplateBundle::new(b"official content".to_vec(), metadata3);

        // Publish templates in different namespaces
        registry
            .publish(bundle1, "john/invoice", "latest")
            .await
            .unwrap();
        registry
            .publish(bundle2, "alice/letter", "latest")
            .await
            .unwrap();
        registry
            .publish(bundle3, "invoice", "official")
            .await
            .unwrap(); // No namespace

        let templates = registry.list_templates().await.unwrap();
        assert_eq!(templates.len(), 3);

        // Templates should be sorted by full name
        assert_eq!(templates[0].full_name(), "alice/letter");
        assert_eq!(templates[1].full_name(), "invoice");
        assert_eq!(templates[2].full_name(), "john/invoice");

        // Check individual templates
        assert_eq!(templates[0].namespace, Some("alice".to_string()));
        assert_eq!(templates[0].name, "letter");
        assert_eq!(templates[0].metadata.author, "alice@example.com");

        assert_eq!(templates[1].namespace, None);
        assert_eq!(templates[1].name, "invoice");
        assert_eq!(templates[1].metadata.author, "admin@example.com");

        assert_eq!(templates[2].namespace, Some("john".to_string()));
        assert_eq!(templates[2].name, "invoice");
        assert_eq!(templates[2].metadata.author, "john@example.com");
    }

    #[tokio::test]
    async fn test_parse_ref_key() {
        // Test valid reference keys
        assert_eq!(
            Registry::<MemoryStorage, crate::render_storage::MemoryRenderStorage>::parse_ref_key(
                "refs/invoice/latest"
            ),
            Some(("invoice".to_string(), "latest".to_string()))
        );

        assert_eq!(
            Registry::<MemoryStorage, crate::render_storage::MemoryRenderStorage>::parse_ref_key(
                "refs/john/invoice/v1.0.0"
            ),
            Some(("john/invoice".to_string(), "v1.0.0".to_string()))
        );

        assert_eq!(
            Registry::<MemoryStorage, crate::render_storage::MemoryRenderStorage>::parse_ref_key(
                "refs/org/user/template/stable"
            ),
            Some(("org/user/template".to_string(), "stable".to_string()))
        );

        // Test invalid reference keys
        assert_eq!(
            Registry::<MemoryStorage, crate::render_storage::MemoryRenderStorage>::parse_ref_key(
                "invalid/key"
            ),
            None
        );

        assert_eq!(
            Registry::<MemoryStorage, crate::render_storage::MemoryRenderStorage>::parse_ref_key(
                "refs/"
            ),
            None
        );

        assert_eq!(
            Registry::<MemoryStorage, crate::render_storage::MemoryRenderStorage>::parse_ref_key(
                "refs/onlyname"
            ),
            None
        );
    }

    #[tokio::test]
    async fn test_parse_namespace_path() {
        // Test different namespace path formats
        assert_eq!(
            Registry::<MemoryStorage, crate::render_storage::MemoryRenderStorage>::parse_namespace_path("invoice"),
            (None, "invoice".to_string())
        );

        assert_eq!(
            Registry::<MemoryStorage, crate::render_storage::MemoryRenderStorage>::parse_namespace_path("john/invoice"),
            (Some("john".to_string()), "invoice".to_string())
        );

        assert_eq!(
            Registry::<MemoryStorage, crate::render_storage::MemoryRenderStorage>::parse_namespace_path("org/user/template"),
            (Some("org/user".to_string()), "template".to_string())
        );
    }

    #[tokio::test]
    async fn test_render_and_store_success() {
        let storage = MemoryStorage::new();
        let render_storage = crate::render_storage::MemoryRenderStorage::new();
        let registry = Registry::new(storage, render_storage);
        let bundle = create_test_bundle();

        // First publish a template
        let _manifest_hash = registry
            .publish(bundle, "test-user/test-template", "latest")
            .await
            .unwrap();

        // Test data for rendering
        let test_data = serde_json::json!({
            "name": "Test User"
        });

        // Render with storage tracking
        let result = registry
            .render_and_store("test-user/test-template:latest", &test_data)
            .await
            .unwrap();

        // Verify result structure
        assert!(!result.render_id.is_empty());
        assert!(!result.pdf_bytes.is_empty());
        assert!(result.pdf_hash.starts_with("sha256:"));
        assert!(result.duration_ms > 0);

        // Verify render record was stored
        let records = registry.list_recent_renders(10).await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].render_id, result.render_id);
        assert_eq!(records[0].template_name, "test-template");
        assert_eq!(records[0].template_tag, "latest");
        assert!(records[0].success);
    }

    #[tokio::test]
    async fn test_render_and_store_without_render_storage() {
        let storage = MemoryStorage::new();
        // Create registry without render storage using new method for blob-only
        let registry =
            Registry::<MemoryStorage, crate::render_storage::MemoryRenderStorage>::new_blob_only(
                storage,
            );
        let bundle = create_test_bundle();

        // First publish a template
        let _manifest_hash = registry
            .publish(bundle, "test-user/test-template", "latest")
            .await
            .unwrap();

        // Test data for rendering
        let test_data = serde_json::json!({
            "name": "Test User"
        });

        // Render with storage tracking should still work but not store records
        let result = registry
            .render_and_store("test-user/test-template:latest", &test_data)
            .await
            .unwrap();

        // Verify result structure
        assert!(!result.render_id.is_empty());
        assert!(!result.pdf_bytes.is_empty());
        assert!(result.pdf_hash.starts_with("sha256:"));
        assert!(result.duration_ms > 0);

        // Trying to list renders should fail without render storage
        let list_result = registry.list_recent_renders(10).await;
        assert!(list_result.is_err());
    }

    #[tokio::test]
    async fn test_render_history_methods() {
        let storage = MemoryStorage::new();
        let render_storage = crate::render_storage::MemoryRenderStorage::new();
        let registry = Registry::new(storage, render_storage);
        let bundle = create_test_bundle();

        // First publish a template
        let _manifest_hash = registry
            .publish(bundle, "test-user/test-template", "latest")
            .await
            .unwrap();

        // Test data for rendering
        let test_data = serde_json::json!({
            "name": "Test User",
            "age": 25
        });

        // Render with storage tracking
        let result = registry
            .render_and_store("test-user/test-template:latest", &test_data)
            .await
            .unwrap();

        // Test get_render_data
        let retrieved_data = registry.get_render_data(&result.render_id).await.unwrap();
        assert_eq!(retrieved_data, test_data);

        // Test get_render_pdf
        let retrieved_pdf = registry.get_render_pdf(&result.render_id).await.unwrap();
        assert_eq!(retrieved_pdf, result.pdf_bytes);

        // Test with non-existent render ID
        let invalid_result = registry.get_render_data("invalid-uuid").await;
        assert!(invalid_result.is_err());
    }

    #[tokio::test]
    async fn test_render_and_store_failure_tracking() {
        let storage = MemoryStorage::new();
        let render_storage = crate::render_storage::MemoryRenderStorage::new();
        let registry = Registry::new(storage, render_storage);

        // Test data for rendering
        let test_data = serde_json::json!({
            "name": "Test User"
        });

        // Try to render non-existent template (should fail)
        let result = registry
            .render_and_store("non-existent:latest", &test_data)
            .await;
        assert!(result.is_err());

        // Verify failure was still tracked in render storage
        let records = registry.list_recent_renders(10).await.unwrap();
        assert_eq!(records.len(), 1);
        assert!(!records[0].success);
        assert!(records[0].error.is_some());
        assert_eq!(records[0].template_name, "non-existent");
        assert_eq!(records[0].template_tag, "latest");

        // Getting PDF for failed render should fail
        let pdf_result = registry.get_render_pdf(&records[0].render_id).await;
        assert!(pdf_result.is_err());
    }

    #[tokio::test]
    async fn test_render_analytics() {
        let storage = MemoryStorage::new();
        let render_storage = crate::render_storage::MemoryRenderStorage::new();
        let registry = Registry::new(storage, render_storage);
        let bundle = create_test_bundle();

        // First publish a template
        let _manifest_hash = registry
            .publish(bundle, "test-user/test-template", "latest")
            .await
            .unwrap();

        // Create another template for variety
        let bundle2 = create_test_bundle();
        let _manifest_hash2 = registry
            .publish(bundle2, "other-template", "v1")
            .await
            .unwrap();

        // Test data for rendering
        let test_data = serde_json::json!({
            "name": "Test User"
        });

        // Render multiple times to generate analytics data
        for i in 0..3 {
            let data = serde_json::json!({
                "name": format!("User {}", i)
            });
            let _result = registry
                .render_and_store("test-user/test-template:latest", &data)
                .await
                .unwrap();
        }

        // Render other template once
        let _result = registry
            .render_and_store("other-template:v1", &test_data)
            .await
            .unwrap();

        // Test volume analytics
        let volume_result = registry
            .get_render_analytics(AnalyticsQuery::VolumeOverTime { days: 1 })
            .await
            .unwrap();
        if let AnalyticsResult::Volume(volume_points) = volume_result {
            assert!(!volume_points.is_empty());
            assert!(volume_points.iter().any(|p| p.renders >= 4));
        } else {
            panic!("Expected Volume result");
        }

        // Test template statistics
        let template_result = registry
            .get_render_analytics(AnalyticsQuery::TemplateStats)
            .await
            .unwrap();
        if let AnalyticsResult::Templates(template_stats) = template_result {
            assert_eq!(template_stats.len(), 2);
            let test_template_stats = template_stats
                .iter()
                .find(|s| s.template_name == "test-template")
                .unwrap();
            assert_eq!(test_template_stats.total_renders, 3);

            let other_template_stats = template_stats
                .iter()
                .find(|s| s.template_name == "other-template")
                .unwrap();
            assert_eq!(other_template_stats.total_renders, 1);
        } else {
            panic!("Expected Templates result");
        }

        // Test duration analytics
        let duration_result = registry
            .get_render_analytics(AnalyticsQuery::DurationOverTime { days: 1 })
            .await
            .unwrap();
        if let AnalyticsResult::Duration(duration_points) = duration_result {
            assert!(!duration_points.is_empty());
            assert!(duration_points.iter().any(|p| p.avg_duration_ms > 0.0));
        } else {
            panic!("Expected Duration result");
        }
    }

    #[tokio::test]
    async fn test_extract_template_name() {
        use crate::reference::Reference;

        // Test various reference formats
        let ref1 = Reference::parse("invoice:latest").unwrap();
        assert_eq!(ref1.name, "invoice");

        let ref2 = Reference::parse("john/invoice:latest").unwrap();
        assert_eq!(ref2.name, "invoice");

        let ref3 = Reference::parse("acme-corp/letterhead:stable").unwrap();
        assert_eq!(ref3.name, "letterhead");
    }

    #[tokio::test]
    async fn test_content_addressable_storage() {
        let storage = MemoryStorage::new();
        let render_storage = crate::render_storage::MemoryRenderStorage::new();
        let registry = Registry::new(storage, render_storage);
        let bundle = create_test_bundle();

        // First publish a template
        let _manifest_hash = registry
            .publish(bundle, "test-user/test-template", "latest")
            .await
            .unwrap();

        // Test data for rendering
        let test_data = serde_json::json!({
            "name": "Test User"
        });

        // Render twice with same data
        let result1 = registry
            .render_and_store("test-user/test-template:latest", &test_data)
            .await
            .unwrap();

        let result2 = registry
            .render_and_store("test-user/test-template:latest", &test_data)
            .await
            .unwrap();

        // Different render IDs but same content hashes (due to deduplication)
        assert_ne!(result1.render_id, result2.render_id);
        assert_eq!(result1.pdf_hash, result2.pdf_hash);

        // Verify both renders can retrieve the same PDF content
        let pdf1 = registry.get_render_pdf(&result1.render_id).await.unwrap();
        let pdf2 = registry.get_render_pdf(&result2.render_id).await.unwrap();
        assert_eq!(pdf1, pdf2);

        // Verify both renders can retrieve the same data content
        let data1 = registry.get_render_data(&result1.render_id).await.unwrap();
        let data2 = registry.get_render_data(&result2.render_id).await.unwrap();
        assert_eq!(data1, data2);
        assert_eq!(data1, test_data);
    }

    #[tokio::test]
    async fn test_render_and_store_retention_precedence() {
        let storage = MemoryStorage::new();
        let render_storage = crate::render_storage::MemoryRenderStorage::new();
        let registry = Registry::new(storage, render_storage).with_retention_days(30);

        // Publish a template with a per-template default of 7 days.
        let metadata =
            TemplateMetadata::new("Test Template", "test@example.com").with_retain_days(7);
        let main_content = br#"#let data = json(bytes(sys.inputs.data))
= Test
Hello #data.name"#
            .to_vec();
        let bundle = TemplateBundle::new(main_content, metadata);
        registry.publish(bundle, "t", "latest").await.unwrap();

        let data = serde_json::json!({ "name": "x" });
        let today = time::OffsetDateTime::now_utc().date();

        // Per-template default (7) applies with no override.
        let r_default = registry.render_and_store("t:latest", &data).await.unwrap();
        let meta = registry
            .get_render_meta(&r_default.render_id)
            .await
            .unwrap();
        assert_eq!(meta.expiry_date, today.checked_add(time::Duration::days(7)));

        // Per-render override (3) beats the template default.
        let r_override = registry
            .render_and_store_with_retention("t:latest", &data, Some(3))
            .await
            .unwrap();
        let meta = registry
            .get_render_meta(&r_override.render_id)
            .await
            .unwrap();
        assert_eq!(meta.expiry_date, today.checked_add(time::Duration::days(3)));

        // Override of 0 means keep forever → no expiry date.
        let r_forever = registry
            .render_and_store_with_retention("t:latest", &data, Some(0))
            .await
            .unwrap();
        let meta = registry
            .get_render_meta(&r_forever.render_id)
            .await
            .unwrap();
        assert_eq!(meta.expiry_date, None);
    }

    #[tokio::test]
    async fn test_batch_render_reuses_template() {
        let storage = MemoryStorage::new();
        let render_storage = crate::render_storage::MemoryRenderStorage::new();
        let registry = Registry::new(storage, render_storage);
        registry
            .publish(create_test_bundle(), "batch", "latest")
            .await
            .unwrap();

        let inputs = vec![
            serde_json::json!({ "name": "A" }),
            serde_json::json!({ "name": "B" }),
            serde_json::json!({ "name": "C" }),
        ];
        let ids = registry
            .batch_render("batch:latest", &inputs)
            .await
            .unwrap();

        assert_eq!(ids.len(), 3);
        // Distinct render ids.
        assert_eq!(
            ids.iter().collect::<std::collections::HashSet<_>>().len(),
            3
        );
        // Each render's PDF is fetchable by id, and its input round-trips.
        for (id, input) in ids.iter().zip(&inputs) {
            let pdf = registry.get_render_pdf(id).await.unwrap();
            assert!(pdf.starts_with(b"%PDF"));
            assert_eq!(&registry.get_render_data(id).await.unwrap(), input);
        }
        // All three analytics records were staged.
        let recent = registry.list_recent_renders(10).await.unwrap();
        assert_eq!(recent.len(), 3);
    }

    #[tokio::test]
    async fn test_batch_job_persists_and_completes() {
        use crate::batch::{BatchInput, ItemStatus, JobStatus};

        let storage = MemoryStorage::new();
        let render_storage = crate::render_storage::MemoryRenderStorage::new();
        let registry = Registry::new(storage, render_storage);
        registry
            .publish(create_test_bundle(), "batch", "latest")
            .await
            .unwrap();

        let inputs = vec![
            BatchInput {
                data: serde_json::json!({ "name": "A" }),
                key: Some("cust-a".to_string()),
            },
            BatchInput {
                data: serde_json::json!({ "name": "B" }),
                key: None,
            },
        ];

        // Job is persisted immediately (running, all items pending).
        let job = registry
            .create_batch_job("batch:latest", &inputs)
            .await
            .unwrap();
        let job_id = job.job_id.clone();
        let initial = registry.get_batch_job(&job_id).await.unwrap();
        assert_eq!(initial.status, JobStatus::Running);
        assert_eq!(initial.total, 2);
        assert!(
            initial
                .items
                .iter()
                .all(|i| i.status == ItemStatus::Pending)
        );

        // Run to completion (synchronously in the test; the server spawns this).
        registry.run_batch_job(job, inputs, None).await.unwrap();

        // Polling AFTER completion still returns the full, persisted result.
        let done = registry.get_batch_job(&job_id).await.unwrap();
        assert_eq!(done.status, JobStatus::Completed);
        assert_eq!(done.done, 2);
        assert_eq!(done.failed, 0);
        assert_eq!(done.items[0].key.as_deref(), Some("cust-a"));
        assert_eq!(done.items[1].key, None);
        for item in &done.items {
            assert_eq!(item.status, ItemStatus::Success);
            let render_id = item.render_id.as_ref().expect("render_id set");
            let pdf = registry.get_render_pdf(render_id).await.unwrap();
            assert!(pdf.starts_with(b"%PDF"));
        }
    }

    #[tokio::test]
    async fn test_reap_interrupted_jobs() {
        use crate::batch::{BatchInput, JobStatus};

        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);

        // create_batch_job just persists the doc (no rendering), so no publish
        // is needed for this test.
        let inputs = vec![BatchInput {
            data: serde_json::json!({}),
            key: None,
        }];
        let job = registry
            .create_batch_job("x:latest", &inputs)
            .await
            .unwrap();
        let job_id = job.job_id.clone();

        // A cutoff after the job's creation reaps it.
        let cutoff = time::OffsetDateTime::now_utc() + time::Duration::seconds(1);
        assert_eq!(registry.reap_interrupted_jobs(cutoff).await.unwrap(), 1);
        assert_eq!(
            registry.get_batch_job(&job_id).await.unwrap().status,
            JobStatus::Interrupted
        );

        // Idempotent: an already-interrupted job is not reaped again.
        assert_eq!(registry.reap_interrupted_jobs(cutoff).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_delete_version_gc_shared_and_unique_assets() {
        let storage = MemoryStorage::new();
        let registry = Registry::new_storage_only(storage);

        let logo = b"SHARED-LOGO-BYTES".to_vec();
        let main_a = b"#let data = json(bytes(sys.inputs.data))\n= A #data.name".to_vec();
        let main_b = b"#let data = json(bytes(sys.inputs.data))\n= B #data.name".to_vec();

        // a:latest and a:v2 are byte-identical → same manifest hash (dedup).
        // b:latest shares the same logo blob but has a different main.typ.
        let bundle_a = || {
            TemplateBundle::new(main_a.clone(), TemplateMetadata::new("A", "a@x.com"))
                .add_file("assets/logo.png", logo.clone())
        };
        let bundle_b = TemplateBundle::new(main_b.clone(), TemplateMetadata::new("B", "b@x.com"))
            .add_file("assets/logo.png", logo.clone());

        registry.publish(bundle_a(), "a", "latest").await.unwrap();
        registry.publish(bundle_a(), "a", "v2").await.unwrap();
        registry.publish(bundle_b, "b", "latest").await.unwrap();

        let logo_key = ContentAddress::blob_key(&ContentAddress::hash(&logo));
        let main_a_key = ContentAddress::blob_key(&ContentAddress::hash(&main_a));

        // Delete a:v2 — its manifest is still referenced by a:latest, so nothing
        // is garbage-collected.
        let s1 = registry.delete_version("a", "v2").await.unwrap();
        assert!(s1.manifest_kept);
        assert!(!s1.manifest_deleted);
        assert_eq!(s1.blobs_deleted, 0);
        assert!(registry.resolve("a:v2").await.is_err());
        assert!(registry.resolve("a:latest").await.is_ok());

        // Delete a:latest — manifest now orphaned. main.typ(A) is unique → gone;
        // the logo is shared with b:latest → kept.
        let s2 = registry.delete_version("a", "latest").await.unwrap();
        assert!(s2.manifest_deleted);
        assert_eq!(s2.blobs_deleted, 1);
        assert!(!registry.storage.exists(&main_a_key).await.unwrap());
        assert!(registry.storage.exists(&logo_key).await.unwrap());
        assert!(registry.resolve("a:latest").await.is_err());
        assert!(registry.resolve("b:latest").await.is_ok());

        // Deleting a missing version errors.
        assert!(registry.delete_version("a", "latest").await.is_err());
    }
}
