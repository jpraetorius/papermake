use super::*;

impl<S: BlobStorage + 'static, R: RenderStorage + 'static> Registry<S, R> {
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
        let ctx = self
            .prepare_batch(reference, retain_override, RenderOptions::default())
            .await?;
        let mut world = Some(ctx.build_world());
        let mut render_ids = Vec::with_capacity(inputs.len());
        for data in inputs {
            let (render_id, _success) = self.render_one(&ctx, &mut world, data).await?;
            render_ids.push(render_id);
        }
        Ok(render_ids)
    }

    /// Enqueue a batch job: write immutable job metadata and split the inputs
    /// into fixed-size shards, each with its own `inputs.json` and a `pending`
    /// shard descriptor. Returns the job metadata (for its `job_id`). Workers
    /// pick up shards via [`Registry::claim_next_shard`]. Does no rendering.
    pub async fn enqueue_batch_job(
        &self,
        reference: &str,
        inputs: &[BatchInput],
        retain_override: Option<u32>,
        options: &RenderOptions,
    ) -> Result<BatchJob, RegistryError> {
        let job_id = uuid::Uuid::now_v7().to_string();
        let total = inputs.len();
        let shard_size = self.batch_shard_size.max(1);
        let num_shards = total.div_ceil(shard_size);
        let now = time::OffsetDateTime::now_utc();

        // Write each shard's inputs + a pending descriptor.
        for k in 0..num_shards {
            let start = k * shard_size;
            let len = (total - start).min(shard_size);
            let inputs_bytes = serde_json::to_vec(&inputs[start..start + len])?;
            self.storage
                .put(&layout::shard_inputs_key(&job_id, k), inputs_bytes)
                .await
                .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;
            self.put_shard(&Shard {
                job_id: job_id.clone(),
                index: k,
                start,
                len,
                status: ShardStatus::Pending,
                owner: None,
                lease_expires_at: None,
                done: 0,
                failed: 0,
                attempts: 0,
                updated_at: now,
            })
            .await?;
        }

        let job = BatchJob {
            job_id,
            reference: reference.to_string(),
            total,
            retain_days: retain_override,
            pdf_standards: options.pdf_standards.clone(),
            shard_size,
            num_shards,
            created_at: now,
        };
        let bytes = serde_json::to_vec(&job)?;
        self.storage
            .put(&layout::job_key(&job.job_id), bytes)
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;

        // Publish pending-shard markers last, once job.json, descriptors, and
        // inputs all exist. A worker claims work by listing these markers, so it
        // never observes a shard whose job metadata or inputs aren't written yet.
        for k in 0..num_shards {
            self.storage
                .put(&layout::pending_key(&job.job_id, k), Vec::new())
                .await
                .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;
        }

        tracing::info!(
            job_id = %job.job_id,
            reference = %reference,
            total,
            num_shards,
            "batch job enqueued",
        );
        Ok(job)
    }

    /// Claim the next processable **shard** (across all jobs) for `worker_id`,
    /// or `None`. A shard is claimable when `Pending`, or `Running` with an
    /// expired lease (its owner is gone). Claiming bumps `attempts`, sets owner +
    /// a fresh lease, persists, then re-reads to confirm ownership. Shards are
    /// tried in a per-worker pseudo-random order so workers spread out.
    ///
    /// No compare-and-set is needed: a lost claim race is harmless because render
    /// output is content-addressed (idempotent). Returns the job metadata, the
    /// claimed shard, and that shard's inputs.
    pub async fn claim_next_shard(
        &self,
        worker_id: &str,
        lease_ttl_secs: u64,
        max_attempts: u32,
        now: time::OffsetDateTime,
    ) -> Result<Option<(BatchJob, Shard, Vec<BatchInput>)>, RegistryError> {
        // List only outstanding shards via their pending markers, so the cost
        // scales with pending work rather than with all job history.
        let mut markers: Vec<String> = self
            .storage
            .list_keys(layout::PENDING_PREFIX)
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;

        // Spread workers across shards: order candidates by a per-worker hash.
        markers.sort_by_key(|k| Self::spread_hash(worker_id, k));

        for marker in markers {
            let Some((job_id, index)) = layout::parse_pending_key(&marker) else {
                continue;
            };
            let key = layout::shard_key(&job_id, index);
            let Ok(bytes) = self.storage.get(&key).await else {
                // The shard (or its whole job) is gone; drop the stale marker.
                let _ = self.storage.delete(&marker).await;
                continue;
            };
            let Ok(mut shard) = serde_json::from_slice::<Shard>(&bytes) else {
                continue;
            };

            let claimable = match shard.status {
                ShardStatus::Pending => true,
                ShardStatus::Running => shard.lease_expires_at.is_none_or(|exp| exp < now),
                ShardStatus::Done | ShardStatus::Failed => {
                    // Terminal: the marker is stale (e.g. this worker crashed
                    // between finishing and clearing it). Clean it up.
                    let _ = self.storage.delete(&marker).await;
                    continue;
                }
            };
            if !claimable {
                continue;
            }

            // Poison guard: give up on a shard after too many claims.
            if shard.attempts >= max_attempts {
                shard.status = ShardStatus::Failed;
                shard.owner = None;
                shard.lease_expires_at = None;
                shard.updated_at = now;
                let _ = self.put_shard(&shard).await;
                let _ = self.storage.delete(&marker).await;
                continue;
            }

            // Optimistic claim, then read-back to confirm we still own it.
            shard.status = ShardStatus::Running;
            shard.owner = Some(worker_id.to_string());
            shard.lease_expires_at = Some(now + time::Duration::seconds(lease_ttl_secs as i64));
            shard.attempts += 1;
            shard.updated_at = now;
            self.put_shard(&shard).await?;
            if let Ok(rb) = self.storage.get(&key).await
                && let Ok(current) = serde_json::from_slice::<Shard>(&rb)
                && current.owner.as_deref() != Some(worker_id)
            {
                // A later write landed after ours — that worker owns it now.
                continue;
            }

            let meta = self.get_job_meta(&shard.job_id).await?;
            let inputs = self.load_shard_inputs(&shard.job_id, shard.index).await?;
            return Ok(Some((meta, shard, inputs)));
        }
        Ok(None)
    }

    /// Read a shard descriptor from storage.
    pub(crate) async fn get_shard(
        &self,
        job_id: &str,
        index: usize,
    ) -> Result<Shard, RegistryError> {
        let bytes = self
            .storage
            .get(&layout::shard_key(job_id, index))
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| RegistryError::RenderStorage(RenderStorageError::Serialization(e)))
    }

    /// Renew a running shard's lease as a heartbeat, first confirming we still
    /// own it.
    ///
    /// Returns `false` when another worker has reclaimed the shard (its `owner`
    /// in storage no longer matches ours) — the caller must abandon its run so
    /// the shard is not rendered to completion twice. Returns `true` after a
    /// successful renewal.
    ///
    /// A storage read error is treated as "still ours" (fail open): dropping a
    /// healthy run on a transient blip is worse than the rare double-render the
    /// content-addressed design already tolerates. On that error we keep running
    /// but *skip* the renewal write, so a blip can never clobber the descriptor
    /// of a worker that legitimately reclaimed the shard. A single missed beat
    /// is harmless — heartbeats run well inside the lease TTL — and only a
    /// sustained read outage lets the lease lapse, which is the correct outcome.
    pub(crate) async fn heartbeat_shard(
        &self,
        shard: &mut Shard,
        lease_ttl_secs: u64,
        now: time::OffsetDateTime,
    ) -> bool {
        match self.get_shard(&shard.job_id, shard.index).await {
            Ok(current) if current.owner != shard.owner => return false,
            Ok(_) => {}
            Err(_) => return true,
        }
        shard.lease_expires_at = Some(now + time::Duration::seconds(lease_ttl_secs as i64));
        shard.updated_at = now;
        let _ = self.put_shard(shard).await;
        true
    }

    /// Persist a shard descriptor.
    pub(crate) async fn put_shard(&self, shard: &Shard) -> Result<(), RegistryError> {
        let bytes = serde_json::to_vec(shard)
            .map_err(|e| RegistryError::RenderStorage(RenderStorageError::Serialization(e)))?;
        self.storage
            .put(&layout::shard_key(&shard.job_id, shard.index), bytes)
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))
    }

    /// Read a job's immutable metadata document.
    pub(crate) async fn get_job_meta(&self, job_id: &str) -> Result<BatchJob, RegistryError> {
        let bytes = self
            .storage
            .get(&layout::job_key(job_id))
            .await
            .map_err(|e| Self::map_blob_not_found(e, job_id))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| RegistryError::RenderStorage(RenderStorageError::Serialization(e)))
    }

    /// Load one shard's slice of the batch inputs.
    pub(crate) async fn load_shard_inputs(
        &self,
        job_id: &str,
        shard_index: usize,
    ) -> Result<Vec<BatchInput>, RegistryError> {
        let bytes = self
            .storage
            .get(&layout::shard_inputs_key(job_id, shard_index))
            .await
            .map_err(|e| Self::map_blob_not_found(e, job_id))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| RegistryError::RenderStorage(RenderStorageError::Serialization(e)))
    }

    /// Stable per-(worker, key) hash used to spread workers across shards.
    pub(crate) fn spread_hash(worker_id: &str, key: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        worker_id.hash(&mut h);
        key.hash(&mut h);
        h.finish()
    }

    /// Run a claimed shard to completion, **resuming** items whose (content-
    /// addressed) output already exists. Renders each item with a warm world,
    /// heartbeats the lease, then writes the shard's `results.json` and marks it
    /// `done`. Safe to run concurrently with another worker on the same shard:
    /// identical outputs, deduped analytics.
    /// `should_stop` is polled between items so a graceful shutdown can release
    /// a long shard mid-flight instead of running all its items past the process
    /// grace period; already-rendered items are content-addressed and skipped on
    /// resume.
    pub async fn run_claimed_shard(
        &self,
        meta: BatchJob,
        mut shard: Shard,
        inputs: Vec<BatchInput>,
        lease_ttl_secs: u64,
        should_stop: impl Fn() -> bool,
    ) -> Result<(), RegistryError> {
        // Renew the lease on a time schedule (roughly half the TTL) rather than
        // every N items: a shard of slow items would otherwise let its lease
        // expire and get reclaimed, while a shard of fast items would rewrite the
        // descriptor needlessly.
        let heartbeat_interval = std::time::Duration::from_secs((lease_ttl_secs / 2).max(1));
        let mut last_heartbeat = std::time::Instant::now();

        let options = RenderOptions {
            pdf_standards: meta.pdf_standards.clone(),
        };
        let ctx = match self
            .prepare_batch(&meta.reference, meta.retain_days, options)
            .await
        {
            Ok(ctx) => ctx,
            Err(e) => {
                // A prepare error is often transient (e.g. an S3 hiccup loading
                // the manifest or entrypoint). Marking the shard Failed here
                // would bypass the max_attempts poison guard in
                // claim_next_shard and terminally fail the whole shard on the
                // first blip. Release it back to claimable instead and let
                // repeated failures trip that guard. `attempts` (bumped at claim
                // time) is preserved, so a genuinely broken template still gets
                // poisoned after max_attempts.
                shard.status = ShardStatus::Pending;
                shard.owner = None;
                shard.lease_expires_at = None;
                shard.updated_at = time::OffsetDateTime::now_utc();
                self.put_shard(&shard).await?;
                return Err(e);
            }
        };
        let mut world = Some(ctx.build_world());

        // Only a reclaim (attempt > 1) can have partial prior output worth
        // resuming; a first attempt renders every item without probing storage.
        let is_resume = shard.attempts > 1;

        let mut items: Vec<BatchItem> = Vec::with_capacity(inputs.len());
        let mut done = 0usize;
        let mut failed = 0usize;
        for (j, input) in inputs.iter().enumerate() {
            // Graceful shutdown mid-shard: persist progress counts, release the
            // shard for reclaim (its pending marker stays), and stop. Completed
            // items already wrote their content-addressed output, so a resuming
            // worker skips them.
            if should_stop() {
                shard.done = done;
                shard.failed = failed;
                shard.status = ShardStatus::Pending;
                shard.owner = None;
                shard.lease_expires_at = None;
                shard.updated_at = time::OffsetDateTime::now_utc();
                self.put_shard(&shard).await?;
                tracing::info!(
                    job_id = %meta.job_id,
                    shard = shard.index,
                    done,
                    failed,
                    "shard released on shutdown; will resume on reclaim",
                );
                return Ok(());
            }

            let global_index = shard.start + j;
            // Compute the item's content-addressed id to check for an existing
            // output (resume) — matches what `render_one` would produce.
            let data_bytes = serde_json::to_vec(&input.data)?;
            let data_hash = ContentAddress::hash(&data_bytes);
            let det_id = ContentAddress::content_render_id_with_options(
                &ctx.manifest_hash,
                &data_hash,
                &render_options_tag(&ctx.options),
            );

            // Resume skips an item only if a prior attempt *succeeded*. A
            // persisted `success=false` meta (a timeout or S3 hiccup) is retried
            // rather than counted terminally failed: the id is content-addressed,
            // so re-rendering overwrites the same keys and is harmless.
            //
            // On a shard's first attempt there is nothing of ours to resume, so
            // skip the per-item existence probe entirely (only reclaims pay it).
            let already_succeeded = is_resume
                && self
                    .storage
                    .exists(&ContentAddress::render_meta_key(&det_id))
                    .await
                    .unwrap_or(false)
                && self.read_meta_success(&det_id).await.unwrap_or(false);

            let (render_id, success) = if already_succeeded {
                (det_id, true)
            } else {
                match self.render_one(&ctx, &mut world, &input.data).await {
                    Ok((rid, ok)) => (rid, ok),
                    // Storage error persisting this item — count it failed, keep going.
                    Err(_) => (det_id, false),
                }
            };

            if success {
                done += 1;
            } else {
                failed += 1;
            }
            items.push(BatchItem {
                index: global_index,
                key: input.key.clone(),
                render_id: Some(render_id),
                status: if success {
                    ItemStatus::Success
                } else {
                    ItemStatus::Failed
                },
            });

            if last_heartbeat.elapsed() >= heartbeat_interval {
                shard.done = done;
                shard.failed = failed;
                let now = time::OffsetDateTime::now_utc();
                if !self.heartbeat_shard(&mut shard, lease_ttl_secs, now).await {
                    tracing::warn!(
                        job_id = %meta.job_id,
                        shard = shard.index,
                        "shard reclaimed by another worker mid-run; abandoning",
                    );
                    return Ok(());
                }
                last_heartbeat = std::time::Instant::now();
            }
        }

        // Persist per-item results, then mark the shard done.
        let results_bytes = serde_json::to_vec(&items)?;
        self.storage
            .put(
                &layout::shard_results_key(&meta.job_id, shard.index),
                results_bytes,
            )
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;
        shard.done = done;
        shard.failed = failed;
        shard.status = ShardStatus::Done;
        shard.owner = None;
        shard.lease_expires_at = None;
        shard.updated_at = time::OffsetDateTime::now_utc();
        self.put_shard(&shard).await?;

        // The shard no longer needs work: drop its pending marker so future
        // polls skip it.
        let _ = self
            .storage
            .delete(&layout::pending_key(&meta.job_id, shard.index))
            .await;

        tracing::info!(
            job_id = %meta.job_id,
            shard = shard.index,
            done,
            failed,
            "batch shard completed",
        );
        Ok(())
    }

    /// Read a render's success flag from its persisted `meta.json`.
    pub(crate) async fn read_meta_success(&self, render_id: &str) -> Result<bool, RegistryError> {
        let bytes = self
            .storage
            .get(&ContentAddress::render_meta_key(render_id))
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;
        let rec: RenderRecord = serde_json::from_slice(&bytes)
            .map_err(|e| RegistryError::RenderStorage(RenderStorageError::Serialization(e)))?;
        Ok(rec.success)
    }

    /// Aggregated, read-time view of a batch job (status + counts derived from
    /// its shard descriptors). Not-found → error.
    pub async fn get_batch_job(&self, job_id: &str) -> Result<JobView, RegistryError> {
        let meta = self.get_job_meta(job_id).await?;
        let shards = self.list_job_shards(job_id).await?;
        Ok(JobView::aggregate(&meta, &shards))
    }

    /// List a job's shard descriptors, ordered by shard index.
    pub(crate) async fn list_job_shards(&self, job_id: &str) -> Result<Vec<Shard>, RegistryError> {
        let prefix = format!("{}shards/", layout::job_prefix(job_id));
        let keys = self
            .storage
            .list_keys(&prefix)
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;
        let mut shards = Vec::new();
        for key in keys.iter().filter(|k| k.ends_with("/shard.json")) {
            if let Ok(bytes) = self.storage.get(key).await
                && let Ok(shard) = serde_json::from_slice::<Shard>(&bytes)
            {
                shards.push(shard);
            }
        }
        shards.sort_by_key(|s| s.index);
        Ok(shards)
    }

    /// Page of the job's item→render_id results, ordered by global item index.
    /// Only items in completed shards appear (others are simply not present yet).
    pub async fn list_job_items(
        &self,
        job_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<BatchItem>, RegistryError> {
        let meta = self.get_job_meta(job_id).await?;
        if limit == 0 || offset >= meta.total {
            return Ok(Vec::new());
        }
        let end = offset.saturating_add(limit).min(meta.total);
        if end <= offset {
            return Ok(Vec::new());
        }
        let shard_size = meta.shard_size.max(1);
        let mut out = Vec::new();
        for k in (offset / shard_size)..=((end - 1) / shard_size) {
            for it in self.load_shard_results(job_id, k).await? {
                if it.index >= offset && it.index < end {
                    out.push(it);
                }
            }
        }
        out.sort_by_key(|i| i.index);
        Ok(out)
    }

    /// Load a shard's persisted per-item results (`results.json`), if present.
    pub async fn load_shard_results(
        &self,
        job_id: &str,
        shard_index: usize,
    ) -> Result<Vec<BatchItem>, RegistryError> {
        let key = layout::shard_results_key(job_id, shard_index);
        // Absent until the shard completes — treat as no results yet.
        if !self
            .storage
            .exists(&key)
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?
        {
            return Ok(Vec::new());
        }
        let bytes = self
            .storage
            .get(&key)
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| RegistryError::RenderStorage(RenderStorageError::Serialization(e)))
    }

    /// Resolve + load a template and build one warm world, reused for a batch.
    pub(crate) async fn prepare_batch(
        &self,
        reference: &str,
        retain_override: Option<u32>,
        options: RenderOptions,
    ) -> Result<BatchCtx, RegistryError> {
        let parsed_ref = Reference::parse(reference)?;
        let template_name = parsed_ref.name.clone();
        let template_tag = parsed_ref.tag.unwrap_or_else(|| "latest".to_string());

        let LoadedTemplate {
            manifest_hash,
            manifest,
            entrypoint_content,
            file_system,
            fonts,
        } = self.load_template(reference).await?;

        let retain_days = crate::render_storage::retention::effective_retain_days(
            retain_override,
            manifest.metadata.retain_days,
            self.default_retention_days,
        );

        Ok(BatchCtx {
            reference: reference.to_string(),
            template_name,
            template_tag,
            manifest_hash,
            retain_days,
            entrypoint_content,
            file_system,
            fonts,
            options,
        })
    }

    /// Render a single input against the warm world and persist its artifacts +
    /// analytics record (like `render_and_store`). Returns `(render_id, success)`.
    pub(crate) async fn render_one(
        &self,
        ctx: &BatchCtx,
        world: &mut Option<papermake::PapermakeWorld>,
        data: &serde_json::Value,
    ) -> Result<(String, bool), RegistryError> {
        let data_bytes = serde_json::to_vec(data)?;
        let data_hash = ContentAddress::hash(&data_bytes);
        // Content-addressed id: identical (template version, data, options) =>
        // same id, so re-processing an item is idempotent (overwrites the same
        // keys). The options tag keeps PDF/A output distinct from plain PDF.
        let render_id = ContentAddress::content_render_id_with_options(
            &ctx.manifest_hash,
            &data_hash,
            &render_options_tag(&ctx.options),
        );
        self.storage
            .put(&ContentAddress::render_data_key(&render_id), data_bytes)
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;

        let start_time = std::time::Instant::now();
        // Take the warm world to hand ownership to the blocking task. On success
        // it is returned and put back; on error it was moved into the failed (or
        // timed-out, un-cancellable) task and is gone, so rebuild a fresh
        // fonts-aware world. Rebuilding matters: a world lacking the template's
        // bundled fonts would silently render later items with fallback fonts.
        let owned_world = world.take().unwrap_or_else(|| ctx.build_world());
        let outcome: Result<papermake::RenderResult, String> = match self
            .render_typst_cached_blocking(ctx, owned_world, data.clone())
            .await
        {
            Ok((updated_world, outcome)) => {
                *world = Some(updated_world);
                outcome.map_err(|e| e.to_string())
            }
            Err(e) => {
                *world = Some(ctx.build_world());
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
                let record = RenderRecord::from_outcome(
                    render_id.clone(),
                    timestamp,
                    ctx.reference.clone(),
                    ctx.template_name.clone(),
                    ctx.template_tag.clone(),
                    ctx.manifest_hash.clone(),
                    data_hash,
                    duration_ms,
                    expiry_date,
                    Ok((pdf_hash, pdf_size_bytes)),
                );
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
                let record = RenderRecord::from_outcome(
                    render_id.clone(),
                    timestamp,
                    ctx.reference.clone(),
                    ctx.template_name.clone(),
                    ctx.template_tag.clone(),
                    ctx.manifest_hash.clone(),
                    data_hash,
                    duration_ms,
                    expiry_date,
                    Err(error),
                );
                (record, false)
            }
        };

        self.put_render_meta(&record).await?;
        self.render_storage.store_render(record).await?;
        Ok((render_id, success))
    }
}
