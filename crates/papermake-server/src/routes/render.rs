use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::post,
};

use papermake_registry::{PdfStandard, RenderOptions, batch::BatchInput};
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
    /// Arbitrary JSON made available to the template as `data`.
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
        (status = 400, description = "Malformed template reference"),
        (status = 404, description = "Template not found"),
        (status = 408, description = "Render timed out"),
        (status = 422, description = "Template failed to compile"),
        (status = 500, description = "Server or storage error"),
        (status = 503, description = "Render capacity temporarily exhausted"),
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
            // Defer to the centralized mapping in `error.rs`, which honors the
            // documented contract (404 unknown template, 408 timeout, 422
            // compile failure, 500 storage/internal) and never leaks S3 or
            // internal key details for server-side errors.
            return Err(ApiError::Registry(e));
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
        (status = 400, description = "Empty batch, too many inputs, or an oversized item"),
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

    // Bound the batch before doing any work: an unbounded count fans out into
    // tens of thousands of synchronous S3 writes here and unbounded worker/
    // storage amplification. A 50 MiB body alone can pack millions of tiny
    // `{"data":{}}` items.
    if request.inputs.is_empty() {
        return Err(ApiError::bad_request(
            "Batch must contain at least one input",
        ));
    }
    if request.inputs.len() > state.config.max_batch_inputs {
        return Err(ApiError::bad_request(&format!(
            "Batch has {} inputs; the maximum is {}",
            request.inputs.len(),
            state.config.max_batch_inputs,
        )));
    }

    let max_item_bytes = state.config.max_batch_item_bytes;
    let mut inputs: Vec<BatchInput> = Vec::with_capacity(request.inputs.len());
    for (index, item) in request.inputs.into_iter().enumerate() {
        let item_bytes = serde_json::to_vec(&item.data)?;
        if item_bytes.len() > max_item_bytes {
            return Err(ApiError::bad_request(&format!(
                "Batch item {index} data is {} bytes; the maximum is {} bytes",
                item_bytes.len(),
                max_item_bytes,
            )));
        }
        inputs.push(BatchInput {
            data: item.data,
            key: item.key,
        });
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;
    use axum::{
        body::Body,
        http::{Request, StatusCode, header::CONTENT_TYPE},
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn render_template_returns_render_metadata_for_published_template() {
        let registry = test_support::registry();
        registry
            .publish(test_support::bundle(), "invoice", "latest")
            .await
            .unwrap();
        let app = router().with_state(test_support::state(registry));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/invoice:latest")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"data":{"name":"Alice"},"retain_days":0,"pdf_standard":"1.7"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = test_support::response_json(response).await;
        assert!(body["data"]["render_id"].as_str().unwrap().len() > 10);
        assert!(
            body["data"]["pdf_hash"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );
        assert!(body["data"]["duration_ms"].as_u64().is_some());
    }

    #[tokio::test]
    async fn render_template_reports_missing_template_as_not_found() {
        let app = router().with_state(test_support::state(test_support::registry()));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/missing:latest")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"data":{"name":"Alice"}}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = test_support::response_json(response).await;
        assert_eq!(body["status"].as_u64(), Some(404));
    }

    #[tokio::test]
    async fn render_template_treats_explicit_plain_pdf_as_default() {
        let registry = test_support::registry();
        registry
            .publish(test_support::bundle(), "invoice", "latest")
            .await
            .unwrap();
        let app = router().with_state(test_support::state(registry));

        let render = |body: &'static str| {
            let app = app.clone();
            async move {
                let response = app
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/invoice:latest")
                            .header(CONTENT_TYPE, "application/json")
                            .body(Body::from(body))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                test_support::response_json(response).await
            }
        };

        let default = render(r#"{"data":{"name":"Alice"}}"#).await;
        let explicit = render(r#"{"data":{"name":"Alice"},"pdf_standard":"1.7"}"#).await;

        assert_eq!(default["data"]["render_id"], explicit["data"]["render_id"]);
        assert_eq!(default["data"]["pdf_hash"], explicit["data"]["pdf_hash"]);
    }

    #[tokio::test]
    async fn render_template_reports_compile_failure_as_unprocessable() {
        let registry = test_support::registry();
        registry
            .publish(test_support::bundle(), "invoice", "latest")
            .await
            .unwrap();
        let app = router().with_state(test_support::state(registry));

        // The template reads `data.name`; omitting it makes the render fail to
        // compile. That is a 422 (the diagnostic is the product), not a 404.
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/invoice:latest")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"data":{}}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = test_support::response_json(response).await;
        assert_eq!(body["status"].as_u64(), Some(422));
    }

    #[tokio::test]
    async fn batch_render_enqueues_job_and_returns_poll_url() {
        let registry = test_support::registry();
        registry
            .publish(test_support::bundle(), "invoice", "latest")
            .await
            .unwrap();
        let app = router().with_state(test_support::state(registry));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/invoice:latest/batch")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"inputs":[{"key":"cust-1","data":{"name":"Alice"}}],"retain_days":0}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = test_support::response_json(response).await;
        let job_id = body["data"]["job_id"].as_str().unwrap();
        assert!(!job_id.is_empty());
        assert_eq!(body["data"]["total"].as_u64(), Some(1));
        assert_eq!(
            body["data"]["status_url"].as_str(),
            Some(format!("/api/jobs/{job_id}").as_str())
        );
    }

    /// A published-template `AppState`, with an optional config tweak applied.
    async fn published_state(configure: impl FnOnce(&mut crate::AppState)) -> crate::AppState {
        let registry = test_support::registry();
        registry
            .publish(test_support::bundle(), "invoice", "latest")
            .await
            .unwrap();
        let mut state = test_support::state(registry);
        configure(&mut state);
        state
    }

    async fn batch_status(state: crate::AppState, body: &'static str) -> StatusCode {
        router()
            .with_state(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/invoice:latest/batch")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn batch_render_rejects_empty_too_many_and_oversized_inputs() {
        // Empty batch → 400.
        let state = published_state(|_| {}).await;
        assert_eq!(
            batch_status(state, r#"{"inputs":[]}"#).await,
            StatusCode::BAD_REQUEST
        );

        // More inputs than the cap → 400.
        let state = published_state(|s| s.config.max_batch_inputs = 2).await;
        assert_eq!(
            batch_status(state, r#"{"inputs":[{"data":{}},{"data":{}},{"data":{}}]}"#).await,
            StatusCode::BAD_REQUEST
        );

        // A single oversized item → 400.
        let state = published_state(|s| s.config.max_batch_item_bytes = 8).await;
        assert_eq!(
            batch_status(
                state,
                r#"{"inputs":[{"data":{"name":"far longer than eight bytes"}}]}"#,
            )
            .await,
            StatusCode::BAD_REQUEST
        );

        // Within limits → still accepted.
        let state = published_state(|_| {}).await;
        assert_eq!(
            batch_status(state, r#"{"inputs":[{"data":{"name":"ok"}}]}"#).await,
            StatusCode::ACCEPTED
        );
    }
}
