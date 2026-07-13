use axum::{
    Json, Router,
    extract::{Path, State},
    routing::post,
};

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
pub struct BatchRenderRequest {
    /// One data payload per render.
    pub inputs: Vec<serde_json::Value>,
    /// Retention override in days applied to every render in the batch
    /// (`0` = keep forever). Falls back to template/global defaults when absent.
    #[serde(default)]
    pub retain_days: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct BatchRenderResponse {
    /// One render_id per input, in order. Fetch each PDF at
    /// `GET /api/renders/{render_id}/pdf`. A failed input still yields an id.
    pub render_ids: Vec<String>,
}

/// Render one template against many inputs, reusing a single warm Typst world.
/// Returns the render_ids; PDFs are fetched separately by id.
#[axum::debug_handler]
pub async fn batch_render(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    Json(request): Json<BatchRenderRequest>,
) -> ApiResult<Json<ApiResponse<BatchRenderResponse>>> {
    let render_ids = state
        .registry
        .batch_render_with_retention(&reference, &request.inputs, request.retain_days)
        .await
        .map_err(|e| ApiError::RenderFailed(e.to_string()))?;

    Ok(Json(ApiResponse::new(BatchRenderResponse { render_ids })))
}
