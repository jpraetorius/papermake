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

    /// Search term filtering templates by (full) name, case-insensitive.
    pub search: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn pagination_query_defaults_to_first_reasonable_page() {
        let query: PaginationQuery = serde_json::from_value(json!({})).unwrap();

        assert_eq!(query.limit, 50);
        assert_eq!(query.offset, 0);
    }

    #[test]
    fn paginated_response_reports_more_when_total_exceeds_returned_window() {
        let response = PaginatedResponse::new(vec![1, 2], 2, 4, Some(7));

        assert_eq!(response.data, vec![1, 2]);
        assert_eq!(response.pagination.limit, 2);
        assert_eq!(response.pagination.offset, 4);
        assert_eq!(response.pagination.total, Some(7));
        assert!(response.pagination.has_more);
    }

    #[test]
    fn paginated_response_reports_end_when_total_is_exhausted() {
        let response = PaginatedResponse::new(vec![1], 2, 4, Some(5));

        assert!(!response.pagination.has_more);
    }

    #[test]
    fn paginated_response_uses_full_page_as_unknown_total_signal() {
        let full_page = PaginatedResponse::new(vec![1, 2], 2, 0, None);
        let partial_page = PaginatedResponse::new(vec![1], 2, 0, None);

        assert!(full_page.pagination.has_more);
        assert!(!partial_page.pagination.has_more);
    }

    #[test]
    fn api_response_omits_message_until_one_is_provided() {
        let plain = serde_json::to_value(ApiResponse::new(json!({"id": 1}))).unwrap();
        let with_message = serde_json::to_value(ApiResponse::with_message(
            json!({"id": 1}),
            "created".to_string(),
        ))
        .unwrap();

        assert_eq!(plain["data"], json!({"id": 1}));
        assert!(plain.get("message").is_none());
        assert_eq!(with_message["data"], json!({"id": 1}));
        assert_eq!(with_message["message"], "created");
    }
}
