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

/// Upper bound on `limit` for `GET /api/renders`, so a caller can't request an
/// unbounded page (and `offset + limit + 1` can't overflow).
const MAX_RENDER_PAGE_LIMIT: u32 = 200;

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
        ("limit" = Option<u32>, Query, description = "Max records to return (default 50, max 200)"),
        ("offset" = Option<u32>, Query, description = "Records to skip (default 0)"),
    ),
    responses((status = 200, description = "Recent renders", body = crate::models::api::PaginatedRenders)),
)]
#[axum::debug_handler]
pub async fn list_renders(
    State(state): State<AppState>,
    Query(pagination): Query<PaginationQuery>,
) -> ApiResult<Json<PaginatedResponse<RenderRecord>>> {
    let limit = pagination.limit.clamp(1, MAX_RENDER_PAGE_LIMIT);
    let offset = pagination.offset;

    // `list_recent_renders` has no offset parameter, so fetch everything up to
    // the requested window plus one extra: `offset` rows to skip, `limit` rows
    // to return, and one more to tell whether a further page exists. Saturating
    // math keeps a large `offset` from overflowing.
    let fetch = offset.saturating_add(limit).saturating_add(1);

    let renders = state
        .registry
        .list_recent_renders(fetch)
        .await
        .map_err(|e| match e {
            papermake_registry::RegistryError::RenderStorage(_) => {
                ApiError::Internal("Failed to fetch render records".to_string())
            }
            _ => ApiError::Internal(e.to_string()),
        })?;

    // Skip the offset, then keep at most `limit`; the presence of the extra
    // fetched row after the offset means there is another page.
    let mut data: Vec<RenderRecord> = renders.into_iter().skip(offset as usize).collect();
    let has_more = data.len() > limit as usize;
    data.truncate(limit as usize);

    let response = PaginatedResponse {
        data,
        pagination: crate::models::api::PaginationInfo {
            limit,
            offset,
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

    /// Render `n` distinct inputs so there are `n` records to page through.
    async fn seed_renders(registry: &test_support::TestRegistry, n: usize) {
        registry
            .publish(test_support::bundle(), "invoice", "latest")
            .await
            .unwrap();
        for i in 0..n {
            registry
                .render_and_store(
                    "invoice:latest",
                    &serde_json::json!({ "name": format!("c{i}") }),
                )
                .await
                .unwrap();
        }
    }

    async fn list_page(app: &Router, query: &str) -> serde_json::Value {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/?{query}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        test_support::response_json(response).await
    }

    #[tokio::test]
    async fn list_renders_paginates_with_offset() {
        let registry = test_support::registry();
        seed_renders(&registry, 5).await;
        let app = router().with_state(test_support::state(registry));

        let ids_of = |page: &serde_json::Value| -> Vec<String> {
            page["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["render_id"].as_str().unwrap().to_string())
                .collect()
        };

        // Ground truth: the full ordered list of render ids.
        let full = list_page(&app, "limit=50&offset=0").await;
        let all = ids_of(&full);
        assert_eq!(all.len(), 5);

        // Each page is the matching slice of the full list — not skewed by offset.
        let page0 = list_page(&app, "limit=2&offset=0").await;
        assert_eq!(ids_of(&page0), all[0..2]);
        assert_eq!(page0["pagination"]["has_more"].as_bool(), Some(true));

        let page1 = list_page(&app, "limit=2&offset=2").await;
        assert_eq!(ids_of(&page1), all[2..4]);
        assert_eq!(page1["pagination"]["offset"].as_u64(), Some(2));
        assert_eq!(page1["pagination"]["has_more"].as_bool(), Some(true));

        // Final partial page: one record left, no further page.
        let page2 = list_page(&app, "limit=2&offset=4").await;
        assert_eq!(ids_of(&page2), all[4..5]);
        assert_eq!(page2["pagination"]["has_more"].as_bool(), Some(false));
    }

    #[tokio::test]
    async fn list_renders_clamps_limit_without_overflow() {
        let registry = test_support::registry();
        seed_renders(&registry, 3).await;
        let app = router().with_state(test_support::state(registry));

        // u32::MAX limit must neither overflow `offset + limit + 1` nor return an
        // unbounded page: the limit is clamped and all available rows come back.
        let page = list_page(&app, "limit=4294967295&offset=0").await;
        assert_eq!(
            page["pagination"]["limit"].as_u64(),
            Some(u64::from(MAX_RENDER_PAGE_LIMIT))
        );
        assert_eq!(page["data"].as_array().unwrap().len(), 3);
        assert_eq!(page["pagination"]["has_more"].as_bool(), Some(false));
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
