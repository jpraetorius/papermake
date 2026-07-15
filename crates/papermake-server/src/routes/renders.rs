use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::header::{CONTENT_DISPOSITION, CONTENT_TYPE},
    response::Response,
    routing::get,
};

use crate::{
    AppState,
    error::{ApiError, Result as ApiResult},
    models::api::{PaginatedResponse, PaginationQuery},
};

use papermake_registry::render_storage::types::RenderRecord;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_renders))
        .route("/{render_id}/pdf", get(get_render_pdf))
}

/// Handler for GET /api/renders - List recent renders with pagination
#[utoipa::path(
    get,
    path = "/api/renders",
    tag = "renders",
    params(
        ("limit" = Option<u32>, Query, description = "Max records to return (default 50)"),
        ("offset" = Option<u32>, Query, description = "Records to skip (default 0)"),
    ),
    responses((status = 200, description = "Recent renders", body = crate::models::api::PaginatedRenders)),
)]
#[axum::debug_handler]
pub async fn list_renders(
    State(state): State<AppState>,
    Query(pagination): Query<PaginationQuery>,
) -> ApiResult<Json<PaginatedResponse<RenderRecord>>> {
    let renders = state
        .registry
        .list_recent_renders(pagination.limit + 1) // Get one extra to check if there are more
        .await
        .map_err(|e| match e {
            papermake_registry::RegistryError::RenderStorage(_) => {
                ApiError::Internal("Failed to fetch render records".to_string())
            }
            _ => ApiError::Internal(e.to_string()),
        })?;

    // Apply offset manually since the registry method doesn't support it yet
    let mut data: Vec<RenderRecord> = renders
        .into_iter()
        .skip(pagination.offset as usize)
        .collect();

    // Check if there are more records and trim to requested limit
    let has_more = data.len() > pagination.limit as usize;
    data.truncate(pagination.limit as usize);

    let response = PaginatedResponse {
        data,
        pagination: crate::models::api::PaginationInfo {
            limit: pagination.limit,
            offset: pagination.offset,
            total: None, // We don't have total count yet
            has_more,
        },
    };

    Ok(Json(response))
}

/// Download a rendered PDF by render id.
#[utoipa::path(
    get,
    path = "/api/renders/{render_id}/pdf",
    tag = "renders",
    params(("render_id" = String, Path, description = "Render id")),
    responses(
        (status = 200, description = "PDF document", content_type = "application/pdf", body = [u8]),
        (status = 404, description = "Render not found"),
        (status = 422, description = "Render failed; no PDF available"),
    ),
)]
#[axum::debug_handler]
pub async fn get_render_pdf(
    State(state): State<AppState>,
    Path(render_id): Path<String>,
) -> ApiResult<Response<Body>> {
    let pdf_bytes = state
        .registry
        .get_render_pdf(&render_id)
        .await
        .map_err(|e| match e {
            papermake_registry::RegistryError::RenderStorage(_) => {
                ApiError::render_not_found(&render_id)
            }
            _ => ApiError::Internal(e.to_string()),
        })?;

    let filename = format!("render-{}.pdf", render_id);

    // `inline` so the PDF renders in the browser (e.g. the editor's <iframe>
    // preview). Explicit "download" links in the UI set the HTML `download`
    // attribute to force a save instead.
    Ok(Response::builder()
        .header(CONTENT_TYPE, "application/pdf")
        .header(
            CONTENT_DISPOSITION,
            format!("inline; filename=\"{}\"", filename),
        )
        .body(Body::from(pdf_bytes))
        .unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;
    use axum::{
        body::Body,
        http::{Request, StatusCode, header},
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn render_routes_list_recent_renders_and_serve_pdf() {
        let registry = test_support::registry();
        registry
            .publish(test_support::bundle(), "invoice", "latest")
            .await
            .unwrap();
        let rendered = registry
            .render_and_store("invoice:latest", &serde_json::json!({ "name": "Alice" }))
            .await
            .unwrap();
        let render_id = rendered.render_id.clone();
        let app = router().with_state(test_support::state(registry));

        let list = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/?limit=1&offset=0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(list.status(), StatusCode::OK);
        let list_body = test_support::response_json(list).await;
        assert_eq!(
            list_body["data"][0]["render_id"].as_str(),
            Some(render_id.as_str())
        );
        assert_eq!(list_body["pagination"]["limit"].as_u64(), Some(1));

        let pdf = app
            .oneshot(
                Request::builder()
                    .uri(format!("/{render_id}/pdf"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(pdf.status(), StatusCode::OK);
        assert_eq!(
            pdf.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/pdf"
        );
        assert!(
            pdf.headers()
                .get(header::CONTENT_DISPOSITION)
                .unwrap()
                .to_str()
                .unwrap()
                .contains(&render_id)
        );
        let pdf_bytes = test_support::response_bytes(pdf).await;
        assert!(pdf_bytes.starts_with(b"%PDF"));
    }

    #[tokio::test]
    async fn unknown_render_pdf_returns_not_found() {
        let app = router().with_state(test_support::state(test_support::registry()));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/missing/pdf")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
