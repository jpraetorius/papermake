//! Batch-render job status.

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use papermake_registry::RegistryError;
use papermake_registry::batch::BatchJob;

use crate::{
    AppState,
    error::{ApiError, Result as ApiResult},
    models::ApiResponse,
};

pub fn router() -> Router<AppState> {
    Router::new().route("/{job_id}", get(get_job))
}

/// GET /api/jobs/{job_id} — poll a batch job. The document is persisted in S3,
/// so this returns the full result whether the job is still running or done.
#[axum::debug_handler]
pub async fn get_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> ApiResult<Json<ApiResponse<BatchJob>>> {
    let job = state
        .registry
        .get_batch_job(&job_id)
        .await
        .map_err(|e| match e {
            RegistryError::RenderStorage(_) => ApiError::render_not_found(&job_id),
            other => ApiError::Internal(other.to_string()),
        })?;
    Ok(Json(ApiResponse::new(job)))
}
