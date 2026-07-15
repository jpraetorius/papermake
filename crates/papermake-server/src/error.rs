//! Error handling for the API server

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use papermake_registry::RegistryError;
use papermake_registry::render_storage::types::RenderStorageError;
use serde_json::json;
use thiserror::Error;
use tracing::{error, warn};

/// Result type for API operations
pub type Result<T> = std::result::Result<T, ApiError>;

/// API error types
#[derive(Debug, Error)]
pub enum ApiError {
    #[error("Template not found: {0}")]
    TemplateNotFound(String),

    #[error("Render job not found: {0}")]
    RenderNotFound(String),

    #[error("Render job failed: {0}")]
    RenderFailed(String),

    #[error("Registry error: {0}")]
    Registry(#[from] RegistryError),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Internal server error: {0}")]
    Internal(String),

    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Papermake error: {0}")]
    Papermake(#[from] papermake::PapermakeError),

    #[error("Timeout error: operation timed out")]
    Timeout,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let debug_error = format!("{:?}", self);
        let (status, error_message) = match &self {
            ApiError::TemplateNotFound(_) | ApiError::RenderNotFound(_) => {
                (StatusCode::NOT_FOUND, self.to_string())
            }
            ApiError::Validation(_) | ApiError::BadRequest(_) => {
                (StatusCode::BAD_REQUEST, self.to_string())
            }
            ApiError::Config(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Configuration error".to_string(),
            ),
            ApiError::Registry(e) => match e {
                RegistryError::RenderTimeout { .. } => {
                    (StatusCode::REQUEST_TIMEOUT, self.to_string())
                }
                RegistryError::Template(_) => (StatusCode::NOT_FOUND, self.to_string()),
                RegistryError::RenderStorage(RenderStorageError::NotFound(_)) => {
                    (StatusCode::NOT_FOUND, self.to_string())
                }
                RegistryError::RenderStorage(RenderStorageError::RenderFailed(_)) => {
                    (StatusCode::UNPROCESSABLE_ENTITY, self.to_string())
                }
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Registry error".to_string(),
                ),
            },
            ApiError::RenderFailed(_) => (StatusCode::UNPROCESSABLE_ENTITY, self.to_string()),
            ApiError::Timeout => (StatusCode::REQUEST_TIMEOUT, "Request timed out".to_string()),
            ApiError::Serialization(_) => {
                (StatusCode::BAD_REQUEST, "Invalid JSON format".to_string())
            }
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            ),
        };

        if status.is_server_error() {
            error!(
                status = status.as_u16(),
                error = %self,
                debug_error = %debug_error,
                "api error response",
            );
        } else if status.is_client_error() {
            warn!(
                status = status.as_u16(),
                error = %self,
                debug_error = %debug_error,
                "api error response",
            );
        }

        let body = Json(json!({
            "error": error_message,
            "status": status.as_u16()
        }));

        (status, body).into_response()
    }
}

// Convenience functions for common errors
impl ApiError {
    pub fn template_not_found(id: &str) -> Self {
        Self::TemplateNotFound(id.to_string())
    }

    pub fn render_not_found(id: &str) -> Self {
        Self::RenderNotFound(id.to_string())
    }

    pub fn bad_request(msg: &str) -> Self {
        Self::BadRequest(msg.to_string())
    }

    pub fn internal(msg: &str) -> Self {
        Self::Internal(msg.to_string())
    }

    pub fn validation(msg: &str) -> Self {
        Self::Validation(msg.to_string())
    }
}

#[cfg(test)]
mod tests {
    use axum::{body::to_bytes, response::IntoResponse};
    use serde_json::Value;

    use super::*;

    async fn response_json(error: ApiError) -> (StatusCode, Value) {
        let response = error.into_response();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = serde_json::from_slice(&bytes).unwrap();
        (status, body)
    }

    #[tokio::test]
    async fn not_found_errors_are_reported_as_not_found() {
        let (status, body) = response_json(ApiError::template_not_found("invoice")).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            body["status"].as_u64(),
            Some(u64::from(StatusCode::NOT_FOUND.as_u16()))
        );
        assert!(body["error"].as_str().unwrap().contains("invoice"));
    }

    #[tokio::test]
    async fn request_errors_are_reported_as_bad_request() {
        for error in [
            ApiError::bad_request("missing field"),
            ApiError::validation("invalid field"),
        ] {
            let (status, body) = response_json(error).await;

            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(
                body["status"].as_u64(),
                Some(u64::from(StatusCode::BAD_REQUEST.as_u16()))
            );
        }
    }

    #[tokio::test]
    async fn internal_configuration_details_are_redacted() {
        let (status, body) = response_json(ApiError::Config("secret token".to_string())).await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body["status"].as_u64(),
            Some(u64::from(StatusCode::INTERNAL_SERVER_ERROR.as_u16()))
        );
        assert_eq!(body["error"], "Configuration error");
    }

    #[tokio::test]
    async fn generic_internal_errors_are_redacted() {
        let (status, body) = response_json(ApiError::internal("database password")).await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body["status"].as_u64(),
            Some(u64::from(StatusCode::INTERNAL_SERVER_ERROR.as_u16()))
        );
        assert_eq!(body["error"], "Internal server error");
    }

    #[tokio::test]
    async fn timeout_errors_have_timeout_status() {
        let (status, body) = response_json(ApiError::Timeout).await;

        assert_eq!(status, StatusCode::REQUEST_TIMEOUT);
        assert_eq!(
            body["status"].as_u64(),
            Some(u64::from(StatusCode::REQUEST_TIMEOUT.as_u16()))
        );
    }
}
