use super::*;

impl<S: BlobStorage + 'static, R: RenderStorage + 'static> Registry<S, R> {
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
                    match self.get_immutable(&manifest_key).await {
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

    /// Load a single template's info by full name (`name` or `namespace/name`),
    /// scanning only that template's refs instead of the whole `refs/` tree.
    ///
    /// Returns `None` when the template has no tags. Used by the single-template
    /// endpoints so they don't pay the full `list_templates` scan.
    pub async fn get_template_info(
        &self,
        name: &str,
    ) -> Result<Option<TemplateInfo>, RegistryError> {
        let prefix = format!("refs/{}/", name);
        let ref_keys = self
            .storage
            .list_keys(&prefix)
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;

        let mut tags: Vec<String> = Vec::new();
        let mut latest_ref_key: Option<String> = None;
        for ref_key in &ref_keys {
            if let Some((namespace_path, tag)) = Self::parse_ref_key(ref_key) {
                // Guard against prefix bleed (e.g. `refs/john/` also lists the
                // namespaced `refs/john/invoice/...`): only exact matches count.
                if namespace_path != name {
                    continue;
                }
                if tag == "latest" {
                    latest_ref_key = Some(ref_key.clone());
                }
                tags.push(tag);
            }
        }
        if tags.is_empty() {
            return Ok(None);
        }
        tags.sort();

        // Prefer "latest" for metadata, else the first tag alphabetically.
        let ref_key_to_use = latest_ref_key.unwrap_or_else(|| format!("refs/{}/{}", name, tags[0]));
        let manifest_hash_bytes = self
            .storage
            .get(&ref_key_to_use)
            .await
            .map_err(|e| RegistryError::Storage(StorageError::backend(e.to_string())))?;
        let manifest_hash = String::from_utf8(manifest_hash_bytes).map_err(|e| {
            RegistryError::Storage(StorageError::backend(format!(
                "Invalid UTF-8 in stored manifest hash: {}",
                e
            )))
        })?;
        let manifest_key = ContentAddress::manifest_key(&manifest_hash);
        let manifest_bytes = self.get_immutable(&manifest_key).await.map_err(|e| {
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

        let (namespace, template_name) = Self::parse_namespace_path(name);
        Ok(Some(TemplateInfo::new(
            template_name,
            namespace,
            tags,
            manifest_hash,
            manifest.metadata.clone(),
        )))
    }

    /// Parse a reference key to extract namespace/name path and tag
    ///
    /// Examples:
    /// - "refs/invoice/latest" -> Some(("invoice", "latest"))
    /// - "refs/john/invoice/v1.0.0" -> Some(("john/invoice", "v1.0.0"))
    /// - "invalid/key" -> None
    pub(crate) fn parse_ref_key(ref_key: &str) -> Option<(String, String)> {
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
    pub(crate) fn parse_namespace_path(namespace_path: &str) -> (Option<String>, String) {
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
}
