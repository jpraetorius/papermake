# Papermake - Typst Template Registry

A content-addressable registry for Typst templates with server-side rendering capabilities.

## Project Overview

Papermake consists of three main crates:
- **`papermake`**: Core Typst compilation engine with virtual filesystem
- **`papermake-registry`**: Content-addressable template storage and publishing
- **`papermake-server`**: HTTP API and web interface

## Architecture

```
User → Web UI → Server API → Registry Library → Typst Engine
                     ↓              ↓
                Cache Layer    Blob Storage (S3)
```

### Key Design Principles
- **Content-addressable storage** using SHA-256 hashes
- **Merkle tree approach** for efficient deduplication
- **Mutable tags** pointing to immutable content
- **Server-side rendering** with `template.render(data) → PDF`
- **Library-first architecture** with clean separation

## Current Implementation Status

### ✅ Phase 1: Core Typst Engine (`papermake` crate)
- [x] `TypstWorld` with virtual filesystem support
- [x] `TypstFileSystem` trait for async file resolution
- [x] Font caching via `CACHED_FONTS` static
- [x] Data injection through `sys.inputs.data`
- [x] Basic template rendering functionality

### ✅ Phase 2: Registry Foundation (`papermake-registry` crate)
- [x] `BlobStorage` trait with async operations
- [x] `S3Storage` implementation for AWS S3 compatibility
- [x] `ContentAddress` utilities for SHA-256 hashing and key generation
- [x] `TemplateBundle` and `TemplateMetadata` structs with validation
- [x] `Manifest` serialization/deserialization
- [x] Integration tests for storage layer

### 🚧 Phase 3: Registry File System Integration (In Progress)
- [x] `RegistryFileSystem<S: BlobStorage>` implementing `TypstFileSystem`
- [x] `TemplateReference` parsing (`namespace:tag@hash` format)
- [x] `Registry::publish()` method (store files → create manifest → update refs)
- [x] `Registry::resolve()` method (tag → manifest hash lookup)
- [ ] `Registry::render()` method using `RegistryFileSystem` ← **NEXT**

### 📋 Phase 4: Caching Layer (Planned)
- [ ] `Cache` struct with LRU for blobs, manifests, and refs
- [ ] Cache integration in `Registry` methods
- [ ] Cache invalidation API for webhooks
- [ ] Performance testing

### 📋 Phase 5: Server Layer (Planned)
- [ ] HTTP server with `/render/{reference}` endpoint
- [ ] `/publish` endpoint for template uploads
- [ ] Authentication and authorization
- [ ] Version tag immutability enforcement

## Code Structure

```
papermake/
├── papermake/                 # Core Typst engine
│   ├── src/
│   │   ├── lib.rs
│   │   ├── typst_world.rs     # TypstWorld implementation
│   │   └── filesystem.rs      # TypstFileSystem trait
│   └── Cargo.toml
├── papermake-registry/        # Registry core
│   ├── src/
│   │   ├── lib.rs
│   │   ├── storage/           # BlobStorage implementations
│   │   ├── bundle.rs          # TemplateBundle & TemplateMetadata
│   │   ├── manifest.rs        # Manifest format ← IMPLEMENT NEXT
│   │   ├── address.rs         # ContentAddress utilities
│   │   ├── registry.rs        # Registry core logic
│   │   └── reference.rs       # TemplateReference parsing
│   └── Cargo.toml
└── papermake-server/          # HTTP API (future)
    ├── src/lib.rs
    └── Cargo.toml
```

## Reference Format

Templates are referenced using: `[org/user]/name:tag@sha256:hash`

**Examples:**
- `invoice:latest` - Official template
- `john/invoice:latest` - User template
- `acme-corp/letterhead:stable` - Organization template
- `john/invoice:latest@sha256:abc123` - Tag with hash verification

## Storage Layout

```
storage/
├── blobs/sha256/{hash}           # Individual files
├── manifests/sha256/{hash}       # Template manifests
└── refs/
    ├── invoice/latest            # Official: name/tag
    ├── john/invoice/latest       # User: user/name/tag
    └── acme-corp/invoice/stable  # Org: org/name/tag
```

## Manifest Format

```json
{
  "entrypoint": "main.typ",
  "files": {
    "main.typ": "sha256:abc123...",
    "schema.json": "sha256:def456...",
    "components/header.typ": "sha256:ghi789...",
    "assets/logo.png": "sha256:jkl012..."
  },
  "metadata": {
    "name": "invoice-template",
    "author": "john@example.com"
  }
}
```

## Implementation Tasks

### Immediate Next Steps

1. **Implement `Manifest` struct** (`papermake-registry/src/manifest.rs`)
   ```rust
   #[derive(Serialize, Deserialize)]
   pub struct Manifest {
       pub entrypoint: String,
       pub files: HashMap<String, String>, // filename -> hash
       pub metadata: TemplateMetadata,
   }

   pub struct TemplateMetadata {
       pub name: String,
       pub author: String,
   }
   ```

2. **Add manifest serialization tests**
   - JSON roundtrip testing
   - Validate required fields
   - Error handling for malformed manifests

3. **Implement `TemplateReference` parsing** (`papermake-registry/src/reference.rs`)
   ```rust
   pub struct TemplateReference {
       pub namespace: String,
       pub tag: Option<String>,
       pub hash: Option<String>,
   }

   impl std::str::FromStr for TemplateReference { /* ... */ }
   ```

### Current Development Focus

**Working on:** Registry file system integration to resolve Typst imports directly through blob storage

**Key insight:** Instead of materializing full template bundles, resolve individual file imports on-demand through the `TypstFileSystem` trait, enabling:
- Memory efficiency (lazy loading)
- Natural caching at the blob level
- Consistent content-addressable access

## Testing Strategy

### Unit Tests
- Content addressing consistency (same content = same hash)
- Template bundle validation
- Reference parsing edge cases
- Storage backend operations

### Integration Tests
- End-to-end publish workflow
- Template resolution and rendering
- Cache behavior and invalidation
- S3 storage with mocked backend

### Performance Tests
- Template rendering latency
- Cache hit/miss ratios
- Concurrent access patterns
- Large template handling

## Error Handling

```rust
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),

    #[error("Template not found: {0}")]
    TemplateNotFound(String),

    #[error("Invalid reference format: {0}")]
    InvalidReference(String),

    #[error("Compilation error: {0}")]
    CompileError(#[from] papermake::CompileError),

    #[error("Hash verification failed")]
    HashMismatch,
}
```

## Usage Examples

### Publishing a Template
```rust
let metadata = TemplateMetadata::new(
    "Invoice Template",
    "john@example.com"
);

let bundle = TemplateBundle::new(main_typ_content, metadata)
    .with_schema(schema_json)
    .add_file("assets/logo.png", logo_data);

let manifest_hash = registry.publish(bundle, "john/invoice", "latest").await?;
```

### Rendering a Template
```rust
let pdf_bytes = registry.render(
    "john/invoice:latest",
    json!({
        "from": "Acme Corp",
        "to": "Client Name",
        "items": [{"description": "Service", "amount": "$100"}],
        "total": "$100"
    })
).await?;
```

## Development Workflow

1. **Run tests**: `cargo test --workspace`
2. **Check formatting**: `cargo fmt --all`
3. **Run clippy**: `cargo clippy --workspace --all-targets`
4. **Build all crates**: `cargo build --workspace`
5. **Run integration tests**: `cargo test --workspace --test integration`

## Contributing Guidelines

1. **Code Style**: Use `cargo fmt` and `cargo clippy`
2. **Testing**: All new features must include comprehensive tests
3. **Documentation**: Public APIs must be documented
4. **Error Handling**: Use `thiserror` for custom error types
5. **Async**: Use `async-trait` for async trait methods

---

*This document is probably not up-to-date. If there are differences between the code and the document, please refer to the source code for the most accurate information.*


# Papermake Server Implementation Todo List

## Tech Stack & Decisions
- **Framework**: Axum/Tokio for async HTTP server
- **Storage**: S3 for **everything** — blobs (templates/assets/manifests, content-addressed) and render outputs (`renders/{render_id}/{meta.json,pdf,data}`, keyed by render_id). No always-on database.
- **Analytics**: **buffered-S3** — each server instance stages `RenderRecord`s in memory and flushes them to S3 as NDJSON (`analytics/raw/dt=…`); the **papermake-worker** binary aggregates all raw into `analytics/agg/summary.json` and prunes expired outputs. Queries are always answered from `summary.json` (globally eventually consistent). ClickHouse has been removed.
- **UI**: server-side-rendered (maud + a small hand-rolled stylesheet in `assets/app.css` + a tiny htmx sprinkle). Semantic-first CSS (modern: cascade layers, nesting, oklch, light-dark()). No SPA/build step. (The old Next.js `./webui` was deleted.)
- **Auth**: None initially, optional API keys later
- **Namespaces**: Simplified `name:tag` format (no user/org prefixes)
- **Config**: Environment variables only
- **Deployment**: Docker containers (server + worker + SeaweedFS)

### Analytics/output storage design (see `docs/analytics-storage-and-ssr.md`)
- **Artifacts** are keyed by `render_id`: `get_render_pdf` reads `renders/{id}/meta.json` (missing → 404, `success=false` → 422/`RenderFailed`, else serves `renders/{id}/pdf`); `get_render_data` is a direct blob read. Immediate, flush-independent.
- **Records** flow: `render_and_store` → in-memory buffer → periodic `flush()` → `analytics/raw` NDJSON + an `expiry/dt=<expiry-date>/…` index → worker `aggregator::run` → `summary.json`; worker `retention::prune` deletes due-partition artifacts and old raw.
- **Retention** precedence: per-render (`retain_days` on the render request) → per-template (`TemplateMetadata.retain_days`) → global (`RENDER_RETENTION_DAYS`); `0` = keep forever.

### Environment variables
- Server/worker S3: `S3_ENDPOINT_URL`, `S3_REGION`, `S3_BUCKET`, `S3_ACCESS_KEY_ID`, `S3_SECRET_ACCESS_KEY`.
- Server buffered analytics/retention: `PAPERMAKE_INSTANCE_ID`, `FLUSH_INTERVAL_SECONDS`, `FLUSH_MAX_RECORDS`, `RENDER_RETENTION_DAYS`.
- Worker: `WORKER_INTERVAL_SECONDS`, `ANALYTICS_RETENTION_DAYS`.

## Phase 1: Basic HTTP Server
- [x] Set up Axum server with basic routing
- [x] Add environment variable configuration (S3 credentials + analytics/retention)
- [x] Implement health check endpoint `GET /health`
- [x] Add startup validation (required S3 connectivity)
- [x] Basic error handling and JSON response structure

## Phase 2: Template Management
- [x] Implement `POST /templates/{name}/publish?tag=latest` with multipart form
- [x] Implement `POST /templates/{name}/publish-simple` with json body
- [x] Implement `GET /templates` - list all templates with metadata
- [x] Implement `GET /templates/{name}/tags` - list tags for template
- [x] Implement `GET /templates/{reference}` - get template metadata
- [x] Implement `GET /templates/{reference}/source` - entrypoint source for the editor

## Phase 3: Render Functionality
- [x] Buffered-S3 render store (`S3BufferedRenderStorage`) + worker aggregation to `summary.json`
- [x] Implement `POST /render/{reference}` endpoint (optional `retain_days` override)

## Phase 4: Render History & Analytics
- [x] `GET /renders?limit=N` - recent renders (from the aggregate)
- [x] `GET /renders/{render_id}/pdf` - fetch rendered PDF from S3 (by render_id)
- [x] Analytics endpoints backed by `summary.json`:
  - [x] `GET /analytics/volume?days=30` - render volume over time
  - [x] `GET /analytics/templates` - total renders per template
  - [x] `GET /analytics/performance?days=30` - average duration over time

## Phase 5: Docker & Deployment
- [x] Multi-stage Dockerfiles (server + worker)
- [x] docker-compose.yml with SeaweedFS + papermake-worker for local dev (no ClickHouse)
- [x] Document required environment variables (see above)
- [ ] Add startup scripts and health checks

## Phase 6: Error Handling & Observability
- [x] Comprehensive error responses (404, 400, 500 with JSON structure)
- [ ] Request/response logging
- [ ] Metrics collection (render counts, durations, errors)
- [ ] Version tag immutability enforcement (409 Conflict for duplicate versions)

## Phase 7: Testing & Documentation
- [x] Integration tests with test S3/SeaweedFS (partially in papermake-registry)
- [ ] API documentation (OpenAPI/Swagger)
- [ ] Example templates and curl commands
- [ ] Performance testing under load

**Dependencies:**
- `papermake-registry` crate (completed phases 1-4)
- S3-compatible storage (SeaweedFS for dev)
- `papermake-worker` (aggregator + pruner)

## HTTP API Endpoints

### Server Configuration
- **Framework**: Axum with Tokio
- **Base URL**: All API routes under `/api`
- **CORS**: Permissive CORS enabled
- **Body Limit**: 50MB for large PDF uploads

### Route Structure
```
/health (GET)
/ (GET) - SSR dashboard
/templates/{reference} (GET) - SSR template detail (editor + test render + publish)
/ui/templates/{name}/render (POST) - htmx test-render fragment
/ui/templates/{name}/publish (POST) - publish form -> redirect
/assets/app.css, /assets/htmx.min.js - stylesheet + htmx, embedded in the binary
/api/
├── templates/
│   ├── / (GET) - List all templates
│   ├── /{name}/publish (POST) - Publish template (multipart)
│   ├── /{name}/publish-simple (POST) - Publish template (JSON)
│   ├── /{name}/tags (GET) - List template tags
│   ├── /{reference} (GET) - Get template metadata
│   └── /{reference}/source (GET) - Entrypoint source (text/plain)
├── render/
│   └── /{reference} (POST) - Render template to PDF (optional retain_days)
├── renders/
│   ├── / (GET) - List recent renders
│   └── /{render_id}/pdf (GET) - Download rendered PDF
└── analytics/
    ├── /volume?days=N (GET) - render volume over time
    ├── /templates (GET) - total renders per template
    └── /performance?days=N (GET) - average duration over time
```

### 1. Health Check
- **`GET /health`**
  - Returns server status, version, and timestamp
  - Response: JSON with health information

### 2. Template Management (`/api/templates`)

#### 2.1 List Templates
- **`GET /api/templates`**
  - Query parameters: `limit` (default: 50), `offset` (default: 0), `search`
  - Returns paginated list of templates
  - Response: `PaginatedResponse<TemplateInfo>`

#### 2.2 Publish Template (Multipart)
- **`POST /api/templates/{name}/publish?tag=latest`**
  - Content-Type: `multipart/form-data`
  - Form fields:
    - `main_typ`: Main template file (required)
    - `metadata`: JSON metadata with name and author (required)
    - `schema`: Optional JSON schema file
    - `files[]`: Additional template files (optional, multiple)
  - Response: `ApiResponse<PublishResponse>`

#### 2.3 Publish Template (JSON)
- **`POST /api/templates/{name}/publish-simple?tag=latest`**
  - Content-Type: `application/json`
  - JSON body:
    - `main_typ`: Template content as string
    - `schema`: Optional JSON schema object
    - `metadata`: Template metadata object
  - Response: `ApiResponse<PublishResponse>`

#### 2.4 List Template Tags
- **`GET /api/templates/{name}/tags`**
  - Returns all available tags for a template
  - Response: `ApiResponse<Vec<String>>`

#### 2.5 Get Template Metadata
- **`GET /api/templates/{reference}`**
  - Reference formats: `name`, `name:tag`, `namespace/name`, `namespace/name:tag`
  - Returns template metadata
  - Response: `ApiResponse<TemplateMetadataResponse>`

### 3. Template Rendering (`/api/render`)

#### 3.1 Render Template
- **`POST /api/render/{reference}`**
  - Content-Type: `application/json`
  - JSON body: `{"data": {...}}` - Data to inject into template
  - Returns PDF metadata with render_id
  - Response: `ApiResponse<RenderResponse>` with render_id, pdf_hash, duration_ms

### 4. Render History (`/api/renders`)

#### 4.1 Download Rendered PDF
- **`GET /api/renders/{render_id}/pdf`**
  - Downloads PDF file for specific render
  - Response: PDF file with `application/pdf` content-type

### 5. Analytics (`/api/analytics`)
- Backed by the S3 aggregate (`summary.json`); refreshed by the worker each cycle.
- **`GET /api/analytics/volume?days=N`** - render volume over time
- **`GET /api/analytics/templates`** - total renders per template
- **`GET /api/analytics/performance?days=N`** - average render duration over time

### Error Handling
- **400 Bad Request**: Invalid input or malformed requests
- **404 Not Found**: Template or resource not found (incl. unknown/pruned render_id)
- **422 Unprocessable Entity**: PDF requested for a render that failed
- **409 Conflict**: Version tag conflicts (planned)
- **500 Internal Server Error**: Server or storage errors
- All errors return JSON with error details

### Content-Addressable Features
- Templates stored with SHA-256 hashes
- Immutable content with mutable tag references
- Support for namespaced template references
- Server-side rendering with Typst engine
- PDF storage and retrieval by render ID
