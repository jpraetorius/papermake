use super::*;
use crate::{S3Storage, bundle::TemplateMetadata, storage::blob_storage::MemoryStorage};

/// Blob storage that can be toggled to fail `get` for `manifests/…` keys,
/// simulating a transient storage error while loading a template. All other
/// operations delegate to an inner in-memory store.
struct FlakyManifestStorage {
    inner: MemoryStorage,
    fail_manifest_get: std::sync::atomic::AtomicBool,
}

impl FlakyManifestStorage {
    fn new() -> Self {
        Self {
            inner: MemoryStorage::new(),
            fail_manifest_get: std::sync::atomic::AtomicBool::new(false),
        }
    }
    fn set_fail(&self, fail: bool) {
        self.fail_manifest_get.store(fail, Ordering::Relaxed);
    }
}

#[async_trait::async_trait]
impl BlobStorage for FlakyManifestStorage {
    async fn put(
        &self,
        key: &str,
        data: Vec<u8>,
    ) -> Result<(), crate::storage::blob_storage::StorageError> {
        self.inner.put(key, data).await
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>, crate::storage::blob_storage::StorageError> {
        if key.starts_with("manifests/") && self.fail_manifest_get.load(Ordering::Relaxed) {
            return Err(crate::storage::blob_storage::StorageError::Backend(
                "injected manifest get failure".to_string(),
            ));
        }
        self.inner.get(key).await
    }
    async fn exists(&self, key: &str) -> Result<bool, crate::storage::blob_storage::StorageError> {
        self.inner.exists(key).await
    }
    async fn delete(&self, key: &str) -> Result<(), crate::storage::blob_storage::StorageError> {
        self.inner.delete(key).await
    }
    async fn list_keys(
        &self,
        prefix: &str,
    ) -> Result<Vec<String>, crate::storage::blob_storage::StorageError> {
        self.inner.list_keys(prefix).await
    }
}

/// Blob storage that counts `get` calls, to prove caching avoids re-reads.
#[derive(Default)]
struct CountingStorage {
    inner: MemoryStorage,
    gets: std::sync::Mutex<usize>,
}

impl CountingStorage {
    fn total_gets(&self) -> usize {
        *self.gets.lock().unwrap()
    }
}

#[async_trait::async_trait]
impl BlobStorage for CountingStorage {
    async fn put(
        &self,
        key: &str,
        data: Vec<u8>,
    ) -> Result<(), crate::storage::blob_storage::StorageError> {
        self.inner.put(key, data).await
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>, crate::storage::blob_storage::StorageError> {
        *self.gets.lock().unwrap() += 1;
        self.inner.get(key).await
    }
    async fn exists(&self, key: &str) -> Result<bool, crate::storage::blob_storage::StorageError> {
        self.inner.exists(key).await
    }
    async fn delete(&self, key: &str) -> Result<(), crate::storage::blob_storage::StorageError> {
        self.inner.delete(key).await
    }
    async fn list_keys(
        &self,
        prefix: &str,
    ) -> Result<Vec<String>, crate::storage::blob_storage::StorageError> {
        self.inner.list_keys(prefix).await
    }
}

fn create_test_bundle() -> TemplateBundle {
    let metadata = TemplateMetadata::new("Test Template", "test@example.com");
    let main_content = br#"#let data = json(bytes(sys.inputs.data))
= Test Template
Hello #data.name"#
        .to_vec();

    TemplateBundle::new(main_content, metadata)
        .add_file("assets/logo.png", b"fake_png_data".to_vec())
        .with_schema(br#"{"type": "object", "properties": {"name": {"type": "string"}}}"#.to_vec())
}

#[test]
fn test_registry_public_constructors_preserve_render_storage_access() {
    let registry = Registry::new_with_render_storage(
        MemoryStorage::new(),
        crate::render_storage::MemoryRenderStorage::new(),
    )
    .with_render_limits(0, std::time::Duration::ZERO);

    assert!(registry.render_storage().is_some());
}

#[test]
fn render_capacity_check_trips_when_all_slots_are_leaked() {
    let registry = Registry::new_storage_only(MemoryStorage::new())
        .with_render_limits(2, std::time::Duration::from_secs(1));

    // No leaked renders: capacity is available.
    assert!(registry.check_render_capacity().is_ok());

    // One of two slots leaked: still room for another render.
    registry.leaked_renders.fetch_add(1, Ordering::Relaxed);
    assert!(registry.check_render_capacity().is_ok());

    // Both slots leaked: reject fast instead of queueing until timeout.
    registry.leaked_renders.fetch_add(1, Ordering::Relaxed);
    assert!(matches!(
        registry.check_render_capacity(),
        Err(RegistryError::RenderPoolExhausted { max: 2 })
    ));

    // A leaked render finishing frees capacity again.
    registry.leaked_renders.fetch_sub(1, Ordering::Relaxed);
    assert!(registry.check_render_capacity().is_ok());
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
async fn test_get_template_source_returns_published_entrypoint() {
    let storage = MemoryStorage::new();
    let registry = Registry::new_storage_only(storage);
    let metadata = TemplateMetadata::new("Source Template", "test@example.com");
    let main_content = br#"#let data = json(bytes(sys.inputs.data))
Hello #data.name"#
        .to_vec();
    let bundle = TemplateBundle::new(main_content.clone(), metadata);

    registry
        .publish(bundle, "source-template", "latest")
        .await
        .unwrap();

    let source = registry
        .get_template_source("source-template:latest")
        .await
        .unwrap();
    assert_eq!(source.as_bytes(), main_content.as_slice());
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
        .with_schema(br#"{"type": "object", "properties": {"name": {"type": "string"}}}"#.to_vec());

    let bundle2 = TemplateBundle::new(main_content, metadata2)
        .add_file("assets/logo.png", b"fake_png_data".to_vec())
        .with_schema(br#"{"type": "object", "properties": {"name": {"type": "string"}}}"#.to_vec());

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
async fn test_publish_and_delete_reject_unsafe_names() {
    let registry = Registry::new_storage_only(MemoryStorage::new());

    // Unsafe names/tags are rejected at the library boundary, before any key
    // is written — regardless of caller (the HTTP layer decodes %2F to '/').
    for (name, tag) in [
        ("Invoice", "latest"),
        ("../evil", "latest"),
        ("a/b/c", "latest"),
        ("ok", "../x"),
    ] {
        let published = registry.publish(create_test_bundle(), name, tag).await;
        assert!(
            matches!(published, Err(RegistryError::Reference(_))),
            "publish {name}:{tag} -> {published:?}",
        );
        let deleted = registry.delete_version(name, tag).await;
        assert!(
            matches!(deleted, Err(RegistryError::Reference(_))),
            "delete {name}:{tag} -> {deleted:?}",
        );
    }
    // No phantom refs were created.
    assert!(
        registry
            .storage
            .list_keys("refs/")
            .await
            .unwrap()
            .is_empty()
    );

    // A valid name still publishes and resolves.
    registry
        .publish(create_test_bundle(), "invoice", "latest")
        .await
        .unwrap();
    assert!(registry.resolve("invoice:latest").await.is_ok());
}

#[tokio::test]
async fn immutable_reads_are_cached_across_renders() {
    let registry = Registry::new_storage_only(CountingStorage::default());
    registry
        .publish(create_test_bundle(), "invoice", "latest")
        .await
        .unwrap();

    // Warm the caches with a first render.
    registry
        .render_and_store("invoice:latest", &serde_json::json!({ "name": "A" }))
        .await
        .unwrap();

    // A second identical render must not read any immutable object (manifest,
    // entrypoint, assets) or the tag from storage again — everything is cached.
    let before = registry.storage.total_gets();
    registry
        .render_and_store("invoice:latest", &serde_json::json!({ "name": "B" }))
        .await
        .unwrap();
    assert_eq!(registry.storage.total_gets(), before);
}

#[tokio::test]
async fn ref_cache_is_invalidated_on_publish_and_delete() {
    let registry = Registry::new_storage_only(MemoryStorage::new());
    registry
        .publish(create_test_bundle(), "invoice", "latest")
        .await
        .unwrap();
    let h1 = registry.resolve("invoice:latest").await.unwrap();

    // Republishing the same tag with different content must be visible
    // immediately, not shadowed by the cached resolution.
    let other = TemplateBundle::new(
        br#"#let data = json(bytes(sys.inputs.data))
= Changed
Bye #data.name"#
            .to_vec(),
        TemplateMetadata::new("Changed", "test@example.com"),
    );
    registry.publish(other, "invoice", "latest").await.unwrap();
    let h2 = registry.resolve("invoice:latest").await.unwrap();
    assert_ne!(h1, h2, "republish must update the cached resolution");

    // Deleting the tag must not keep serving it from cache.
    registry.delete_version("invoice", "latest").await.unwrap();
    assert!(registry.resolve("invoice:latest").await.is_err());
}

#[tokio::test]
async fn get_template_info_loads_one_template_without_prefix_bleed() {
    let registry = Registry::new_storage_only(MemoryStorage::new());
    registry
        .publish(create_test_bundle(), "invoice", "latest")
        .await
        .unwrap();
    registry
        .publish(create_test_bundle(), "invoice", "v1")
        .await
        .unwrap();
    // A different template sharing a name prefix must not leak in.
    registry
        .publish(create_test_bundle(), "invoice-2", "latest")
        .await
        .unwrap();

    let info = registry
        .get_template_info("invoice")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info.name, "invoice");
    assert_eq!(info.tags, vec!["latest".to_string(), "v1".to_string()]);

    // Unknown template → None (not an error).
    assert!(
        registry
            .get_template_info("missing")
            .await
            .unwrap()
            .is_none()
    );
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
async fn test_registry_render_with_pdf_a3b() {
    let storage = MemoryStorage::new();
    let registry = Registry::new_storage_only(storage);
    let bundle = create_test_bundle();
    registry
        .publish(bundle, "john/invoice", "latest")
        .await
        .unwrap();

    let data = serde_json::json!({ "name": "Test Customer" });
    let pdf_bytes = registry
        .render_with_options("john/invoice:latest", &data, &RenderOptions::pdf_a3b())
        .await
        .unwrap();

    assert!(pdf_bytes.starts_with(b"%PDF"));
    // PDF/A conformance is declared in the XMP metadata.
    assert!(
        pdf_bytes
            .windows(b"pdfaid".len())
            .any(|w| w == b"pdfaid".as_slice())
    );
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

    let bundle = TemplateBundle::new(main_content, metadata).add_file("header.typ", header_content);

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
    let logo =
        br##"<svg xmlns="http://www.w3.org/2000/svg" width="120" height="48" viewBox="0 0 120 48">
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

// Integration test: requires a live S3-compatible backend at localhost:9000.
// Run with `cargo test -- --ignored` after `docker compose up -d object-store`.
#[ignore = "requires a live S3 at localhost:9000 (see docker-compose.yml)"]
#[tokio::test(flavor = "multi_thread")]
async fn test_registry_list_templates_no_namespace() {
    let storage = S3Storage::from_env_values(|key| match key {
        "S3_ENDPOINT_URL" => Ok("http://localhost:9000".to_string()),
        "S3_ACCESS_KEY_ID" => Ok("papermake".to_string()),
        "S3_SECRET_ACCESS_KEY" => Ok("papermake-secret".to_string()),
        "S3_BUCKET" => Ok("papermake-registry-test".to_string()),
        _ => Err(std::env::VarError::NotPresent),
    })
    .unwrap();
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
        Registry::<MemoryStorage, crate::render_storage::MemoryRenderStorage>::parse_namespace_path(
            "invoice"
        ),
        (None, "invoice".to_string())
    );

    assert_eq!(
        Registry::<MemoryStorage, crate::render_storage::MemoryRenderStorage>::parse_namespace_path(
            "john/invoice"
        ),
        (Some("john".to_string()), "invoice".to_string())
    );

    assert_eq!(
        Registry::<MemoryStorage, crate::render_storage::MemoryRenderStorage>::parse_namespace_path(
            "org/user/template"
        ),
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
    assert_eq!(result.pdf_hash, ContentAddress::hash(&result.pdf_bytes));

    // Verify render record was stored
    let records = registry.list_recent_renders(10).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].render_id, result.render_id);
    assert_eq!(records[0].duration_ms, result.duration_ms);
    assert_eq!(records[0].template_name, "test-template");
    assert_eq!(records[0].template_tag, "latest");
    assert!(records[0].success);
}

#[tokio::test]
async fn test_render_and_store_with_options_updates_public_render_queries() {
    let storage = MemoryStorage::new();
    let render_storage = crate::render_storage::MemoryRenderStorage::new();
    let registry = Registry::new(storage, render_storage);
    registry
        .publish(create_test_bundle(), "test-user/test-template", "latest")
        .await
        .unwrap();

    let result = registry
        .render_and_store_with_options(
            "test-user/test-template:latest",
            &serde_json::json!({ "name": "Test User" }),
            &RenderOptions::default(),
        )
        .await
        .unwrap();

    assert!(result.pdf_bytes.starts_with(b"%PDF"));
    let renders = registry
        .list_template_renders("test-template", 10)
        .await
        .unwrap();
    assert_eq!(renders.len(), 1);
    assert_eq!(renders[0].render_id, result.render_id);

    let summary = registry.render_summary().await.unwrap();
    assert_eq!(summary.recent.len(), 1);
    assert_eq!(summary.recent[0].render_id, result.render_id);
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
    assert_eq!(result.pdf_hash, ContentAddress::hash(&result.pdf_bytes));

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
        assert!(duration_points.iter().all(|p| {
            p.avg_duration_ms >= 0.0
                && p.p90_duration_ms <= p.p95_duration_ms
                && p.p95_duration_ms <= p.p99_duration_ms
        }));
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

    // Content-addressed: identical (template version, data) => SAME render_id
    // (idempotent dedupe), and identical content hashes.
    assert_eq!(result1.render_id, result2.render_id);
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
    let metadata = TemplateMetadata::new("Test Template", "test@example.com").with_retain_days(7);
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

#[test]
fn test_is_font_path() {
    assert!(is_font_path("Ubuntu-R.ttf"));
    assert!(is_font_path("fonts/Custom.OTF"));
    assert!(is_font_path("a/b/collection.ttc"));
    assert!(!is_font_path("assets/logo.png"));
    assert!(!is_font_path("main.typ"));
    assert!(!is_font_path("notes.ttf.txt"));
}

#[tokio::test]
async fn test_batch_job_enqueue_claim_run() {
    use crate::batch::{BatchInput, ItemStatus, JobStatus, ShardStatus};

    let storage = MemoryStorage::new();
    let render_storage = crate::render_storage::MemoryRenderStorage::new();
    // shard_size 1 => two shards for two inputs, exercising multi-shard drain.
    let registry = Registry::new(storage, render_storage).with_shard_size(1);
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

    // Server enqueues: metadata + two pending shards; nothing claimed yet.
    let job = registry
        .enqueue_batch_job("batch:latest", &inputs, None, &RenderOptions::default())
        .await
        .unwrap();
    let job_id = job.job_id.clone();
    let queued = registry.get_batch_job(&job_id).await.unwrap();
    assert_eq!(queued.status, JobStatus::Queued);
    assert_eq!(queued.total, 2);
    assert_eq!(queued.num_shards, 2);
    assert_eq!(queued.done, 0);

    // One worker drains both shards (like the worker loop).
    let now = time::OffsetDateTime::now_utc();
    let mut shards_run = 0;
    while let Some((meta, shard, sinputs)) = registry
        .claim_next_shard("worker-1", 120, 3, now)
        .await
        .unwrap()
    {
        assert_eq!(shard.status, ShardStatus::Running);
        assert_eq!(shard.owner.as_deref(), Some("worker-1"));
        assert_eq!(shard.attempts, 1);
        assert_eq!(sinputs.len(), 1);
        registry
            .run_claimed_shard(meta, shard, sinputs, 120, || false)
            .await
            .unwrap();
        shards_run += 1;
    }
    assert_eq!(shards_run, 2);

    // Aggregated status is derived from the shards.
    let done = registry.get_batch_job(&job_id).await.unwrap();
    assert_eq!(done.status, JobStatus::Completed);
    assert_eq!(done.done, 2);
    assert_eq!(done.failed, 0);

    // Item→render_id map, paginated, ordered by index.
    let items = registry.list_job_items(&job_id, 0, 10).await.unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].index, 0);
    assert_eq!(items[0].key.as_deref(), Some("cust-a"));
    for item in &items {
        assert_eq!(item.status, ItemStatus::Success);
        let render_id = item.render_id.as_ref().expect("render_id set");
        let pdf = registry.get_render_pdf(render_id).await.unwrap();
        assert!(pdf.starts_with(b"%PDF"));
    }
}

#[tokio::test]
async fn test_batch_render_pdf_a_distinct_from_plain() {
    use crate::batch::BatchInput;
    use papermake::PdfStandard;

    let storage = MemoryStorage::new();
    let render_storage = crate::render_storage::MemoryRenderStorage::new();
    let registry = Registry::new(storage, render_storage);
    registry
        .publish(create_test_bundle(), "batch", "latest")
        .await
        .unwrap();

    let data = serde_json::json!({ "name": "A" });

    // Pre-render the SAME data as a plain PDF via the sync path. Without the
    // options-discriminated render id, the PDF/A batch below would resume-skip
    // onto this plain output and never actually render PDF/A.
    let plain = registry
        .render_and_store("batch:latest", &data)
        .await
        .unwrap();
    let plain_pdf = registry.get_render_pdf(&plain.render_id).await.unwrap();
    assert!(
        !contains(&plain_pdf, b"pdfaid"),
        "sync render should be plain"
    );

    // Enqueue + run a PDF/A-3b batch over the same input.
    let options = RenderOptions {
        pdf_standards: vec![PdfStandard::A3b],
    };
    let inputs = vec![BatchInput {
        data: data.clone(),
        key: None,
    }];
    let job = registry
        .enqueue_batch_job("batch:latest", &inputs, None, &options)
        .await
        .unwrap();
    assert_eq!(job.pdf_standards, vec![PdfStandard::A3b]);

    let now = time::OffsetDateTime::now_utc();
    while let Some((meta, shard, sinputs)) =
        registry.claim_next_shard("w", 120, 3, now).await.unwrap()
    {
        registry
            .run_claimed_shard(meta, shard, sinputs, 120, || false)
            .await
            .unwrap();
    }

    let items = registry.list_job_items(&job.job_id, 0, 10).await.unwrap();
    assert_eq!(items.len(), 1);
    let batch_id = items[0].render_id.as_ref().expect("render_id set");
    // The PDF/A batch got a DISTINCT id from the plain sync render...
    assert_ne!(
        batch_id, &plain.render_id,
        "PDF/A output must not collide with the plain render id"
    );
    // ...and actually produced PDF/A-conformant output.
    let batch_pdf = registry.get_render_pdf(batch_id).await.unwrap();
    assert!(batch_pdf.starts_with(b"%PDF"));
    assert!(
        contains(&batch_pdf, b"pdfaid"),
        "batch output should declare PDF/A conformance"
    );
}

/// Byte-substring search, for asserting on raw PDF bytes.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test]
async fn test_batch_shard_orphan_reclaim_and_resume() {
    use crate::batch::{BatchInput, JobStatus};

    let storage = MemoryStorage::new();
    let render_storage = crate::render_storage::MemoryRenderStorage::new();
    let registry = Registry::new(storage, render_storage); // default shard_size => 1 shard
    registry
        .publish(create_test_bundle(), "batch", "latest")
        .await
        .unwrap();

    let inputs = vec![
        BatchInput {
            data: serde_json::json!({ "name": "A" }),
            key: None,
        },
        BatchInput {
            data: serde_json::json!({ "name": "B" }),
            key: None,
        },
    ];
    let job = registry
        .enqueue_batch_job("batch:latest", &inputs, None, &RenderOptions::default())
        .await
        .unwrap();
    let job_id = job.job_id.clone();

    // Pre-render item A directly: the shard should later *resume* (skip) it
    // since its content-addressed output already exists.
    let pre = registry
        .render_and_store("batch:latest", &serde_json::json!({ "name": "A" }))
        .await
        .unwrap();

    // Worker A claims the shard but "dies" before running (lease will expire).
    let t0 = time::OffsetDateTime::now_utc();
    let (_meta_a, shard_a, _) = registry
        .claim_next_shard("worker-a", 60, 3, t0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(shard_a.attempts, 1);

    // Before the lease expires, the shard is NOT claimable by anyone else.
    let t_soon = t0 + time::Duration::seconds(10);
    assert!(
        registry
            .claim_next_shard("worker-b", 60, 3, t_soon)
            .await
            .unwrap()
            .is_none()
    );

    // After expiry, worker B reclaims (attempts=2) and runs to completion.
    let t_late = t0 + time::Duration::seconds(120);
    let (meta_b, shard_b, inputs_b) = registry
        .claim_next_shard("worker-b", 60, 3, t_late)
        .await
        .unwrap()
        .expect("expired lease is reclaimable");
    assert_eq!(shard_b.owner.as_deref(), Some("worker-b"));
    assert_eq!(shard_b.attempts, 2);
    registry
        .run_claimed_shard(meta_b, shard_b, inputs_b, 60, || false)
        .await
        .unwrap();

    let done = registry.get_batch_job(&job_id).await.unwrap();
    assert_eq!(done.status, JobStatus::Completed);
    assert_eq!(done.done, 2);
    // Item A reused the pre-existing content-addressed render (idempotent).
    let items = registry.list_job_items(&job_id, 0, 10).await.unwrap();
    assert_eq!(items[0].render_id.as_deref(), Some(pre.render_id.as_str()));
}

#[tokio::test]
async fn test_run_claimed_shard_retries_persisted_failure() {
    use crate::batch::{BatchInput, JobStatus};

    let registry = Registry::new(
        MemoryStorage::new(),
        crate::render_storage::MemoryRenderStorage::new(),
    );
    registry
        .publish(create_test_bundle(), "batch", "latest")
        .await
        .unwrap();

    let inputs = vec![BatchInput {
        data: serde_json::json!({ "name": "A" }),
        key: None,
    }];
    let job = registry
        .enqueue_batch_job("batch:latest", &inputs, None, &RenderOptions::default())
        .await
        .unwrap();
    let job_id = job.job_id.clone();

    // Simulate a prior attempt that persisted a *failure* meta for item A
    // (e.g. a timeout or S3 hiccup) at its content-addressed id.
    let manifest_hash = registry.resolve("batch:latest").await.unwrap();
    let data_bytes = serde_json::to_vec(&serde_json::json!({ "name": "A" })).unwrap();
    let data_hash = ContentAddress::hash(&data_bytes);
    let det_id = ContentAddress::content_render_id_with_options(&manifest_hash, &data_hash, "");
    let mut failed_meta = RenderRecord::failure(
        "batch:latest".to_string(),
        "batch".to_string(),
        "latest".to_string(),
        manifest_hash.clone(),
        data_hash.clone(),
        "transient failure".to_string(),
        1,
    );
    failed_meta.render_id = det_id.clone();
    registry
        .storage
        .put(
            &ContentAddress::render_meta_key(&det_id),
            serde_json::to_vec(&failed_meta).unwrap(),
        )
        .await
        .unwrap();

    // Run the shard. The persisted failure must be retried, not counted as a
    // terminal failure.
    let t0 = time::OffsetDateTime::now_utc();
    let (meta, shard, sinputs) = registry
        .claim_next_shard("worker", 60, 3, t0)
        .await
        .unwrap()
        .unwrap();
    registry
        .run_claimed_shard(meta, shard, sinputs, 60, || false)
        .await
        .unwrap();

    let done = registry.get_batch_job(&job_id).await.unwrap();
    assert_eq!(done.status, JobStatus::Completed);
    assert_eq!(done.done, 1);
    assert_eq!(done.failed, 0);
    // The failure meta was overwritten by a successful re-render.
    assert!(registry.read_meta_success(&det_id).await.unwrap());
}

#[tokio::test]
async fn test_transient_prepare_error_releases_shard_for_retry() {
    use crate::batch::{BatchInput, JobStatus, ShardStatus};

    let registry = Registry::new(
        FlakyManifestStorage::new(),
        crate::render_storage::MemoryRenderStorage::new(),
    );
    registry
        .publish(create_test_bundle(), "batch", "latest")
        .await
        .unwrap();

    let inputs = vec![BatchInput {
        data: serde_json::json!({ "name": "A" }),
        key: None,
    }];
    let job = registry
        .enqueue_batch_job("batch:latest", &inputs, None, &RenderOptions::default())
        .await
        .unwrap();
    let job_id = job.job_id.clone();

    // First attempt: prepare_batch fails on a (transient) manifest get error.
    registry.storage.set_fail(true);
    let t0 = time::OffsetDateTime::now_utc();
    let (meta, shard, sinputs) = registry
        .claim_next_shard("w1", 60, 3, t0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(shard.attempts, 1);
    assert!(
        registry
            .run_claimed_shard(meta, shard, sinputs, 60, || false)
            .await
            .is_err()
    );

    // The shard is released back to claimable, not terminally Failed, and
    // its attempt count is preserved for the poison guard.
    let shards = registry.list_job_shards(&job_id).await.unwrap();
    assert_eq!(shards[0].status, ShardStatus::Pending);
    assert_eq!(shards[0].attempts, 1);
    assert!(shards[0].owner.is_none());

    // Storage recovers; a later worker reclaims and completes the shard.
    registry.storage.set_fail(false);
    let (meta2, shard2, sinputs2) = registry
        .claim_next_shard("w2", 60, 3, t0)
        .await
        .unwrap()
        .expect("released shard is reclaimable");
    assert_eq!(shard2.attempts, 2);
    registry
        .run_claimed_shard(meta2, shard2, sinputs2, 60, || false)
        .await
        .unwrap();

    let done = registry.get_batch_job(&job_id).await.unwrap();
    assert_eq!(done.status, JobStatus::Completed);
    assert_eq!(done.done, 1);
}

#[tokio::test]
async fn test_render_error_rebuilds_world_and_batch_continues() {
    use crate::batch::BatchInput;

    // A near-zero render timeout forces every item down the render-error
    // path, which consumes the warm world. render_one must rebuild the world
    // (as Some, via the fonts-aware constructor) each time, so the batch
    // keeps running instead of panicking on a taken/None world or silently
    // continuing with a fonts-less placeholder.
    let registry = Registry::new(
        MemoryStorage::new(),
        crate::render_storage::MemoryRenderStorage::new(),
    )
    .with_render_limits(2, std::time::Duration::from_nanos(1));
    registry
        .publish(create_test_bundle(), "batch", "latest")
        .await
        .unwrap();

    let inputs: Vec<BatchInput> = ["A", "B", "C"]
        .iter()
        .map(|n| BatchInput {
            data: serde_json::json!({ "name": n }),
            key: None,
        })
        .collect();
    let job = registry
        .enqueue_batch_job("batch:latest", &inputs, None, &RenderOptions::default())
        .await
        .unwrap();
    let job_id = job.job_id.clone();

    let t0 = time::OffsetDateTime::now_utc();
    let (meta, shard, sinputs) = registry
        .claim_next_shard("w", 60, 3, t0)
        .await
        .unwrap()
        .unwrap();
    // Completes without panicking even though every item errors.
    registry
        .run_claimed_shard(meta, shard, sinputs, 60, || false)
        .await
        .unwrap();

    let view = registry.get_batch_job(&job_id).await.unwrap();
    assert_eq!(view.done, 0);
    assert_eq!(view.failed, 3);
}

#[tokio::test]
async fn claim_skips_completed_jobs_via_pending_markers() {
    use crate::batch::BatchInput;

    let registry = Registry::new(
        CountingStorage::default(),
        crate::render_storage::MemoryRenderStorage::new(),
    );
    registry
        .publish(create_test_bundle(), "batch", "latest")
        .await
        .unwrap();
    let job = registry
        .enqueue_batch_job(
            "batch:latest",
            &[BatchInput {
                data: serde_json::json!({ "name": "A" }),
                key: None,
            }],
            None,
            &RenderOptions::default(),
        )
        .await
        .unwrap();

    // One shard, one pending marker.
    assert_eq!(
        registry
            .storage
            .list_keys(layout::PENDING_PREFIX)
            .await
            .unwrap()
            .len(),
        1
    );

    let now = time::OffsetDateTime::now_utc();
    let (meta, shard, inputs) = registry
        .claim_next_shard("w", 60, 3, now)
        .await
        .unwrap()
        .unwrap();
    registry
        .run_claimed_shard(meta, shard, inputs, 60, || false)
        .await
        .unwrap();
    let _ = job;

    // Completing the shard removes its marker.
    assert!(
        registry
            .storage
            .list_keys(layout::PENDING_PREFIX)
            .await
            .unwrap()
            .is_empty()
    );

    // A later poll reads no shard descriptors at all — the completed job's
    // descriptors are never scanned, only the (empty) pending listing.
    let before = registry.storage.total_gets();
    assert!(
        registry
            .claim_next_shard("w", 60, 3, now)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(registry.storage.total_gets(), before);
}

#[tokio::test]
async fn dead_worker_shard_is_reclaimed_via_its_pending_marker() {
    use crate::batch::{BatchInput, JobStatus};

    let registry = Registry::new(
        MemoryStorage::new(),
        crate::render_storage::MemoryRenderStorage::new(),
    );
    registry
        .publish(create_test_bundle(), "batch", "latest")
        .await
        .unwrap();
    let job = registry
        .enqueue_batch_job(
            "batch:latest",
            &[BatchInput {
                data: serde_json::json!({ "name": "A" }),
                key: None,
            }],
            None,
            &RenderOptions::default(),
        )
        .await
        .unwrap();
    let job_id = job.job_id.clone();

    // Worker A claims the only shard, then "dies" without running it.
    let t0 = time::OffsetDateTime::now_utc();
    let (_m, shard_a, _i) = registry
        .claim_next_shard("worker-a", 60, 3, t0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(shard_a.attempts, 1);

    // The pending marker survives the claim (a claim isn't completion), so
    // the shard still shows up as outstanding work.
    assert_eq!(
        registry
            .storage
            .list_keys(layout::PENDING_PREFIX)
            .await
            .unwrap()
            .len(),
        1
    );

    // Before the lease expires no other worker can take it.
    let t_soon = t0 + time::Duration::seconds(10);
    assert!(
        registry
            .claim_next_shard("worker-b", 60, 3, t_soon)
            .await
            .unwrap()
            .is_none()
    );

    // After the lease expires, worker B reclaims it via the marker and runs
    // it to completion.
    let t_late = t0 + time::Duration::seconds(120);
    let (meta_b, shard_b, inputs_b) = registry
        .claim_next_shard("worker-b", 60, 3, t_late)
        .await
        .unwrap()
        .expect("expired lease is reclaimable via its pending marker");
    assert_eq!(shard_b.attempts, 2);
    registry
        .run_claimed_shard(meta_b, shard_b, inputs_b, 60, || false)
        .await
        .unwrap();

    // Completed now: the marker is gone and the job is done.
    assert!(
        registry
            .storage
            .list_keys(layout::PENDING_PREFIX)
            .await
            .unwrap()
            .is_empty()
    );
    let view = registry.get_batch_job(&job_id).await.unwrap();
    assert_eq!(view.status, JobStatus::Completed);
    assert_eq!(view.done, 1);
}

#[tokio::test]
async fn run_claimed_shard_releases_for_reclaim_on_shutdown() {
    use crate::batch::{BatchInput, JobStatus, ShardStatus};

    let registry = Registry::new(
        MemoryStorage::new(),
        crate::render_storage::MemoryRenderStorage::new(),
    );
    registry
        .publish(create_test_bundle(), "batch", "latest")
        .await
        .unwrap();
    let inputs: Vec<BatchInput> = ["A", "B"]
        .iter()
        .map(|n| BatchInput {
            data: serde_json::json!({ "name": n }),
            key: None,
        })
        .collect();
    let job = registry
        .enqueue_batch_job("batch:latest", &inputs, None, &RenderOptions::default())
        .await
        .unwrap();
    let job_id = job.job_id.clone();

    // A shutdown that trips immediately releases the shard before any item.
    let t0 = time::OffsetDateTime::now_utc();
    let (meta, shard, sinputs) = registry
        .claim_next_shard("w1", 60, 3, t0)
        .await
        .unwrap()
        .unwrap();
    registry
        .run_claimed_shard(meta, shard, sinputs, 60, || true)
        .await
        .unwrap();

    let shards = registry.list_job_shards(&job_id).await.unwrap();
    assert_eq!(shards[0].status, ShardStatus::Pending);
    assert!(shards[0].owner.is_none());
    // The marker survives so the shard is still discovered as pending work.
    assert_eq!(
        registry
            .storage
            .list_keys(layout::PENDING_PREFIX)
            .await
            .unwrap()
            .len(),
        1
    );

    // A later worker (not shutting down) reclaims and completes it.
    let (m2, s2, i2) = registry
        .claim_next_shard("w2", 60, 3, t0)
        .await
        .unwrap()
        .unwrap();
    registry
        .run_claimed_shard(m2, s2, i2, 60, || false)
        .await
        .unwrap();
    let done = registry.get_batch_job(&job_id).await.unwrap();
    assert_eq!(done.status, JobStatus::Completed);
    assert_eq!(done.done, 2);
}

#[tokio::test]
async fn test_batch_shards_concurrent_workers() {
    use crate::batch::{BatchInput, ItemStatus, JobStatus};
    use std::sync::Arc;

    let storage = MemoryStorage::new();
    let render_storage = crate::render_storage::MemoryRenderStorage::new();
    let registry = Arc::new(Registry::new(storage, render_storage).with_shard_size(2));
    registry
        .publish(create_test_bundle(), "batch", "latest")
        .await
        .unwrap();

    // 7 items over shard_size 2 => 4 shards, drained by 3 concurrent workers.
    let inputs: Vec<BatchInput> = (0..7)
        .map(|i| BatchInput {
            data: serde_json::json!({ "name": format!("c{i}") }),
            key: Some(format!("k{i}")),
        })
        .collect();
    let job = registry
        .enqueue_batch_job("batch:latest", &inputs, None, &RenderOptions::default())
        .await
        .unwrap();
    let job_id = job.job_id.clone();

    let mut handles = Vec::new();
    for w in 0..3 {
        let reg = registry.clone();
        let wid = format!("worker-{w}");
        handles.push(tokio::spawn(async move {
            let now = time::OffsetDateTime::now_utc();
            while let Some((meta, shard, sinputs)) =
                reg.claim_next_shard(&wid, 120, 3, now).await.unwrap()
            {
                reg.run_claimed_shard(meta, shard, sinputs, 120, || false)
                    .await
                    .unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let done = registry.get_batch_job(&job_id).await.unwrap();
    assert_eq!(done.status, JobStatus::Completed);
    assert_eq!(done.done, 7);
    assert_eq!(done.failed, 0);

    // Every item rendered exactly once (indices 0..7 all present).
    let items = registry.list_job_items(&job_id, 0, 100).await.unwrap();
    assert_eq!(items.len(), 7);
    assert!(
        items
            .iter()
            .all(|i| i.status == ItemStatus::Success && i.render_id.is_some())
    );
    let mut idxs: Vec<usize> = items.iter().map(|i| i.index).collect();
    idxs.sort();
    assert_eq!(idxs, (0..7).collect::<Vec<_>>());
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
