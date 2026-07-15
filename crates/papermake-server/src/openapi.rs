//! Generated OpenAPI 3.1 document.
//!
//! The spec is derived from the `#[utoipa::path]` annotations on the handlers
//! and the `ToSchema` derives on the request/response types — so it stays in
//! sync with the implementation. Served as JSON at `/api/openapi.json`; point
//! any OpenAPI client (Scalar, Swagger UI, Redoc, `openapi-generator`, …) at it.

use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Papermake Server API",
        description = "Content-addressable Typst template registry with server-side PDF rendering.",
        version = env!("CARGO_PKG_VERSION"),
    ),
    paths(
        crate::health_check,
        crate::routes::templates::list_templates,
        crate::routes::templates::publish_template,
        crate::routes::templates::list_template_tags,
        crate::routes::templates::get_template_source,
        crate::routes::templates::get_template_metadata,
        crate::routes::render::render_template,
        crate::routes::render::batch_render,
        crate::routes::renders::list_renders,
        crate::routes::renders::get_render_pdf,
        crate::routes::jobs::get_job,
        crate::routes::jobs::get_job_items,
        crate::routes::analytics::volume,
        crate::routes::analytics::templates,
        crate::routes::analytics::performance,
    ),
    components(schemas(
        // Server request/response DTOs
        crate::routes::render::RenderRequest,
        crate::routes::render::RenderResponse,
        crate::routes::render::BatchInputRequest,
        crate::routes::render::BatchRenderRequest,
        crate::routes::render::BatchAccepted,
        crate::routes::templates::PublishResponse,
        crate::routes::templates::TemplateMetadataResponse,
        crate::models::api::PaginationInfo,
        // Concrete generic wrappers (aliases)
        crate::models::api::PaginatedTemplates,
        crate::models::api::PaginatedRenders,
        crate::models::api::PublishApiResponse,
        crate::models::api::TagsApiResponse,
        crate::models::api::TemplateMetadataApiResponse,
        crate::models::api::RenderApiResponse,
        crate::models::api::BatchAcceptedApiResponse,
        crate::models::api::JobViewApiResponse,
        crate::models::api::JobItemsApiResponse,
        // Registry domain types on the wire
        papermake_registry::TemplateInfo,
        papermake_registry::bundle::TemplateMetadata,
        papermake_registry::render_storage::types::RenderRecord,
        papermake_registry::render_storage::types::VolumePoint,
        papermake_registry::render_storage::types::TemplateStats,
        papermake_registry::render_storage::types::DurationPoint,
        papermake_registry::render_storage::types::AnalyticsResult,
        papermake_registry::batch::JobView,
        papermake_registry::batch::BatchItem,
        papermake_registry::batch::JobStatus,
        papermake_registry::batch::ItemStatus,
    )),
    tags(
        (name = "health", description = "Liveness"),
        (name = "templates", description = "Template management"),
        (name = "render", description = "Synchronous and batch rendering"),
        (name = "renders", description = "Render history and PDF download"),
        (name = "jobs", description = "Batch job status"),
        (name = "analytics", description = "Aggregate render analytics"),
    ),
)]
pub struct ApiDoc;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_document_builds_and_covers_key_paths() {
        let doc = ApiDoc::openapi();
        let json = serde_json::to_string(&doc).expect("spec serializes to JSON");
        for path in [
            "/api/render/{reference}",
            "/api/render/{reference}/batch",
            "/api/templates",
            "/api/jobs/{job_id}",
            "/api/analytics/volume",
            "/health",
        ] {
            assert!(json.contains(path), "spec missing path {path}");
        }
        assert!(
            !json.contains("publish-simple"),
            "spec should not include removed publish-simple endpoint"
        );
        // PDF/A option surfaced on the render request.
        assert!(json.contains("pdf_standard"), "spec missing pdf_standard");
    }
}
