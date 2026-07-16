//! Template management routes

use crate::{
    AppState,
    error::{ApiError, Result},
    models::api::{ApiResponse, PaginatedResponse, SearchQuery},
};
use axum::{
    Json, Router,
    body::Body,
    extract::{Multipart, Path, Query, State},
    http::header::CONTENT_TYPE,
    response::Response,
    routing::{get, post},
};
use papermake_registry::{
    TemplateInfo,
    bundle::{TemplateBundle, TemplateMetadata},
    reference::Reference,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;

/// Query parameters for publishing a template
#[derive(Debug, Deserialize)]
pub struct PublishParams {
    /// Tag for the template (defaults to "latest")
    #[serde(default = "default_tag")]
    pub tag: String,
}

fn default_tag() -> String {
    "latest".to_string()
}

/// Response after successfully publishing a template
#[derive(Debug, Serialize, ToSchema)]
pub struct PublishResponse {
    /// Success message
    pub message: String,
    /// The manifest hash of the published template
    pub manifest_hash: String,
    /// Template reference for future use
    pub reference: String,
}

/// Template metadata response for API
#[derive(Debug, Serialize, ToSchema)]
pub struct TemplateMetadataResponse {
    /// Template name
    pub name: String,
    /// Optional namespace
    pub namespace: Option<String>,
    /// Current tag being viewed
    pub tag: String,
    /// Available tags
    pub tags: Vec<String>,
    /// Manifest hash
    pub manifest_hash: String,
    /// Template metadata
    pub metadata: TemplateMetadata,
    /// Full template reference
    pub reference: String,
}

/// Create template routes
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_templates))
        .route("/{name}/publish", post(publish_template))
        .route("/{name}/tags", get(list_template_tags))
        .route("/{reference}/source", get(get_template_source))
        .route("/{reference}", get(get_template_metadata))
}

/// Get the entrypoint (`main.typ`) source for a template reference, for the
/// editor. GET /api/templates/{reference}/source (text/plain)
#[utoipa::path(
    get,
    path = "/api/templates/{reference}/source",
    tag = "templates",
    params(("reference" = String, Path, description = "Template reference")),
    responses(
        (status = 200, description = "Entrypoint source", content_type = "text/plain", body = String),
        (status = 404, description = "Template not found"),
    ),
)]
pub async fn get_template_source(
    State(state): State<AppState>,
    Path(reference): Path<String>,
) -> Result<Response<Body>> {
    let source = state.registry.get_template_source(&reference).await?;
    Ok(Response::builder()
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(source))
        .unwrap())
}

/// List all templates in the registry
///
/// GET /api/templates
/// Query parameters:
/// - limit: Maximum number of templates to return (default: 50)
/// - offset: Number of templates to skip (default: 0)
/// - search: Search term to filter templates by name
#[utoipa::path(
    get,
    path = "/api/templates",
    tag = "templates",
    params(
        ("limit" = Option<u32>, Query, description = "Max templates to return (default 50)"),
        ("offset" = Option<u32>, Query, description = "Templates to skip (default 0)"),
        ("search" = Option<String>, Query, description = "Filter by name"),
    ),
    responses(
        (status = 200, description = "Paginated templates", body = crate::models::api::PaginatedTemplates),
    ),
)]
pub async fn list_templates(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<PaginatedResponse<TemplateInfo>>> {
    let templates = state.registry.list_templates().await?;

    // Apply the case-insensitive name search when provided.
    let filtered_templates: Vec<TemplateInfo> = match &query.search {
        Some(term) => {
            let needle = term.to_lowercase();
            templates
                .into_iter()
                .filter(|t| t.full_name().to_lowercase().contains(&needle))
                .collect()
        }
        None => templates,
    };

    // Apply pagination
    let total = filtered_templates.len() as u32;
    let offset = query.pagination.offset as usize;
    let limit = query.pagination.limit as usize;

    let paginated_templates: Vec<TemplateInfo> = filtered_templates
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect();

    let response = PaginatedResponse::new(
        paginated_templates,
        query.pagination.limit,
        query.pagination.offset,
        Some(total),
    );

    Ok(Json(response))
}

/// Publish a new template or update an existing template with a new tag
///
/// POST /api/templates/{name}/publish?tag=latest
/// Content-Type: multipart/form-data
///
/// Form fields:
/// - main_typ: The main template file (required)
/// - metadata: JSON metadata with name and author (required)
/// - schema: Optional JSON schema file
/// - files[]: Additional template files (optional, multiple)
#[utoipa::path(
    post,
    path = "/api/templates/{name}/publish",
    tag = "templates",
    params(
        ("name" = String, Path, description = "Template name"),
        ("tag" = Option<String>, Query, description = "Tag to publish (default `latest`)"),
    ),
    request_body(content_type = "multipart/form-data", description = "Fields: main_typ (file), metadata (JSON), schema (file, optional), files[path] (optional, multiple)"),
    responses(
        (status = 200, description = "Template published", body = crate::models::api::PublishApiResponse),
        (status = 400, description = "Missing/invalid fields"),
    ),
)]
pub async fn publish_template(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(params): Query<PublishParams>,
    mut multipart: Multipart,
) -> Result<Json<ApiResponse<PublishResponse>>> {
    let mut main_typ: Option<Vec<u8>> = None;
    let mut metadata: Option<TemplateMetadata> = None;
    let mut schema: Option<Vec<u8>> = None;
    let mut files: HashMap<String, Vec<u8>> = HashMap::new();

    // Parse multipart form data
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(&format!("Failed to parse multipart data: {}", e)))?
    {
        let field_name = field.name().unwrap_or("").to_string();
        let data = field.bytes().await.map_err(|e| {
            ApiError::bad_request(&format!("Failed to read field '{}': {}", field_name, e))
        })?;

        match field_name.as_str() {
            "main_typ" => {
                main_typ = Some(data.to_vec());
            }
            "metadata" => {
                let metadata_str = String::from_utf8(data.to_vec())
                    .map_err(|_| ApiError::bad_request("Metadata must be valid UTF-8"))?;
                metadata = Some(serde_json::from_str(&metadata_str).map_err(|e| {
                    ApiError::bad_request(&format!("Invalid metadata JSON: {}", e))
                })?);
            }
            "schema" => {
                schema = Some(data.to_vec());
            }
            field_name if field_name.starts_with("files[") => {
                // Extract filename from field name like "files[components/header.typ]"
                if let Some(filename) = extract_filename_from_field(field_name) {
                    files.insert(filename, data.to_vec());
                }
            }
            _ => {
                // Ignore unknown fields
            }
        }
    }

    // Validate required fields
    let main_typ =
        main_typ.ok_or_else(|| ApiError::bad_request("Missing required field: main_typ"))?;

    let metadata =
        metadata.ok_or_else(|| ApiError::bad_request("Missing required field: metadata"))?;

    // Create template bundle
    let mut bundle = TemplateBundle::new(main_typ, metadata);

    // Add schema if provided
    if let Some(schema_data) = schema {
        bundle = bundle.with_schema(schema_data);
    }

    // Add additional files
    for (filename, file_data) in files {
        bundle = bundle.add_file(filename, file_data);
    }

    // Validate bundle before publishing
    bundle
        .validate()
        .map_err(|e| ApiError::bad_request(&format!("Template validation failed: {}", e)))?;

    // Publish the template
    let manifest_hash = state.registry.publish(bundle, &name, &params.tag).await?;

    let reference = format!("{}:{}", name, params.tag);
    let response_data = PublishResponse {
        message: format!("Template '{}' published successfully", reference),
        manifest_hash,
        reference: reference.clone(),
    };

    Ok(Json(ApiResponse::with_message(
        response_data,
        format!("Template published with reference '{}'", reference),
    )))
}

/// List all tags for a specific template
///
/// GET /api/templates/{name}/tags
#[utoipa::path(
    get,
    path = "/api/templates/{name}/tags",
    tag = "templates",
    params(("name" = String, Path, description = "Template name")),
    responses(
        (status = 200, description = "Available tags", body = crate::models::api::TagsApiResponse),
        (status = 404, description = "Template not found"),
    ),
)]
pub async fn list_template_tags(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<ApiResponse<Vec<String>>>> {
    let template = state
        .registry
        .get_template_info(&name)
        .await?
        .ok_or_else(|| ApiError::template_not_found(&name))?;

    Ok(Json(ApiResponse::new(template.tags)))
}

/// Get metadata for a specific template reference
///
/// GET /api/templates/{reference}
///
/// The reference can be:
/// - name (defaults to latest tag)
/// - name:tag
/// - namespace/name
/// - namespace/name:tag
#[utoipa::path(
    get,
    path = "/api/templates/{reference}",
    tag = "templates",
    params(("reference" = String, Path, description = "Template reference (name, name:tag, namespace/name[:tag])")),
    responses(
        (status = 200, description = "Template metadata", body = crate::models::api::TemplateMetadataApiResponse),
        (status = 400, description = "Invalid reference"),
        (status = 404, description = "Template not found"),
    ),
)]
pub async fn get_template_metadata(
    State(state): State<AppState>,
    Path(reference): Path<String>,
) -> Result<Json<ApiResponse<TemplateMetadataResponse>>> {
    // Parse the reference
    let parsed_ref = reference.parse::<Reference>().map_err(|e| {
        ApiError::bad_request(&format!(
            "Invalid template reference '{}': {}",
            reference, e
        ))
    })?;

    // Resolve the requested tag to its manifest hash.
    let manifest_hash = state.registry.resolve(&reference).await?;

    // Load just this template's info (tags + metadata), not the whole registry.
    let template = state
        .registry
        .get_template_info(&parsed_ref.full_name())
        .await?
        .ok_or_else(|| ApiError::template_not_found(&reference))?;

    let tag = parsed_ref.tag_or_default();
    let response_data = TemplateMetadataResponse {
        name: template.name.clone(),
        namespace: template.namespace.clone(),
        tag: tag.to_string(),
        tags: template.tags.clone(),
        manifest_hash,
        metadata: template.metadata.clone(),
        reference: format!("{}:{}", template.full_name(), tag),
    };

    Ok(Json(ApiResponse::new(response_data)))
}

/// Extract filename from multipart field name like "files[components/header.typ]"
fn extract_filename_from_field(field_name: &str) -> Option<String> {
    if field_name.starts_with("files[") && field_name.ends_with(']') {
        let filename = &field_name[6..field_name.len() - 1]; // Remove "files[" and "]"
        if !filename.is_empty() {
            return Some(filename.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_filename_from_field() {
        assert_eq!(
            extract_filename_from_field("files[main.typ]"),
            Some("main.typ".to_string())
        );

        assert_eq!(
            extract_filename_from_field("files[components/header.typ]"),
            Some("components/header.typ".to_string())
        );

        assert_eq!(
            extract_filename_from_field("files[assets/images/logo.png]"),
            Some("assets/images/logo.png".to_string())
        );

        assert_eq!(extract_filename_from_field("files[]"), None);
        assert_eq!(extract_filename_from_field("other_field"), None);
        assert_eq!(extract_filename_from_field("files["), None);
    }

    #[test]
    fn test_default_tag() {
        assert_eq!(default_tag(), "latest");
    }

    #[tokio::test]
    async fn list_templates_applies_search_filter() {
        use crate::test_support;
        use axum::{
            body::Body,
            http::{Request, StatusCode},
        };
        use tower::ServiceExt;

        let registry = test_support::registry();
        for name in ["invoice", "letter"] {
            registry
                .publish(test_support::bundle(), name, "latest")
                .await
                .unwrap();
        }
        let app = router().with_state(test_support::state(registry));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/?search=inv")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = test_support::response_json(response).await;
        let names: Vec<&str> = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["invoice"]);
    }
}
