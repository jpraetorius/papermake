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
