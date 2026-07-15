use thiserror::Error;

/// Main error type for papermake-registry operations
#[derive(Error, Debug)]
pub enum RegistryError {
    /// Storage backend errors (S3, filesystem, network)
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),

    /// Template not found or invalid reference
    #[error("Template error: {0}")]
    Template(#[from] TemplateError),

    /// Reference parsing and resolution errors
    #[error("Reference error: {0}")]
    Reference(#[from] ReferenceError),

    /// Content addressing and hashing errors
    #[error("Content addressing error: {0}")]
    ContentAddressing(#[from] ContentAddressingError),

    /// Template compilation errors from papermake core
    #[error("Compilation error: {0}")]
    Compilation(#[from] papermake::PapermakeError),

    /// JSON serialization/deserialization errors
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Cache-related errors
    #[error("Cache error: {0}")]
    Cache(#[from] CacheError),

    /// Render storage errors
    #[error("Render storage error: {0}")]
    RenderStorage(#[from] crate::render_storage::RenderStorageError),

    /// Render timed out before Typst finished.
    #[error("Render timed out after {seconds}s")]
    RenderTimeout { seconds: u64 },

    /// Authorization and permission errors
    #[error("Access denied: {0}")]
    AccessDenied(String),

    /// Version policy violations
    #[error("Version policy error: {0}")]
    VersionPolicy(String),
}

/// Storage backend operation errors
#[derive(Error, Debug)]
pub enum StorageError {
    /// Key/object not found in storage
    #[error("Not found: {key}")]
    NotFound { key: String },

    /// Access denied or authentication failed
    #[error("Access denied for key: {key}")]
    AccessDenied { key: String },

    /// Network connectivity issues
    #[error("Network error: {message}")]
    Network { message: String },

    /// Storage backend internal error
    #[error("Backend error: {message}")]
    Backend { message: String },

    /// Invalid storage configuration
    #[error("Invalid configuration: {message}")]
    Configuration { message: String },

    /// Operation timeout
    #[error("Operation timeout after {seconds}s")]
    Timeout { seconds: u64 },

    /// Storage quota or limits exceeded
    #[error("Storage limit exceeded: {message}")]
    LimitExceeded { message: String },
}

/// Template-related errors within registry context
#[derive(Error, Debug)]
pub enum TemplateError {
    /// Template not found in registry
    #[error("Template not found: {reference}")]
    NotFound { reference: String },

    /// Invalid template structure or manifest
    #[error("Invalid template: {message}")]
    Invalid { message: String },

    /// Required template file missing (e.g., main.typ)
    #[error("Missing required file: {filename}")]
    MissingFile { filename: String },

    /// Template metadata validation failed
    #[error("Invalid metadata: {field} - {message}")]
    InvalidMetadata { field: String, message: String },

    /// Template bundle conversion errors
    #[error("Bundle conversion failed: {message}")]
    ConversionFailed { message: String },

    /// Template already exists (for immutable operations)
    #[error("Template already exists: {reference}")]
    AlreadyExists { reference: String },

    /// Template size exceeds limits
    #[error("Template too large: {size} bytes (max: {limit})")]
    TooLarge { size: u64, limit: u64 },
}

/// Reference parsing and resolution errors
#[derive(Error, Debug)]
pub enum ReferenceError {
    /// Invalid reference format
    #[error("Invalid reference format: '{reference}' - {reason}")]
    InvalidFormat { reference: String, reason: String },

    /// Invalid namespace format or characters
    #[error("Invalid namespace: '{namespace}' - {reason}")]
    InvalidNamespace { namespace: String, reason: String },

    /// Invalid tag format or characters
    #[error("Invalid tag: '{tag}' - {reason}")]
    InvalidTag { tag: String, reason: String },

    /// Invalid hash format
    #[error("Invalid hash: '{hash}' - expected sha256:...")]
    InvalidHash { hash: String },

    /// Hash verification failed
    #[error("Hash verification failed: tag '{tag}' points to {actual}, expected {expected}")]
    HashMismatch {
        tag: String,
        expected: String,
        actual: String,
    },

    /// Reference resolution failed
    #[error("Failed to resolve reference: {reference}")]
    ResolutionFailed { reference: String },

    /// Ambiguous reference (multiple matches)
    #[error("Ambiguous reference: {reference} - {reason}")]
    Ambiguous { reference: String, reason: String },
}

/// Content addressing and hashing errors
#[derive(Error, Debug)]
pub enum ContentAddressingError {
    /// Hash computation failed
    #[error("Hash computation failed: {message}")]
    HashFailed { message: String },

    /// Content integrity check failed
    #[error("Content integrity check failed: expected {expected}, got {actual}")]
    IntegrityCheckFailed { expected: String, actual: String },

    /// Invalid content hash format
    #[error("Invalid hash format: {hash}")]
    InvalidHashFormat { hash: String },

    /// Manifest creation or parsing failed
    #[error("Manifest error: {message}")]
    ManifestError { message: String },

    /// Circular dependency detected in manifest
    #[error("Circular dependency detected: {path}")]
    CircularDependency { path: String },
}

/// Cache operation errors
#[derive(Error, Debug)]
pub enum CacheError {
    /// Cache initialization failed
    #[error("Cache initialization failed: {message}")]
    InitializationFailed { message: String },

    /// Cache poisoned (lock corruption)
    #[error("Cache lock poisoned")]
    Poisoned,

    /// Cache eviction failed
    #[error("Cache eviction failed: {message}")]
    EvictionFailed { message: String },

    /// Cache invalidation failed
    #[error("Cache invalidation failed for refs: {refs:?}")]
    InvalidationFailed { refs: Vec<String> },

    /// Cache consistency check failed
    #[error("Cache consistency error: {message}")]
    ConsistencyError { message: String },
}

/// Result type alias for registry operations
pub type RegistryResult<T> = Result<T, RegistryError>;

/// Result type alias for storage operations
pub type StorageResult<T> = Result<T, StorageError>;

/// Result type alias for template operations
pub type TemplateResult<T> = Result<T, TemplateError>;

/// Result type alias for reference operations
pub type ReferenceResult<T> = Result<T, ReferenceError>;

/// Result type alias for content addressing operations
pub type ContentAddressingResult<T> = Result<T, ContentAddressingError>;

/// Result type alias for cache operations
pub type CacheResult<T> = Result<T, CacheError>;

impl StorageError {
    /// Create a not found error
    pub fn not_found(key: impl Into<String>) -> Self {
        Self::NotFound { key: key.into() }
    }

    /// Create an access denied error
    pub fn access_denied(key: impl Into<String>) -> Self {
        Self::AccessDenied { key: key.into() }
    }

    /// Create a network error
    pub fn network(message: impl Into<String>) -> Self {
        Self::Network {
            message: message.into(),
        }
    }

    /// Create a backend error
    pub fn backend(message: impl Into<String>) -> Self {
        Self::Backend {
            message: message.into(),
        }
    }

    /// Create a configuration error
    pub fn configuration(message: impl Into<String>) -> Self {
        Self::Configuration {
            message: message.into(),
        }
    }

    /// Create a timeout error
    pub fn timeout(seconds: u64) -> Self {
        Self::Timeout { seconds }
    }

    /// Create a limit exceeded error
    pub fn limit_exceeded(message: impl Into<String>) -> Self {
        Self::LimitExceeded {
            message: message.into(),
        }
    }
}

impl TemplateError {
    /// Create a not found error
    pub fn not_found(reference: impl Into<String>) -> Self {
        Self::NotFound {
            reference: reference.into(),
        }
    }

    /// Create an invalid template error
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid {
            message: message.into(),
        }
    }

    /// Create a missing file error
    pub fn missing_file(filename: impl Into<String>) -> Self {
        Self::MissingFile {
            filename: filename.into(),
        }
    }

    /// Create an invalid metadata error
    pub fn invalid_metadata(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::InvalidMetadata {
            field: field.into(),
            message: message.into(),
        }
    }

    /// Create a conversion failed error
    pub fn conversion_failed(message: impl Into<String>) -> Self {
        Self::ConversionFailed {
            message: message.into(),
        }
    }

    /// Create an already exists error
    pub fn already_exists(reference: impl Into<String>) -> Self {
        Self::AlreadyExists {
            reference: reference.into(),
        }
    }

    /// Create a too large error
    pub fn too_large(size: u64, limit: u64) -> Self {
        Self::TooLarge { size, limit }
    }
}

impl ReferenceError {
    /// Create an invalid format error
    pub fn invalid_format(reference: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::InvalidFormat {
            reference: reference.into(),
            reason: reason.into(),
        }
    }

    /// Create an invalid namespace error
    pub fn invalid_namespace(namespace: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::InvalidNamespace {
            namespace: namespace.into(),
            reason: reason.into(),
        }
    }

    /// Create an invalid tag error
    pub fn invalid_tag(tag: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::InvalidTag {
            tag: tag.into(),
            reason: reason.into(),
        }
    }

    /// Create an invalid hash error
    pub fn invalid_hash(hash: impl Into<String>) -> Self {
        Self::InvalidHash { hash: hash.into() }
    }

    /// Create a hash mismatch error
    pub fn hash_mismatch(
        tag: impl Into<String>,
        expected: impl Into<String>,
        actual: impl Into<String>,
    ) -> Self {
        Self::HashMismatch {
            tag: tag.into(),
            expected: expected.into(),
            actual: actual.into(),
        }
    }

    /// Create a resolution failed error
    pub fn resolution_failed(reference: impl Into<String>) -> Self {
        Self::ResolutionFailed {
            reference: reference.into(),
        }
    }

    /// Create an ambiguous reference error
    pub fn ambiguous(reference: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Ambiguous {
            reference: reference.into(),
            reason: reason.into(),
        }
    }
}

impl ContentAddressingError {
    /// Create a hash failed error
    pub fn hash_failed(message: impl Into<String>) -> Self {
        Self::HashFailed {
            message: message.into(),
        }
    }

    /// Create an integrity check failed error
    pub fn integrity_check_failed(expected: impl Into<String>, actual: impl Into<String>) -> Self {
        Self::IntegrityCheckFailed {
            expected: expected.into(),
            actual: actual.into(),
        }
    }

    /// Create an invalid hash format error
    pub fn invalid_hash_format(hash: impl Into<String>) -> Self {
        Self::InvalidHashFormat { hash: hash.into() }
    }

    /// Create a manifest error
    pub fn manifest_error(message: impl Into<String>) -> Self {
        Self::ManifestError {
            message: message.into(),
        }
    }

    /// Create a circular dependency error
    pub fn circular_dependency(path: impl Into<String>) -> Self {
        Self::CircularDependency { path: path.into() }
    }
}

impl CacheError {
    /// Create an initialization failed error
    pub fn initialization_failed(message: impl Into<String>) -> Self {
        Self::InitializationFailed {
            message: message.into(),
        }
    }

    /// Create a poisoned error
    pub fn poisoned() -> Self {
        Self::Poisoned
    }

    /// Create an eviction failed error
    pub fn eviction_failed(message: impl Into<String>) -> Self {
        Self::EvictionFailed {
            message: message.into(),
        }
    }

    /// Create an invalidation failed error
    pub fn invalidation_failed(refs: Vec<String>) -> Self {
        Self::InvalidationFailed { refs }
    }

    /// Create a consistency error
    pub fn consistency_error(message: impl Into<String>) -> Self {
        Self::ConsistencyError {
            message: message.into(),
        }
    }
}

// Conversion from lock poisoning errors
impl<T> From<std::sync::PoisonError<T>> for CacheError {
    fn from(_: std::sync::PoisonError<T>) -> Self {
        CacheError::Poisoned
    }
}

// Conversion from UTF-8 errors for template content
impl From<std::string::FromUtf8Error> for TemplateError {
    fn from(err: std::string::FromUtf8Error) -> Self {
        TemplateError::Invalid {
            message: format!("Invalid UTF-8 content: {}", err),
        }
    }
}

// Conversion from std::io::Error to StorageError
impl From<std::io::Error> for StorageError {
    fn from(err: std::io::Error) -> Self {
        match err.kind() {
            std::io::ErrorKind::NotFound => StorageError::not_found("file"),
            std::io::ErrorKind::PermissionDenied => StorageError::access_denied("file"),
            std::io::ErrorKind::TimedOut => StorageError::timeout(30),
            _ => StorageError::backend(err.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_error_constructors_preserve_the_error_domain() {
        assert!(matches!(
            StorageError::not_found("templates/main.typ"),
            StorageError::NotFound { key } if key == "templates/main.typ"
        ));
        assert!(matches!(
            StorageError::access_denied("secret"),
            StorageError::AccessDenied { key } if key == "secret"
        ));
        assert!(matches!(
            StorageError::network("offline"),
            StorageError::Network { message } if message == "offline"
        ));
        assert!(matches!(
            StorageError::backend("failed"),
            StorageError::Backend { message } if message == "failed"
        ));
        assert!(matches!(
            StorageError::configuration("missing bucket"),
            StorageError::Configuration { message } if message == "missing bucket"
        ));
        assert!(matches!(
            StorageError::timeout(5),
            StorageError::Timeout { seconds: 5 }
        ));
        assert!(matches!(
            StorageError::limit_exceeded("too large"),
            StorageError::LimitExceeded { message } if message == "too large"
        ));
    }

    #[test]
    fn template_error_constructors_preserve_the_error_domain() {
        assert!(matches!(
            TemplateError::not_found("invoice:latest"),
            TemplateError::NotFound { reference } if reference == "invoice:latest"
        ));
        assert!(matches!(
            TemplateError::invalid("bad manifest"),
            TemplateError::Invalid { message } if message == "bad manifest"
        ));
        assert!(matches!(
            TemplateError::missing_file("main.typ"),
            TemplateError::MissingFile { filename } if filename == "main.typ"
        ));
        assert!(matches!(
            TemplateError::invalid_metadata("name", "empty"),
            TemplateError::InvalidMetadata { field, message } if field == "name" && message == "empty"
        ));
        assert!(matches!(
            TemplateError::conversion_failed("tar failed"),
            TemplateError::ConversionFailed { message } if message == "tar failed"
        ));
        assert!(matches!(
            TemplateError::already_exists("invoice:v1"),
            TemplateError::AlreadyExists { reference } if reference == "invoice:v1"
        ));
        assert!(matches!(
            TemplateError::too_large(12, 10),
            TemplateError::TooLarge {
                size: 12,
                limit: 10
            }
        ));
    }

    #[test]
    fn reference_error_constructors_preserve_the_error_domain() {
        assert!(matches!(
            ReferenceError::invalid_format("bad", "missing name"),
            ReferenceError::InvalidFormat { reference, reason } if reference == "bad" && reason == "missing name"
        ));
        assert!(matches!(
            ReferenceError::invalid_namespace("bad namespace", "spaces"),
            ReferenceError::InvalidNamespace { namespace, reason } if namespace == "bad namespace" && reason == "spaces"
        ));
        assert!(matches!(
            ReferenceError::invalid_tag("bad tag", "spaces"),
            ReferenceError::InvalidTag { tag, reason } if tag == "bad tag" && reason == "spaces"
        ));
        assert!(matches!(
            ReferenceError::invalid_hash("sha256:bad"),
            ReferenceError::InvalidHash { hash } if hash == "sha256:bad"
        ));
        assert!(matches!(
            ReferenceError::hash_mismatch("latest", "sha256:a", "sha256:b"),
            ReferenceError::HashMismatch { tag, expected, actual }
                if tag == "latest" && expected == "sha256:a" && actual == "sha256:b"
        ));
        assert!(matches!(
            ReferenceError::resolution_failed("invoice"),
            ReferenceError::ResolutionFailed { reference } if reference == "invoice"
        ));
        assert!(matches!(
            ReferenceError::ambiguous("invoice", "multiple namespaces"),
            ReferenceError::Ambiguous { reference, reason }
                if reference == "invoice" && reason == "multiple namespaces"
        ));
    }

    #[test]
    fn content_addressing_error_constructors_preserve_the_error_domain() {
        assert!(matches!(
            ContentAddressingError::hash_failed("unavailable"),
            ContentAddressingError::HashFailed { message } if message == "unavailable"
        ));
        assert!(matches!(
            ContentAddressingError::integrity_check_failed("sha256:a", "sha256:b"),
            ContentAddressingError::IntegrityCheckFailed { expected, actual }
                if expected == "sha256:a" && actual == "sha256:b"
        ));
        assert!(matches!(
            ContentAddressingError::invalid_hash_format("bad"),
            ContentAddressingError::InvalidHashFormat { hash } if hash == "bad"
        ));
        assert!(matches!(
            ContentAddressingError::manifest_error("invalid"),
            ContentAddressingError::ManifestError { message } if message == "invalid"
        ));
        assert!(matches!(
            ContentAddressingError::circular_dependency("main.typ"),
            ContentAddressingError::CircularDependency { path } if path == "main.typ"
        ));
    }

    #[test]
    fn cache_error_constructors_preserve_the_error_domain() {
        assert!(matches!(
            CacheError::initialization_failed("unavailable"),
            CacheError::InitializationFailed { message } if message == "unavailable"
        ));
        assert!(matches!(CacheError::poisoned(), CacheError::Poisoned));
        assert!(matches!(
            CacheError::eviction_failed("locked"),
            CacheError::EvictionFailed { message } if message == "locked"
        ));
        assert!(matches!(
            CacheError::invalidation_failed(vec!["a".to_string(), "b".to_string()]),
            CacheError::InvalidationFailed { refs } if refs == vec!["a", "b"]
        ));
        assert!(matches!(
            CacheError::consistency_error("stale"),
            CacheError::ConsistencyError { message } if message == "stale"
        ));
    }

    #[test]
    fn external_error_conversions_preserve_actionable_domains() {
        let not_found = StorageError::from(std::io::Error::from(std::io::ErrorKind::NotFound));
        let permission_denied =
            StorageError::from(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        let timed_out = StorageError::from(std::io::Error::from(std::io::ErrorKind::TimedOut));
        let other = StorageError::from(std::io::Error::other("disk unavailable"));
        let utf8 = TemplateError::from(String::from_utf8(vec![0xff]).unwrap_err());
        let poisoned = CacheError::from(std::sync::PoisonError::new(()));

        assert!(matches!(not_found, StorageError::NotFound { .. }));
        assert!(matches!(
            permission_denied,
            StorageError::AccessDenied { .. }
        ));
        assert!(matches!(timed_out, StorageError::Timeout { .. }));
        assert!(matches!(other, StorageError::Backend { .. }));
        assert!(matches!(utf8, TemplateError::Invalid { .. }));
        assert!(matches!(poisoned, CacheError::Poisoned));
    }
}
