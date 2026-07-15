use std::sync::Arc;
use std::time::Instant;

use papermake::{FileError, RenderFileSystem};

use crate::{
    BlobStorage,
    address::ContentAddress,
    error::{RegistryError, StorageError},
    manifest::Manifest,
};

pub struct RegistryFileSystem<S: BlobStorage> {
    storage: Arc<S>,
    manifest: Manifest,
    runtime: tokio::runtime::Handle,
}

impl<S: BlobStorage> RegistryFileSystem<S> {
    pub fn new(storage: Arc<S>, manifest: Manifest) -> Result<Self, RegistryError> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
            RegistryError::Storage(StorageError::configuration(
                "No tokio runtime available for async operations",
            ))
        })?;

        Ok(Self {
            storage,
            manifest,
            runtime,
        })
    }

    fn normalize_path(&self, path: &str) -> String {
        // Remove leading slash if present
        let path = path.strip_prefix('/').unwrap_or(path);

        path.to_string()
    }
}

impl<S: BlobStorage + 'static> RenderFileSystem for RegistryFileSystem<S> {
    fn get_file(&self, path: &str) -> Result<Vec<u8>, FileError> {
        let started = Instant::now();
        let normalized_path = self.normalize_path(path);
        tracing::debug!(
            path = %path,
            normalized_path = %normalized_path,
            "typst filesystem lookup started",
        );

        let file_hash = self.manifest.files.get(&normalized_path).ok_or_else(|| {
            tracing::warn!(
                path = %path,
                normalized_path = %normalized_path,
                "typst filesystem lookup missing from manifest",
            );
            FileError::NotFound(path.into())
        })?;

        let blob_key = ContentAddress::blob_key(file_hash);
        tracing::debug!(
            path = %path,
            blob_key = %blob_key,
            "typst filesystem blob fetch started",
        );

        let storage = self.storage.clone(); // Ensure storage is cloneable or use Arc
        let blob_key = blob_key.clone();
        let handle = self.runtime.clone();

        let result = std::thread::spawn(move || handle.block_on(storage.get(&blob_key)))
            .join()
            .map_err(|_| {
                tracing::error!(
                    path = %path,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "typst filesystem blob fetch thread panicked",
                );
                FileError::NotFound(path.into())
            })?
            .map_err(|error| {
                tracing::warn!(
                    path = %path,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    error = %error,
                    "typst filesystem blob fetch failed",
                );
                FileError::NotFound(path.into())
            })?;

        tracing::debug!(
            path = %path,
            bytes = result.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "typst filesystem blob fetch completed",
        );

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use papermake::RenderFileSystem;

    use super::*;
    use crate::bundle::TemplateMetadata;
    use crate::storage::blob_storage::MemoryStorage;

    fn manifest_for(files: &[(&str, &[u8])]) -> Manifest {
        let hashes = files
            .iter()
            .map(|(path, contents)| (path.to_string(), ContentAddress::hash(contents)))
            .collect::<BTreeMap<_, _>>();

        Manifest::new(
            hashes,
            TemplateMetadata::new("Test Template", "test@example.com"),
        )
        .unwrap()
    }

    async fn populated_file_system(files: &[(&str, &[u8])]) -> RegistryFileSystem<MemoryStorage> {
        let storage = Arc::new(MemoryStorage::new());
        let manifest = manifest_for(files);

        for (path, contents) in files {
            let hash = manifest.get_file_hash(path).unwrap();
            storage
                .put(&ContentAddress::blob_key(hash), contents.to_vec())
                .await
                .unwrap();
        }

        RegistryFileSystem::new(storage, manifest).unwrap()
    }

    #[tokio::test]
    async fn get_file_reads_manifest_addressed_blob() {
        let fs =
            populated_file_system(&[("main.typ", b"= Main"), ("asset.txt", b"asset contents")])
                .await;

        assert_eq!(fs.get_file("asset.txt").unwrap(), b"asset contents");
        assert_eq!(fs.get_file("/asset.txt").unwrap(), b"asset contents");
    }

    #[tokio::test]
    async fn get_file_reports_not_found_when_manifest_has_no_path() {
        let fs = populated_file_system(&[("main.typ", b"= Main")]).await;

        assert!(matches!(
            fs.get_file("missing.txt"),
            Err(FileError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn get_file_reports_not_found_when_addressed_blob_is_missing() {
        let storage = Arc::new(MemoryStorage::new());
        let manifest = manifest_for(&[("main.typ", b"= Main"), ("asset.txt", b"asset")]);
        let main_hash = manifest.get_file_hash("main.typ").unwrap();
        storage
            .put(&ContentAddress::blob_key(main_hash), b"= Main".to_vec())
            .await
            .unwrap();
        let fs = RegistryFileSystem::new(storage, manifest).unwrap();

        assert!(matches!(
            fs.get_file("asset.txt"),
            Err(FileError::NotFound(_))
        ));
    }

    #[test]
    fn new_requires_an_active_tokio_runtime() {
        let storage = Arc::new(MemoryStorage::new());
        let manifest = manifest_for(&[("main.typ", b"= Main")]);

        let result = RegistryFileSystem::new(storage, manifest);

        assert!(matches!(
            result,
            Err(RegistryError::Storage(StorageError::Configuration { .. }))
        ));
    }
}
