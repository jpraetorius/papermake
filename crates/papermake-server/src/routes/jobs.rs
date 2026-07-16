//! Batch-render job status.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use papermake_registry::batch::{BatchItem, JobView};
use serde::Deserialize;

use crate::{
    AppState,
    error::{ApiError, Result as ApiResult},
    models::ApiResponse,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/{job_id}", get(get_job))
        .route("/{job_id}/items", get(get_job_items))
}

/// GET /api/jobs/{job_id} — poll a batch job's aggregated status and counts
/// (derived from its shard descriptors). Cheap regardless of batch size.
#[utoipa::path(
    get,
    path = "/api/jobs/{job_id}",
    tag = "jobs",
    params(("job_id" = String, Path, description = "Batch job id")),
    responses(
        (status = 200, description = "Aggregated job view", body = crate::models::api::JobViewApiResponse),
        (status = 404, description = "Job not found"),
    ),
)]
#[axum::debug_handler]
pub async fn get_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> ApiResult<Json<ApiResponse<JobView>>> {
    let job = state
        .registry
        .get_batch_job(&job_id)
        .await
        .map_err(ApiError::Registry)?;
    Ok(Json(ApiResponse::new(job)))
}

#[derive(Debug, Deserialize)]
pub struct ItemsQuery {
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    1000
}

/// GET /api/jobs/{job_id}/items?offset=&limit= — a page of the item→render_id
/// mapping (ordered by input index). Only completed shards' items appear; poll
/// until the job is `completed` for the full set. Paginated so large batches
/// (100k+) don't return one giant document.
#[utoipa::path(
    get,
    path = "/api/jobs/{job_id}/items",
    tag = "jobs",
    params(
        ("job_id" = String, Path, description = "Batch job id"),
        ("offset" = Option<usize>, Query, description = "Items to skip (default 0)"),
        ("limit" = Option<usize>, Query, description = "Max items to return (default 1000)"),
    ),
    responses(
        (status = 200, description = "Item → render_id page", body = crate::models::api::JobItemsApiResponse),
        (status = 404, description = "Job not found"),
    ),
)]
#[axum::debug_handler]
pub async fn get_job_items(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
    Query(q): Query<ItemsQuery>,
) -> ApiResult<Json<ApiResponse<Vec<BatchItem>>>> {
    let items = state
        .registry
        .list_job_items(&job_id, q.offset, q.limit)
        .await
        .map_err(ApiError::Registry)?;
    Ok(Json(ApiResponse::new(items)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use papermake_registry::{RenderOptions, batch::BatchInput};
    use tower::ServiceExt;

    #[tokio::test]
    async fn job_routes_return_status_and_item_page() {
        let registry = test_support::registry();
        registry
            .publish(test_support::bundle(), "invoice", "latest")
            .await
            .unwrap();
        let job = registry
            .enqueue_batch_job(
                "invoice:latest",
                &[BatchInput {
                    data: serde_json::json!({ "name": "Alice" }),
                    key: Some("cust-1".to_string()),
                }],
                Some(0),
                &RenderOptions::default(),
            )
            .await
            .unwrap();
        let app = router().with_state(test_support::state(registry));

        let status = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/{}", job.job_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(status.status(), StatusCode::OK);
        let status_body = test_support::response_json(status).await;
        assert_eq!(
            status_body["data"]["job_id"].as_str(),
            Some(job.job_id.as_str())
        );
        assert_eq!(status_body["data"]["total"].as_u64(), Some(1));

        let items = app
            .oneshot(
                Request::builder()
                    .uri(format!("/{}/items", job.job_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(items.status(), StatusCode::OK);
        let items_body = test_support::response_json(items).await;
        assert_eq!(items_body["data"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn missing_job_returns_not_found() {
        let app = router().with_state(test_support::state(test_support::registry()));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/missing-job")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
