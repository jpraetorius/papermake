use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::post,
};

use papermake_registry::batch::BatchInput;
use serde::{Deserialize, Serialize};

use crate::{
    AppState,
    error::{ApiError, Result as ApiResult},
    models::ApiResponse,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/{reference}", post(render_template))
        .route("/{reference}/batch", post(batch_render))
}

#[derive(Debug, Deserialize)]
pub struct RenderRequest {
    pub data: serde_json::Value,
    /// Per-render retention override in days (`0` = keep forever). Falls back to
    /// the template default, then the global default, when absent.
    #[serde(default)]
    pub retain_days: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct RenderResponse {
    pub render_id: String,
    pub pdf_hash: String,
    pub duration_ms: u32,
}

#[axum::debug_handler]
pub async fn render_template(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    Json(request): Json<RenderRequest>,
) -> ApiResult<Json<ApiResponse<RenderResponse>>> {
    let result = state
        .registry
        .render_and_store_with_retention(&reference, &request.data, request.retain_days)
        .await
        .map_err(|e| ApiError::RenderFailed(e.to_string()))?;

    let response = RenderResponse {
        render_id: result.render_id,
        pdf_hash: result.pdf_hash,
        duration_ms: result.duration_ms,
    };

    Ok(Json(ApiResponse::new(response)))
}

#[derive(Debug, Deserialize)]
pub struct BatchInputRequest {
    /// Data payload for this render.
    pub data: serde_json::Value,
    /// Optional caller-chosen key echoed back on the result item, so results
    /// map to your own ids without relying on order.
    #[serde(default)]
    pub key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BatchRenderRequest {
    /// One item per render.
    pub inputs: Vec<BatchInputRequest>,
    /// Retention override in days applied to every render in the batch
    /// (`0` = keep forever). Falls back to template/global defaults when absent.
    #[serde(default)]
    pub retain_days: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct BatchAccepted {
    /// Poll the job at `GET /api/jobs/{job_id}`.
    pub job_id: String,
    pub total: usize,
    pub status_url: String,
}

/// Submit an async batch render. Returns `202 Accepted` with a `job_id`; the
/// job renders in the background (one warm world) and its document is persisted
/// in S3, so clients can poll it during or after the run and fetch each PDF by
/// `render_id`.
#[axum::debug_handler]
pub async fn batch_render(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    Json(request): Json<BatchRenderRequest>,
) -> ApiResult<(StatusCode, Json<ApiResponse<BatchAccepted>>)> {
    let inputs: Vec<BatchInput> = request
        .inputs
        .into_iter()
        .map(|i| BatchInput {
            data: i.data,
            key: i.key,
        })
        .collect();

    let job = state
        .registry
        .create_batch_job(&reference, &inputs)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let job_id = job.job_id.clone();
    let total = job.total;

    // Render in the background; run_batch_job updates the persisted job doc as
    // it goes and writes the final Completed state.
    let registry = state.registry.clone();
    let retain = request.retain_days;
    let log_id = job_id.clone();
    tokio::spawn(async move {
        if let Err(e) = registry.run_batch_job(job, inputs, retain).await {
            tracing::error!("batch job {} failed: {}", log_id, e);
        }
    });

    let accepted = BatchAccepted {
        status_url: format!("/api/jobs/{}", job_id),
        job_id,
        total,
    };
    Ok((StatusCode::ACCEPTED, Json(ApiResponse::new(accepted))))
}
