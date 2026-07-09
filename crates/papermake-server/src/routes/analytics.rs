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
async fn volume(
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
async fn templates(State(state): State<AppState>) -> ApiResult<Json<AnalyticsResult>> {
    let result = state
        .registry
        .get_render_analytics(AnalyticsQuery::TemplateStats)
        .await?;
    Ok(Json(result))
}

/// GET /api/analytics/performance?days=30
async fn performance(
    State(state): State<AppState>,
    Query(q): Query<DaysQuery>,
) -> ApiResult<Json<AnalyticsResult>> {
    let result = state
        .registry
        .get_render_analytics(AnalyticsQuery::DurationOverTime { days: q.days })
        .await?;
    Ok(Json(result))
}
