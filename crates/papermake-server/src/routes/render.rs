use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::post,
};

use papermake_registry::{PdfStandard, RegistryError, RenderOptions, batch::BatchInput};
use serde::{Deserialize, Serialize};
use std::time::Instant;
use tracing::{error, info};
use utoipa::ToSchema;

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

#[derive(Debug, Deserialize, ToSchema)]
pub struct RenderRequest {
    /// Arbitrary JSON injected into the template as `sys.inputs.data`.
    #[schema(value_type = Object)]
    pub data: serde_json::Value,
    /// Per-render retention override in days (`0` = keep forever). Falls back to
    /// the template default, then the global default, when absent.
    #[serde(default)]
    pub retain_days: Option<u32>,
    /// Optional PDF standard for the output. One of `"1.7"` (default), `"2.0"`,
    /// `"a-2a"`, `"a-2b"`, `"a-3a"`, `"a-3b"`, `"a-4"`, `"ua-1"`. Absent = plain
    /// PDF 1.7.
    #[serde(default)]
    #[schema(value_type = Option<String>, example = "a-2b")]
    pub pdf_standard: Option<PdfStandard>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RenderResponse {
    pub render_id: String,
    pub pdf_hash: String,
    pub duration_ms: u32,
}

/// Render a template to PDF synchronously and store the result.
#[utoipa::path(
    post,
    path = "/api/render/{reference}",
    tag = "render",
    params(("reference" = String, Path, description = "Template reference, e.g. `invoice:latest`")),
    request_body = RenderRequest,
    responses(
        (status = 200, description = "Render succeeded", body = crate::models::api::RenderApiResponse),
        (status = 404, description = "Template not found"),
        (status = 408, description = "Render timed out"),
        (status = 422, description = "Template failed to compile"),
        (status = 500, description = "Server or storage error"),
    ),
)]
#[axum::debug_handler]
pub async fn render_template(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    Json(request): Json<RenderRequest>,
) -> ApiResult<Json<ApiResponse<RenderResponse>>> {
    let data_size_bytes = serde_json::to_vec(&request.data)
        .map(|bytes| bytes.len())
        .unwrap_or_default();
    let started = Instant::now();

    info!(
        reference = %reference,
        data_size_bytes,
        retain_days = ?request.retain_days,
        pdf_standard = ?request.pdf_standard,
        "render request accepted",
    );

    let options = RenderOptions {
        pdf_standards: request.pdf_standard.map(|s| vec![s]).unwrap_or_default(),
    };

    let result = match state
        .registry
        .render_and_store_with(&reference, &request.data, request.retain_days, &options)
        .await
    {
        Ok(result) => {
            info!(
                reference = %reference,
                render_id = %result.render_id,
                pdf_hash = %result.pdf_hash,
                render_duration_ms = result.duration_ms,
                wall_time_ms = started.elapsed().as_millis() as u64,
                "render request completed",
            );
            result
        }
        Err(e) => {
            error!(
                reference = %reference,
                wall_time_ms = started.elapsed().as_millis() as u64,
                error = %e,
                "render request failed",
            );
            return Err(match e {
                RegistryError::RenderTimeout { .. } => ApiError::Timeout,
                other => ApiError::RenderFailed(other.to_string()),
            });
        }
    };

    let response = RenderResponse {
        render_id: result.render_id,
        pdf_hash: result.pdf_hash,
        duration_ms: result.duration_ms,
    };

    Ok(Json(ApiResponse::new(response)))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct BatchInputRequest {
    /// Data payload for this render.
    #[schema(value_type = Object)]
    pub data: serde_json::Value,
    /// Optional caller-chosen key echoed back on the result item, so results
    /// map to your own ids without relying on order.
    #[serde(default)]
    pub key: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct BatchRenderRequest {
    /// One item per render.
    pub inputs: Vec<BatchInputRequest>,
    /// Retention override in days applied to every render in the batch
    /// (`0` = keep forever). Falls back to template/global defaults when absent.
    #[serde(default)]
    pub retain_days: Option<u32>,
    /// Optional PDF standard applied to every render in the batch. One of
    /// `"1.7"` (default), `"2.0"`, `"a-2a"`, `"a-2b"`, `"a-3a"`, `"a-3b"`,
    /// `"a-4"`, `"ua-1"`. Absent = plain PDF 1.7.
    #[serde(default)]
    #[schema(value_type = Option<String>, example = "a-2b")]
    pub pdf_standard: Option<PdfStandard>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BatchAccepted {
    /// Poll the job at `GET /api/jobs/{job_id}`.
    pub job_id: String,
    pub total: usize,
    pub status_url: String,
}

/// Submit an async batch render. Returns `202 Accepted` with a `job_id`. The
/// job is durably enqueued in S3; a worker claims and renders it, updating the
/// persisted job document. Poll `GET /api/jobs/{job_id}` and fetch each PDF by
/// `render_id`.
#[utoipa::path(
    post,
    path = "/api/render/{reference}/batch",
    tag = "render",
    params(("reference" = String, Path, description = "Template reference, e.g. `invoice:latest`")),
    request_body = BatchRenderRequest,
    responses(
        (status = 202, description = "Batch accepted; poll the job", body = crate::models::api::BatchAcceptedApiResponse),
        (status = 500, description = "Failed to enqueue the job"),
    ),
)]
#[axum::debug_handler]
pub async fn batch_render(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    Json(request): Json<BatchRenderRequest>,
) -> ApiResult<(StatusCode, Json<ApiResponse<BatchAccepted>>)> {
    info!(
        reference = %reference,
        total = request.inputs.len(),
        retain_days = ?request.retain_days,
        pdf_standard = ?request.pdf_standard,
        "batch render request accepted",
    );

    let inputs: Vec<BatchInput> = request
        .inputs
        .into_iter()
        .map(|i| BatchInput {
            data: i.data,
            key: i.key,
        })
        .collect();

    let options = RenderOptions {
        pdf_standards: request.pdf_standard.map(|s| vec![s]).unwrap_or_default(),
    };

    // Enqueue only — a worker picks it up. Servers never render batches, so
    // scaling the API is safe.
    let job = state
        .registry
        .enqueue_batch_job(&reference, &inputs, request.retain_days, &options)
        .await
        .map_err(|e| {
            error!(
                reference = %reference,
                error = %e,
                "failed to create batch render job",
            );
            ApiError::Internal(e.to_string())
        })?;

    let accepted = BatchAccepted {
        status_url: format!("/api/jobs/{}", job.job_id),
        job_id: job.job_id,
        total: job.total,
    };
    Ok((StatusCode::ACCEPTED, Json(ApiResponse::new(accepted))))
}
