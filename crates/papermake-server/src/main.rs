//! Papermake HTTP API Server
//!
//! Provides REST API endpoints for template management, PDF rendering,
//! and analytics for the Papermake PDF generation system.

use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{Request, Response, header},
    response::Json,
    routing::get,
};
use papermake_registry::{Registry, S3BufferedRenderStorage, S3Storage};
use serde_json::{Value, json};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tower::ServiceBuilder;
use tower_http::{classify::ServerErrorsFailureClass, cors::CorsLayer, trace::TraceLayer};
use tracing::{Span, debug, debug_span, error, info};

mod config;
mod error;
mod i18n;
mod models;
mod routes;

use config::ServerConfig;
use error::Result;

use crate::models::RenderJob;

/// Render storage backing the server: the buffered-S3 store over S3 blobs.
pub type ServerRenderStorage = S3BufferedRenderStorage<S3Storage>;

/// Main application state
#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<Registry<S3Storage, ServerRenderStorage>>,
    pub config: ServerConfig,
    pub job_sender: tokio::sync::mpsc::UnboundedSender<RenderJob>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load environment variables
    dotenv::dotenv().ok();

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| {
            "papermake_server=info,papermake_registry=info,tower_http=info".to_string()
        }))
        .init();

    // Load configuration
    let config = ServerConfig::from_env()?;
    info!(
        "Starting Papermake Server on {}:{}",
        config.host, config.port
    );

    // Load fonts now (embedded + system + FONTS_DIR) so the cost is paid at
    // startup rather than on the first render.
    papermake::preload_fonts();
    info!("Fonts preloaded");

    let s3_storage = S3Storage::from_env().unwrap(); // TODO: improve error handling

    // Ensure S3 bucket exists
    if let Err(e) = s3_storage.ensure_bucket().await {
        error!("Failed to ensure S3 bucket exists: {}", e);
    }

    // Buffered-S3 render store: stages records in memory, flushes NDJSON to S3.
    // Shares the S3 backend with the registry (no separate DB).
    let render_storage = S3BufferedRenderStorage::new(
        Arc::new(s3_storage.clone()),
        config.instance_id.clone(),
        config.flush_max_records,
    );

    // Create registry
    let registry = Arc::new(
        Registry::new(s3_storage, render_storage)
            .with_retention_days(config.render_retention_days)
            .with_shard_size(config.shard_size)
            .with_render_limits(
                config.max_concurrent_renders,
                Duration::from_secs(config.render_timeout_seconds),
            ),
    );

    // Background flush loop against the same buffer render_and_store stages into.
    if let Some(rs) = registry.render_storage() {
        let interval = config.flush_interval_seconds;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(interval)).await;
                if let Err(e) = rs.flush().await {
                    error!("Analytics flush failed: {}", e);
                }
            }
        });
        info!("🔧 Analytics flush loop started (every {}s)", interval);
    }

    // Create job channel for event-driven processing
    let (job_sender, _job_receiver) = tokio::sync::mpsc::unbounded_channel();

    // Create application state
    let state = AppState {
        registry: registry.clone(),
        config: config.clone(),
        job_sender,
    };

    // Build router
    let app = create_router(state);

    // Start server
    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;

    info!("🚀 Server listening on http://{}", addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Flush any staged records on shutdown so none are lost.
    if let Some(rs) = registry.render_storage() {
        info!("Flushing analytics buffer on shutdown");
        if let Err(e) = rs.flush().await {
            error!("Final analytics flush failed: {}", e);
        }
    }

    Ok(())
}

/// Resolve when the process receives Ctrl-C / SIGTERM.
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// Create the main application router
fn create_router(state: AppState) -> Router {
    Router::new()
        // Health check
        .route("/health", get(health_check))
        // API routes
        .nest("/api", api_routes())
        // Server-side-rendered UI + embedded assets (dashboard, detail, htmx, /assets)
        .merge(routes::ui::router())
        // Middleware
        .layer(
            ServiceBuilder::new()
                .layer(
                    TraceLayer::new_for_http()
                        .make_span_with(|request: &Request<_>| {
                            let request_id = uuid::Uuid::new_v4();
                            let user_agent = request
                                .headers()
                                .get(header::USER_AGENT)
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or("-");
                            let content_length = request
                                .headers()
                                .get(header::CONTENT_LENGTH)
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or("-");

                            debug_span!(
                                "http_request",
                                request_id = %request_id,
                                method = %request.method(),
                                uri = %request.uri(),
                                version = ?request.version(),
                                user_agent = %user_agent,
                                content_length = %content_length,
                                status = tracing::field::Empty,
                                latency_ms = tracing::field::Empty,
                            )
                        })
                        .on_request(|request: &Request<_>, span: &Span| {
                            let _entered = span.enter();
                            debug!(
                                method = %request.method(),
                                uri = %request.uri(),
                                "request started",
                            );
                        })
                        .on_response(|response: &Response<_>, latency: Duration, span: &Span| {
                            let latency_ms = latency.as_millis() as u64;
                            span.record("status", response.status().as_u16());
                            span.record("latency_ms", latency_ms);
                            let _entered = span.enter();
                            debug!(
                                status = response.status().as_u16(),
                                latency_ms, "request completed",
                            );
                        })
                        .on_failure(
                            |failure: ServerErrorsFailureClass, latency: Duration, span: &Span| {
                                let latency_ms = latency.as_millis() as u64;
                                span.record("latency_ms", latency_ms);
                                let _entered = span.enter();
                                error!(
                                    failure = %failure,
                                    latency_ms,
                                    "request failed",
                                );
                            },
                        ),
                )
                .layer(CorsLayer::permissive())
                .layer(DefaultBodyLimit::max(50 * 1024 * 1024)), // 50MB for large PDFs
        )
        .with_state(state)
}

/// API routes
fn api_routes() -> Router<AppState> {
    Router::new()
        .nest("/templates", routes::templates::router())
        .nest("/render", routes::render::router())
        .nest("/renders", routes::renders::router())
        .nest("/jobs", routes::jobs::router())
        .nest("/analytics", routes::analytics::router())
}

/// Health check endpoint
async fn health_check() -> Result<Json<Value>> {
    Ok(Json(json!({
        "status": "healthy",
        "service": "papermake-server",
        "version": env!("CARGO_PKG_VERSION"),
        "timestamp": time::OffsetDateTime::now_utc()
    })))
}
