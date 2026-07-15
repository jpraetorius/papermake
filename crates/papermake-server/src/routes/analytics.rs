//! Analytics JSON API, backed by the S3 aggregate (`summary.json`).

use axum::{
    Json, Router,
    extract::{Query, State},
    routing::get,
};
use papermake_registry::{AnalyticsQuery, AnalyticsResult};
use serde::Deserialize;

use crate::{AppState, error::Result as ApiResult};

#[derive(Debug, Deserialize)]
pub struct DaysQuery {
    #[serde(default = "default_days")]
    pub days: u32,
}

fn default_days() -> u32 {
    30
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/volume", get(volume))
        .route("/templates", get(templates))
        .route("/performance", get(performance))
}

/// GET /api/analytics/volume?days=30
#[utoipa::path(
    get,
    path = "/api/analytics/volume",
    tag = "analytics",
    params(("days" = Option<u32>, Query, description = "Window in days (default 30)")),
    responses((status = 200, description = "Render volume over time", body = AnalyticsResult)),
)]
pub async fn volume(
    State(state): State<AppState>,
    Query(q): Query<DaysQuery>,
) -> ApiResult<Json<AnalyticsResult>> {
    let result = state
        .registry
        .get_render_analytics(AnalyticsQuery::VolumeOverTime { days: q.days })
        .await?;
    Ok(Json(result))
}

/// GET /api/analytics/templates
#[utoipa::path(
    get,
    path = "/api/analytics/templates",
    tag = "analytics",
    responses((status = 200, description = "Total renders per template", body = AnalyticsResult)),
)]
pub async fn templates(State(state): State<AppState>) -> ApiResult<Json<AnalyticsResult>> {
    let result = state
        .registry
        .get_render_analytics(AnalyticsQuery::TemplateStats)
        .await?;
    Ok(Json(result))
}

/// GET /api/analytics/performance?days=30
#[utoipa::path(
    get,
    path = "/api/analytics/performance",
    tag = "analytics",
    params(("days" = Option<u32>, Query, description = "Window in days (default 30)")),
    responses((status = 200, description = "Average render duration over time", body = AnalyticsResult)),
)]
pub async fn performance(
    State(state): State<AppState>,
    Query(q): Query<DaysQuery>,
) -> ApiResult<Json<AnalyticsResult>> {
    let result = state
        .registry
        .get_render_analytics(AnalyticsQuery::DurationOverTime { days: q.days })
        .await?;
    Ok(Json(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;
    use axum::{body::Body, http::Request};
    use papermake_registry::render_storage::{MemoryRenderStorage, RenderRecord, RenderStorage};
    use papermake_registry::storage::blob_storage::MemoryStorage;
    use tower::ServiceExt;

    async fn app_with_render_record() -> Router {
        let render_storage = MemoryRenderStorage::new();
        render_storage
            .store_render(RenderRecord::success(
                "invoice:latest".to_string(),
                "invoice".to_string(),
                "latest".to_string(),
                "sha256:m".to_string(),
                "sha256:d".to_string(),
                "sha256:p".to_string(),
                120,
                1024,
            ))
            .await
            .unwrap();
        let registry = papermake_registry::Registry::new(MemoryStorage::new(), render_storage);
        router().with_state(test_support::state(registry))
    }

    #[tokio::test]
    async fn analytics_routes_return_the_requested_rollups() {
        let app = app_with_render_record().await;

        let volume = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/volume")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let volume_body = test_support::response_json(volume).await;
        assert!(volume_body["Volume"][0]["renders"].as_u64().unwrap() >= 1);

        let templates = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/templates")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let templates_body = test_support::response_json(templates).await;
        assert_eq!(
            templates_body["Templates"][0]["template_name"].as_str(),
            Some("invoice")
        );

        let performance = app
            .oneshot(
                Request::builder()
                    .uri("/performance")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let performance_body = test_support::response_json(performance).await;
        assert!(
            performance_body["Duration"][0]["avg_duration_ms"]
                .as_f64()
                .unwrap()
                > 0.0
        );
    }
}
