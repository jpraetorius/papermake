//! Common API types and utilities
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Standard pagination parameters
#[derive(Debug, Deserialize)]
pub struct PaginationQuery {
    #[serde(default = "default_limit")]
    pub limit: u32,

    #[serde(default)]
    pub offset: u32,
}

fn default_limit() -> u32 {
    50
}

/// Standard pagination response
#[derive(Debug, Serialize, ToSchema)]
pub struct PaginatedResponse<T> {
    pub data: Vec<T>,
    pub pagination: PaginationInfo,
}

// Concrete instantiations used as OpenAPI response bodies. utoipa derives a
// schema for each concrete `PaginatedResponse<T>` these name.
pub type PaginatedTemplates = PaginatedResponse<papermake_registry::TemplateInfo>;
pub type PaginatedRenders =
    PaginatedResponse<papermake_registry::render_storage::types::RenderRecord>;

/// Pagination metadata
#[derive(Debug, Serialize, ToSchema)]
pub struct PaginationInfo {
    pub limit: u32,
    pub offset: u32,
    pub total: Option<u32>,
    pub has_more: bool,
}

impl<T> PaginatedResponse<T> {
    pub fn new(data: Vec<T>, limit: u32, offset: u32, total: Option<u32>) -> Self {
        let has_more = match total {
            Some(t) => offset + (data.len() as u32) < t,
            None => data.len() as u32 == limit,
        };

        Self {
            data,
            pagination: PaginationInfo {
                limit,
                offset,
                total,
                has_more,
            },
        }
    }
}

/// Standard API response wrapper
#[derive(Debug, Serialize, ToSchema)]
pub struct ApiResponse<T> {
    pub data: T,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// Concrete `ApiResponse<T>` instantiations used as OpenAPI response bodies.
pub type PublishApiResponse = ApiResponse<crate::routes::templates::PublishResponse>;
pub type TagsApiResponse = ApiResponse<Vec<String>>;
pub type TemplateMetadataApiResponse =
    ApiResponse<crate::routes::templates::TemplateMetadataResponse>;
pub type RenderApiResponse = ApiResponse<crate::routes::render::RenderResponse>;
pub type BatchAcceptedApiResponse = ApiResponse<crate::routes::render::BatchAccepted>;
pub type JobViewApiResponse = ApiResponse<papermake_registry::batch::JobView>;
pub type JobItemsApiResponse = ApiResponse<Vec<papermake_registry::batch::BatchItem>>;

impl<T> ApiResponse<T> {
    pub fn new(data: T) -> Self {
        Self {
            data,
            message: None,
        }
    }

    pub fn with_message(data: T, message: String) -> Self {
        Self {
            data,
            message: Some(message),
        }
    }
}

/// Common query parameters for filtering
#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    #[serde(flatten)]
    pub pagination: PaginationQuery,

    /// Search term for name/content filtering
    #[allow(dead_code)]
    pub search: Option<String>,

    /// Sort field
    #[allow(dead_code)]
    pub sort_by: Option<String>,

    /// Sort direction
    #[allow(dead_code)]
    pub sort_order: Option<SortOrder>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum SortOrder {
    Asc,
    #[default]
    Desc,
}
