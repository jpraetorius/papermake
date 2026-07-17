use super::*;

impl<S: BlobStorage + 'static, R: RenderStorage + 'static> Registry<S, R> {
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
        self.render_with_options(reference, data, &RenderOptions::default())
            .await
    }

    /// Render a template to PDF with explicit export options (e.g. PDF/A-3b
    /// conformant output for archival or ZUGFeRD/Factur-X e-invoices). Behaves
    /// like [`Registry::render`] otherwise.
    pub async fn render_with_options(
        &self,
        reference: &str,
        data: &serde_json::Value,
        options: &RenderOptions,
    ) -> Result<Vec<u8>, RegistryError> {
        let started = std::time::Instant::now();
        tracing::debug!(
            reference = %reference,
            "registry render started",
        );

        // Resolve + load manifest, entrypoint, and hydrated file system/fonts.
        let loaded = self.load_template(reference).await?;
        tracing::debug!(
            reference = %reference,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "registry render loaded template",
        );

        self.render_loaded_template(reference, loaded, data, options)
            .await
    }

    /// Render an already-resolved template. This keeps callers that track
    /// content-addressed output tied to the same immutable manifest they used
    /// for render-id and metadata decisions.
    pub(crate) async fn render_loaded_template(
        &self,
        reference: &str,
        loaded: LoadedTemplate,
        data: &serde_json::Value,
        options: &RenderOptions,
    ) -> Result<Vec<u8>, RegistryError> {
        let started = std::time::Instant::now();
        let LoadedTemplate {
            entrypoint_content,
            file_system,
            fonts,
            ..
        } = loaded;

        tracing::debug!(
            reference = %reference,
            "registry render invoking typst",
        );
        let render_result = self
            .render_typst_blocking(
                reference,
                entrypoint_content,
                file_system,
                fonts,
                data.clone(),
                options.clone(),
            )
            .await?;
        tracing::debug!(
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
            tracing::debug!(
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
    pub(crate) async fn hydrate_file_system(
        &self,
        reference: &str,
        manifest: &Manifest,
    ) -> Result<(Arc<dyn papermake::RenderFileSystem>, Vec<Font>), RegistryError> {
        const TEMPLATE_FILE_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

        use futures_util::StreamExt;

        /// Max template files fetched concurrently while hydrating.
        const FILE_FETCH_CONCURRENCY: usize = 16;

        let started = std::time::Instant::now();
        let mut file_system = papermake::InMemoryFileSystem::new();
        let mut total_bytes = 0usize;
        // Font faces contributed by the template's own font assets (any
        // .ttf/.otf/.ttc file), registered on top of the process font set.
        let mut fonts: Vec<Font> = Vec::new();

        tracing::debug!(
            reference = %reference,
            files = manifest.files.len(),
            "registry render hydrating template file system",
        );

        // Fetch every file concurrently (bounded); building the world is done
        // afterwards so hydration latency scales with the slowest file, not the
        // sum of all of them. Collect owned (path, hash) pairs up front so the
        // stream — and each future — borrows nothing from `self` or `manifest`;
        // a borrow here imposes a higher-ranked lifetime that breaks `Send` when
        // the render runs inside `tokio::spawn`.
        let files: Vec<(String, String)> = manifest
            .files
            .iter()
            .map(|(path, hash)| (path.clone(), hash.clone()))
            .collect();
        let mut fetches = futures_util::stream::iter(files.into_iter().map(|(path, file_hash)| {
            let blob_key = ContentAddress::blob_key(&file_hash);
            let storage = self.storage.clone();
            let cache = self.blob_cache.clone();
            async move {
                let bytes = tokio::time::timeout(
                    TEMPLATE_FILE_FETCH_TIMEOUT,
                    Self::get_immutable_from(&storage, &cache, &blob_key),
                )
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
                Ok::<_, RegistryError>((path, file_hash, bytes))
            }
        }))
        .buffer_unordered(FILE_FETCH_CONCURRENCY);

        let mut fetched: Vec<(String, String, Vec<u8>)> = Vec::with_capacity(manifest.files.len());
        while let Some(result) = fetches.next().await {
            fetched.push(result?);
        }
        // Build the file system in a deterministic order (fetches complete out of
        // order) so font registration order is stable across renders.
        fetched.sort_by(|a, b| a.0.cmp(&b.0));

        for (path, file_hash, bytes) in fetched {
            total_bytes += bytes.len();

            // Register font assets (parsed once per content hash, cached).
            if is_font_path(&path) {
                let faces = {
                    let mut cache = TEMPLATE_FONT_CACHE.lock().unwrap();
                    if let Some(faces) = cache.get(&file_hash) {
                        faces.clone()
                    } else {
                        let parsed = Arc::new(papermake::load_font_faces(&bytes));
                        if cache.len() >= TEMPLATE_FONT_CACHE_CAP {
                            cache.clear();
                        }
                        cache.insert(file_hash.clone(), parsed.clone());
                        parsed
                    }
                };
                fonts.extend(faces.iter().cloned());
            }

            // Typst asks for rooted virtual paths (e.g. `/assets/logo.svg`),
            // while manifests store bundle paths without a leading slash.
            file_system.add_file(&path, bytes.clone());
            if let Some(unrooted_path) = path.strip_prefix('/') {
                file_system.add_file(unrooted_path, bytes);
            } else {
                file_system.add_file(format!("/{path}"), bytes);
            }
        }

        tracing::debug!(
            reference = %reference,
            files = manifest.files.len(),
            total_bytes,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "registry render hydrated template file system",
        );

        Ok((Arc::new(file_system), fonts))
    }

    /// Reject a render immediately if every slot is currently held by a
    /// timed-out render that could not be cancelled.
    ///
    /// A timed-out Typst compile keeps running on the blocking pool and keeps
    /// its semaphore permit until it finishes on its own (see
    /// [`Self::track_leaked_render`]). Without this guard, once enough
    /// non-terminating templates pile up every later render would block on the
    /// semaphore and then time out too. Failing fast turns a slow, repeated
    /// timeout into an immediate, observable "capacity exhausted" signal.
    pub(crate) fn check_render_capacity(&self) -> Result<(), RegistryError> {
        let leaked = self.leaked_renders.load(Ordering::Relaxed);
        if leaked >= self.max_concurrent_renders {
            tracing::error!(
                leaked,
                max_concurrent_renders = self.max_concurrent_renders,
                "render pool exhausted by timed-out renders; rejecting new render",
            );
            return Err(RegistryError::RenderPoolExhausted {
                max: self.max_concurrent_renders,
            });
        }
        Ok(())
    }

    /// Account for a render task that timed out but is still running.
    ///
    /// `spawn_blocking` tasks cannot be cancelled, so a timed-out compile keeps
    /// running and holds its render slot (the permit is moved into the closure)
    /// until it returns on its own. We count it as leaked and watch the handle
    /// so the count drops back once the task actually finishes — a slow but
    /// terminating render self-heals, while a genuinely non-terminating one
    /// stays counted and eventually trips [`Self::check_render_capacity`].
    pub(crate) fn track_leaked_render<T: Send + 'static>(
        &self,
        handle: tokio::task::JoinHandle<T>,
    ) {
        let leaked = self.leaked_renders.clone();
        leaked.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            let _ = handle.await;
            leaked.fetch_sub(1, Ordering::Relaxed);
        });
    }

    /// Run the CPU-bound Typst compile/PDF generation on Tokio's blocking pool.
    ///
    /// S3/template hydration is already complete before this is called, so the
    /// blocking task only performs synchronous Typst work over in-memory data.
    pub(crate) async fn render_typst_blocking(
        &self,
        reference: &str,
        entrypoint_content: String,
        file_system: Arc<dyn papermake::RenderFileSystem>,
        fonts: Vec<Font>,
        data: serde_json::Value,
        options: RenderOptions,
    ) -> Result<papermake::RenderResult, RegistryError> {
        let started = std::time::Instant::now();
        let timeout = self.render_timeout;
        let timeout_seconds = timeout.as_secs().max(1);

        self.check_render_capacity()?;

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

        let mut task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let task_started = std::time::Instant::now();
            tracing::debug!(
                reference = %reference_for_task,
                "typst blocking render started",
            );

            let result = papermake::render_template_with_fonts_and_options(
                entrypoint_content,
                file_system,
                &data,
                fonts,
                &options,
            );

            match &result {
                Ok(render_result) => {
                    tracing::debug!(
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

        match tokio::time::timeout(remaining_timeout, &mut task).await {
            Ok(join_result) => join_result
                .map_err(|e| {
                    RegistryError::Template(crate::error::TemplateError::invalid(format!(
                        "Render task failed: {}",
                        e
                    )))
                })?
                .map_err(RegistryError::Compilation),
            Err(_) => {
                tracing::error!(
                    reference = %reference,
                    timeout_seconds,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "typst blocking render timed out; task detached and still holds a render slot",
                );
                self.track_leaked_render(task);
                Err(RegistryError::RenderTimeout {
                    seconds: timeout_seconds,
                })
            }
        }
    }

    /// Run cached batch rendering on the blocking pool, returning the warmed
    /// world so the next batch item can continue reusing it.
    pub(crate) async fn render_typst_cached_blocking(
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

        self.check_render_capacity()?;

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
        let options = ctx.options.clone();

        tracing::debug!(
            reference = %reference,
            waited_ms,
            remaining_timeout_ms = remaining_timeout.as_millis() as u64,
            "cached typst render acquired blocking permit",
        );

        let mut task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let task_started = std::time::Instant::now();
            tracing::debug!(
                reference = %reference,
                "cached typst blocking render started",
            );

            let result = papermake::render_template_with_cache_and_options(
                entrypoint_content,
                file_system,
                data,
                Some(&mut world),
                &options,
            );

            match &result {
                Ok(render_result) => {
                    tracing::debug!(
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

        match tokio::time::timeout(remaining_timeout, &mut task).await {
            Ok(join_result) => join_result.map_err(|e| {
                RegistryError::Template(crate::error::TemplateError::invalid(format!(
                    "Render task failed: {}",
                    e
                )))
            }),
            Err(_) => {
                tracing::error!(
                    reference = %ctx.reference,
                    timeout_seconds,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "cached typst blocking render timed out; task detached and still holds a render slot",
                );
                self.track_leaked_render(task);
                Err(RegistryError::RenderTimeout {
                    seconds: timeout_seconds,
                })
            }
        }
    }
}
