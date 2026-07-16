use super::*;

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
        // Step 0: Validate the name/tag at the library boundary so no caller can
        // write refs with '/', '..', uppercase, or over-length segments that
        // resolve() could never find (or that a filesystem backend could turn
        // into path traversal).
        Reference::validate_ref_parts(namespace, tag)?;

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

        // Keep this instance's ref cache correct after a republish.
        self.ref_cache
            .insert(ref_key.clone(), manifest_hash.clone())
            .await;

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
        // Reject unsafe name/tag before constructing any key (see `publish`).
        Reference::validate_ref_parts(name, tag)?;

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

        // Drop any cached resolution for the deleted tag.
        self.ref_cache.invalidate(&ref_key).await;

        self.gc_orphaned(&manifest_hash).await
    }

    /// Garbage-collect a manifest (and its asset blobs) that a just-deleted ref
    /// may have orphaned — keeping anything still referenced by a surviving tag.
    pub(crate) async fn gc_orphaned(
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
    /// 1. Parses the reference string (`namespace/name:tag[@hash]`)
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

        // Step 3: Look up the manifest hash — from the short-TTL ref cache when
        // fresh, otherwise from storage (then cache it).
        let manifest_hash = if let Some(hash) = self.ref_cache.get(&ref_key).await {
            hash
        } else {
            let manifest_hash_bytes = self.storage.get(&ref_key).await.map_err(|e| match e {
                crate::storage::blob_storage::StorageError::NotFound(_) => {
                    RegistryError::Template(crate::error::TemplateError::not_found(reference))
                }
                _ => RegistryError::Storage(StorageError::backend(e.to_string())),
            })?;
            let hash = String::from_utf8(manifest_hash_bytes).map_err(|e| {
                RegistryError::Storage(StorageError::backend(format!(
                    "Invalid UTF-8 in stored manifest hash: {}",
                    e
                )))
            })?;
            self.ref_cache.insert(ref_key.clone(), hash.clone()).await;
            hash
        };

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

    /// Fetch an immutable content-addressed object (`blobs/…` or `manifests/…`),
    /// serving it from the in-process cache when present. The content at these
    /// keys never changes, so a cached copy is always valid; this collapses the
    /// per-render manifest/entrypoint/asset re-downloads into one fetch each,
    /// then cache hits.
    pub(crate) async fn get_immutable(
        &self,
        key: &str,
    ) -> Result<Vec<u8>, crate::storage::blob_storage::StorageError> {
        Self::get_immutable_from(&self.storage, &self.blob_cache, key).await
    }

    /// Cache-backed immutable fetch over owned handles (an `Arc<S>` and the
    /// cache), so callers that fan out inside `tokio::spawn` can hold owned
    /// clones instead of borrowing `&self` (which would break `Send`).
    pub(crate) async fn get_immutable_from(
        storage: &Arc<S>,
        cache: &moka::future::Cache<String, Arc<Vec<u8>>>,
        key: &str,
    ) -> Result<Vec<u8>, crate::storage::blob_storage::StorageError> {
        if let Some(hit) = cache.get(key).await {
            return Ok(hit.as_ref().clone());
        }
        let bytes = Arc::new(storage.get(key).await?);
        cache.insert(key.to_string(), bytes.clone()).await;
        Ok(bytes.as_ref().clone())
    }

    /// Load and parse a manifest by hash (cached, immutable).
    pub(crate) async fn load_manifest(
        &self,
        manifest_hash: &str,
    ) -> Result<Manifest, RegistryError> {
        let manifest_key = ContentAddress::manifest_key(manifest_hash);
        let bytes = self.get_immutable(&manifest_key).await.map_err(|e| {
            RegistryError::Storage(StorageError::backend(format!(
                "Failed to load manifest {}: {}",
                manifest_hash, e
            )))
        })?;
        Manifest::from_bytes(&bytes).map_err(|e| {
            RegistryError::ContentAddressing(crate::error::ContentAddressingError::manifest_error(
                e.to_string(),
            ))
        })
    }

    /// Load a manifest's entrypoint (`main.typ`) source as UTF-8 (cached blob).
    pub(crate) async fn load_entrypoint(
        &self,
        manifest: &Manifest,
    ) -> Result<String, RegistryError> {
        let entrypoint_hash = manifest.entrypoint_hash().ok_or_else(|| {
            RegistryError::Template(crate::error::TemplateError::invalid(
                "Manifest missing entrypoint hash",
            ))
        })?;
        let bytes = self
            .get_immutable(&ContentAddress::blob_key(entrypoint_hash))
            .await
            .map_err(|e| {
                RegistryError::Storage(StorageError::backend(format!(
                    "Failed to load entrypoint file: {}",
                    e
                )))
            })?;
        String::from_utf8(bytes).map_err(|e| {
            RegistryError::Template(crate::error::TemplateError::invalid(format!(
                "Entrypoint file is not valid UTF-8: {}",
                e
            )))
        })
    }

    /// Resolve a reference and load everything needed to render it: manifest,
    /// entrypoint source, and the hydrated file system + bundled fonts. This is
    /// the single template-load path shared by synchronous and batch renders.
    pub(crate) async fn load_template(
        &self,
        reference: &str,
    ) -> Result<LoadedTemplate, RegistryError> {
        let manifest_hash = self.resolve(reference).await?;
        let manifest = self.load_manifest(&manifest_hash).await?;
        let entrypoint_content = self.load_entrypoint(&manifest).await?;
        let (file_system, fonts) = self.hydrate_file_system(reference, &manifest).await?;
        Ok(LoadedTemplate {
            manifest_hash,
            manifest,
            entrypoint_content,
            file_system,
            fonts,
        })
    }
}
