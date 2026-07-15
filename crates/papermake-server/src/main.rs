//! Papermake HTTP API Server
//!
//! Provides REST API endpoints for template management, PDF rendering,
//! and analytics for the Papermake PDF generation system.

use async_trait::async_trait;
use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{Request, Response, header},
    response::Json,
    routing::get,
};
use papermake_registry::{
    AnalyticsQuery, AnalyticsResult, BlobStorage, Registry, RegistryError, RenderOptions,
    RenderRecord, RenderStorage, S3BufferedRenderStorage, S3Storage, TemplateInfo,
    batch::{BatchInput, BatchItem, BatchJob, JobView},
    bundle::TemplateBundle,
    registry::{DeleteSummary, RenderResult as RegistryRenderResult},
    render_storage::summary::Summary,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tower::ServiceBuilder;
use tower_http::{classify::ServerErrorsFailureClass, trace::TraceLayer};
use tracing::{Span, debug, debug_span, error, info, warn};

mod config;
mod error;
mod i18n;
mod models;
mod openapi;
mod routes;

use config::ServerConfig;
use error::{ApiError, Result};

use crate::models::RenderJob;

/// Render storage backing the server: the buffered-S3 store over S3 blobs.
pub type ServerRenderStorage = S3BufferedRenderStorage<S3Storage>;
type RegistryApiResult<T> = std::result::Result<T, RegistryError>;

#[async_trait]
pub trait ServerRegistry: Send + Sync {
    async fn list_templates(&self) -> RegistryApiResult<Vec<TemplateInfo>>;

    async fn publish(
        &self,
        bundle: TemplateBundle,
        namespace: &str,
        tag: &str,
    ) -> RegistryApiResult<String>;

    async fn resolve(&self, reference: &str) -> RegistryApiResult<String>;

    async fn get_template_source(&self, reference: &str) -> RegistryApiResult<String>;

    async fn render_summary(&self) -> RegistryApiResult<Summary>;

    async fn list_template_renders(
        &self,
        template_name: &str,
        limit: u32,
    ) -> RegistryApiResult<Vec<RenderRecord>>;

    async fn render_and_store(
        &self,
        reference: &str,
        data: &Value,
    ) -> RegistryApiResult<RegistryRenderResult>;

    async fn render_and_store_with(
        &self,
        reference: &str,
        data: &Value,
        retain_override: Option<u32>,
        options: &RenderOptions,
    ) -> RegistryApiResult<RegistryRenderResult>;

    async fn delete_version(&self, name: &str, tag: &str) -> RegistryApiResult<DeleteSummary>;

    async fn get_render_analytics(
        &self,
        query: AnalyticsQuery,
    ) -> RegistryApiResult<AnalyticsResult>;

    async fn list_recent_renders(&self, limit: u32) -> RegistryApiResult<Vec<RenderRecord>>;

    async fn get_render_pdf(&self, render_id: &str) -> RegistryApiResult<Vec<u8>>;

    async fn enqueue_batch_job(
        &self,
        reference: &str,
        inputs: &[BatchInput],
        retain_days: Option<u32>,
        options: &RenderOptions,
    ) -> RegistryApiResult<BatchJob>;

    async fn get_batch_job(&self, job_id: &str) -> RegistryApiResult<JobView>;

    async fn list_job_items(
        &self,
        job_id: &str,
        offset: usize,
        limit: usize,
    ) -> RegistryApiResult<Vec<BatchItem>>;
}

#[async_trait]
impl<S, R> ServerRegistry for Registry<S, R>
where
    S: BlobStorage + 'static,
    R: RenderStorage + 'static,
{
    async fn list_templates(&self) -> RegistryApiResult<Vec<TemplateInfo>> {
        Registry::list_templates(self).await
    }

    async fn publish(
        &self,
        bundle: TemplateBundle,
        namespace: &str,
        tag: &str,
    ) -> RegistryApiResult<String> {
        Registry::publish(self, bundle, namespace, tag).await
    }

    async fn resolve(&self, reference: &str) -> RegistryApiResult<String> {
        Registry::resolve(self, reference).await
    }

    async fn get_template_source(&self, reference: &str) -> RegistryApiResult<String> {
        Registry::get_template_source(self, reference).await
    }

    async fn render_summary(&self) -> RegistryApiResult<Summary> {
        Registry::render_summary(self).await
    }

    async fn list_template_renders(
        &self,
        template_name: &str,
        limit: u32,
    ) -> RegistryApiResult<Vec<RenderRecord>> {
        Registry::list_template_renders(self, template_name, limit).await
    }

    async fn render_and_store(
        &self,
        reference: &str,
        data: &Value,
    ) -> RegistryApiResult<RegistryRenderResult> {
        Registry::render_and_store(self, reference, data).await
    }

    async fn render_and_store_with(
        &self,
        reference: &str,
        data: &Value,
        retain_override: Option<u32>,
        options: &RenderOptions,
    ) -> RegistryApiResult<RegistryRenderResult> {
        Registry::render_and_store_with(self, reference, data, retain_override, options).await
    }

    async fn delete_version(&self, name: &str, tag: &str) -> RegistryApiResult<DeleteSummary> {
        Registry::delete_version(self, name, tag).await
    }

    async fn get_render_analytics(
        &self,
        query: AnalyticsQuery,
    ) -> RegistryApiResult<AnalyticsResult> {
        Registry::get_render_analytics(self, query).await
    }

    async fn list_recent_renders(&self, limit: u32) -> RegistryApiResult<Vec<RenderRecord>> {
        Registry::list_recent_renders(self, limit).await
    }

    async fn get_render_pdf(&self, render_id: &str) -> RegistryApiResult<Vec<u8>> {
        Registry::get_render_pdf(self, render_id).await
    }

    async fn enqueue_batch_job(
        &self,
        reference: &str,
        inputs: &[BatchInput],
        retain_days: Option<u32>,
        options: &RenderOptions,
    ) -> RegistryApiResult<BatchJob> {
        Registry::enqueue_batch_job(self, reference, inputs, retain_days, options).await
    }

    async fn get_batch_job(&self, job_id: &str) -> RegistryApiResult<JobView> {
        Registry::get_batch_job(self, job_id).await
    }

    async fn list_job_items(
        &self,
        job_id: &str,
        offset: usize,
        limit: usize,
    ) -> RegistryApiResult<Vec<BatchItem>> {
        Registry::list_job_items(self, job_id, offset, limit).await
    }
}

/// Main application state
#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<dyn ServerRegistry>,
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

    let s3_storage = S3Storage::from_env()
        .map_err(|e| ApiError::Config(format!("Invalid S3 configuration: {e}")))?;

    // Ensure the bucket exists before serving. Compose can only wait for the
    // object-store container to start, not for the S3 API to be ready, so the
    // server owns the bounded readiness/create-bucket wait.
    s3_storage
        .wait_for_bucket(30, Duration::from_secs(2))
        .await
        .map_err(|e| {
            error!("Giving up ensuring S3 bucket: {e}");
            ApiError::Config(format!("S3 bucket is unavailable: {e}"))
        })?;

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

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Background flush loop against the same buffer render_and_store stages into.
    if let Some(rs) = registry.render_storage() {
        let interval = config.flush_interval_seconds;
        let mut shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(interval)) => {
                        if let Err(e) = rs.flush().await {
                            error!("Analytics flush failed: {}", e);
                        }
                    }
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
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
    let listener = tokio::net::TcpListener::bind((config.host.as_str(), config.port))
        .await
        .map_err(|e| {
            ApiError::Config(format!(
                "Failed to bind {}:{}: {e}",
                config.host, config.port
            ))
        })?;
    let addr = listener.local_addr()?;

    info!("🚀 Server listening on http://{}", addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_tx))
        .await?;

    // Flush any staged records on shutdown so none are lost.
    if let Some(rs) = registry.render_storage() {
        info!("Flushing analytics buffer on shutdown");
        if let Err(e) = rs.flush().await {
            warn!("Final analytics flush failed during shutdown: {}", e);
        }
    }

    Ok(())
}

/// Resolve when the process receives Ctrl-C / SIGTERM.
async fn shutdown_signal(shutdown_tx: tokio::sync::watch::Sender<bool>) {
    wait_for_shutdown_signal().await;
    info!("Shutdown signal received");
    let _ = shutdown_tx.send(true);
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Create the main application router
fn create_router(state: AppState) -> Router {
    Router::new()
        // Health check
        .route("/health", get(health_check))
        // Machine-readable OpenAPI 3.1 document (generated from the code)
        .route("/api/openapi.json", get(openapi_json))
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
                .layer(DefaultBodyLimit::max(state.config.request_body_limit_bytes)),
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

/// Serve the generated OpenAPI 3.1 document. Point any OpenAPI client at it
/// (Scalar, Swagger UI, Redoc, code generators, …) — we don't bundle a UI.
async fn openapi_json() -> Json<utoipa::openapi::OpenApi> {
    use utoipa::OpenApi;
    Json(openapi::ApiDoc::openapi())
}

/// Health check endpoint
#[utoipa::path(
    get,
    path = "/health",
    tag = "health",
    responses((status = 200, description = "Service status, version and timestamp", body = Object)),
)]
async fn health_check() -> Result<Json<Value>> {
    Ok(Json(json!({
        "status": "healthy",
        "service": "papermake-server",
        "version": env!("CARGO_PKG_VERSION"),
        "timestamp": time::OffsetDateTime::now_utc()
    })))
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use axum::body::{Body, Bytes, to_bytes};
    use papermake_registry::{
        render_storage::MemoryRenderStorage, storage::blob_storage::MemoryStorage,
    };

    pub(crate) type TestRegistry = Registry<MemoryStorage, MemoryRenderStorage>;

    pub(crate) fn registry() -> TestRegistry {
        Registry::new(MemoryStorage::new(), MemoryRenderStorage::new())
    }

    pub(crate) fn state(registry: TestRegistry) -> AppState {
        let (job_sender, _job_receiver) = tokio::sync::mpsc::unbounded_channel();
        AppState {
            registry: Arc::new(registry),
            config: ServerConfig::default(),
            job_sender,
        }
    }

    pub(crate) fn bundle() -> TemplateBundle {
        TemplateBundle::new(
            br#"#let data = json(bytes(sys.inputs.data))
= Test
Hello #data.name"#
                .to_vec(),
            papermake_registry::bundle::TemplateMetadata::new("Test", "test@example.com"),
        )
    }

    pub(crate) async fn response_bytes(response: Response<Body>) -> Bytes {
        to_bytes(response.into_body(), usize::MAX).await.unwrap()
    }

    pub(crate) async fn response_json(response: Response<Body>) -> Value {
        serde_json::from_slice(&response_bytes(response).await).unwrap()
    }
}
