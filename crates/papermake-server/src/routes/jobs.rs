//! Batch-render job status.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use papermake_registry::RegistryError;
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

fn map_job_err(job_id: &str) -> impl Fn(RegistryError) -> ApiError + '_ {
    move |e| match e {
        RegistryError::RenderStorage(_) => ApiError::render_not_found(job_id),
        other => ApiError::Internal(other.to_string()),
    }
}

/// GET /api/jobs/{job_id} — poll a batch job's aggregated status and counts
/// (derived from its shard descriptors). Cheap regardless of batch size.
#[axum::debug_handler]
pub async fn get_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> ApiResult<Json<ApiResponse<JobView>>> {
    let job = state
        .registry
        .get_batch_job(&job_id)
        .await
        .map_err(map_job_err(&job_id))?;
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
        .map_err(map_job_err(&job_id))?;
    Ok(Json(ApiResponse::new(items)))
}
