use super::*;

impl<S: BlobStorage + 'static, R: RenderStorage + 'static> Registry<S, R> {
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
        self.render_and_store_with(reference, data, None, &RenderOptions::default())
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
        self.render_and_store_with(reference, data, retain_override, &RenderOptions::default())
            .await
    }

    /// Like [`Registry::render_and_store`] but with explicit PDF export options
    /// (e.g. PDF/A-3b). See [`Registry::render_with_options`].
    pub async fn render_and_store_with_options(
        &self,
        reference: &str,
        data: &serde_json::Value,
        options: &RenderOptions,
    ) -> Result<RenderResult, RegistryError> {
        self.render_and_store_with(reference, data, None, options)
            .await
    }

    /// Render with tracking, controlling both per-render retention and PDF
    /// export options. The other `render_and_store*` methods delegate here.
    pub async fn render_and_store_with(
        &self,
        reference: &str,
        data: &serde_json::Value,
        retain_override: Option<u32>,
        options: &RenderOptions,
    ) -> Result<RenderResult, RegistryError> {
        let overall_started = std::time::Instant::now();
        // Step 1: Parse template reference to extract name/tag
        let parsed_ref = Reference::parse(reference)?;
        let template_name = parsed_ref.name.clone();
        let template_tag = parsed_ref.tag.unwrap_or_else(|| "latest".to_string());

        // Step 2: Serialize the input and resolve the template up front, so the
        // render_id can be content-addressed (a UUIDv5 over the manifest + data
        // hashes). Identical (template version, data) renders share an id and are
        // idempotent; artifacts remain keyed by render_id under `renders/{id}/`.
        let data_bytes = serde_json::to_vec(data)?;
        let data_hash = ContentAddress::hash(&data_bytes);
        let manifest_res = self.resolve(reference).await;
        let opts_tag = render_options_tag(options);
        let render_id = match &manifest_res {
            Ok(manifest_hash) => {
                ContentAddress::content_render_id_with_options(manifest_hash, &data_hash, &opts_tag)
            }
            // Resolution failed: still record a deterministic failure id keyed by
            // the reference so repeated identical failures dedupe too.
            Err(_) => {
                ContentAddress::content_render_id_with_options(reference, &data_hash, &opts_tag)
            }
        };
        tracing::debug!(
            reference = %reference,
            render_id = %render_id,
            retain_override = ?retain_override,
            "registry render_and_store started",
        );

        // Step 3: Store the input data at `renders/{id}/data` (kept for both
        // success and failure so a failed input is inspectable).
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

        // Step 5: Render - a failed resolution (captured above) is recorded as a
        // failure render below.
        tracing::debug!(
            reference = %reference,
            render_id = %render_id,
            "registry render_and_store resolving and rendering",
        );
        let result: Result<(String, Vec<u8>), RegistryError> = async {
            let manifest_hash = manifest_res?;
            let pdf_bytes = self.render_with_options(reference, data, options).await?;
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

                let record = RenderRecord::from_outcome(
                    render_id.clone(),
                    timestamp,
                    reference.to_string(),
                    template_name,
                    template_tag,
                    manifest_hash,
                    data_hash,
                    duration_ms,
                    expiry_date,
                    Ok((pdf_hash.clone(), pdf_bytes.len() as u32)),
                );

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

                tracing::debug!(
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

                let record = RenderRecord::from_outcome(
                    render_id,
                    timestamp,
                    reference.to_string(),
                    template_name,
                    template_tag,
                    "unknown".to_string(), // Placeholder for failed resolution
                    data_hash,
                    duration_ms,
                    expiry_date,
                    Err(render_error.to_string()),
                );

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
    pub(crate) async fn template_retain_days(&self, manifest_hash: &str) -> Option<u32> {
        let manifest_key = ContentAddress::manifest_key(manifest_hash);
        let bytes = self.get_immutable(&manifest_key).await.ok()?;
        let manifest = Manifest::from_bytes(&bytes).ok()?;
        manifest.metadata.retain_days
    }

    /// Fetch the entrypoint (`main.typ`) source for a template reference, for
    /// the editor. Resolves reference → manifest → entrypoint blob.
    pub async fn get_template_source(&self, reference: &str) -> Result<String, RegistryError> {
        let manifest_hash = self.resolve(reference).await?;
        let manifest = self.load_manifest(&manifest_hash).await?;
        self.load_entrypoint(&manifest).await
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
    pub(crate) async fn put_render_meta(&self, record: &RenderRecord) -> Result<(), RegistryError> {
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
    pub(crate) fn map_blob_not_found(
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
