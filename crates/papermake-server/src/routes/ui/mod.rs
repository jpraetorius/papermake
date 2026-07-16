//! Server-side-rendered UI (maud + a small hand-rolled stylesheet + a tiny
//! htmx sprinkle).
//!
//! Rendering is split into pure `*_page`/`*_fragment` functions (unit-testable
//! without any infra) and thin handlers that fetch data then call them.
//!
//! All user-facing text comes from the [`crate::i18n`] catalogs; the request
//! language (`I18n`) is resolved from `Accept-Language` and threaded into the
//! pure functions.

use axum::{
    Router,
    extract::{Form, Path, Request, State},
    http::HeaderValue,
    http::header::{
        CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS,
        X_FRAME_OPTIONS,
    },
    middleware::{Next, from_fn},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use maud::{DOCTYPE, Markup, html};
use serde::Deserialize;
use time::OffsetDateTime;

use papermake_registry::TemplateInfo;
use papermake_registry::bundle::{TemplateBundle, TemplateMetadata};
use papermake_registry::render_storage::summary::{DurationBucket, Summary, TemplateSummary};
use papermake_registry::render_storage::types::{DurationPoint, RenderRecord, VolumePoint};

use crate::AppState;
use crate::i18n::I18n;

/// Starter template pre-filled in the "New template" editor.
pub(crate) const STARTER_TYP: &str = "= Hello #data.name\n\n\
Welcome to Papermake.\n";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(dashboard))
        .route("/templates", get(templates_list))
        .route("/templates/new", get(new_template))
        .route("/templates/{reference}", get(template_detail))
        .route("/ui/templates", post(ui_create))
        .route("/ui/templates/{name}/render", post(ui_render))
        .route("/ui/templates/{name}/publish", post(ui_publish))
        .route("/ui/templates/{name}/delete", post(ui_delete))
        // Vendored assets embedded in the binary (no filesystem dependency —
        // works under distroless and regardless of the working directory).
        .route("/assets/app.css", get(app_css))
        .route("/assets/htmx.min.js", get(htmx_js))
        .route("/assets/template-detail.js", get(template_detail_js))
        .route("/assets/logo.svg", get(logo_svg))
        .layer(from_fn(security_headers))
}

/// Add defensive security headers to every server-rendered UI response. The CSP
/// keeps resources same-origin (blocking exfiltration via injected `src`s) and
/// restricts framing to same-origin (clickjacking). Scripts are all served from
/// `/assets` so `script-src` stays `'self'` (no inline scripts); `style-src`
/// keeps `'unsafe-inline'` because the charts use inline `style` attributes.
/// `nosniff` stops content-type sniffing.
async fn security_headers(request: Request, next: Next) -> Response {
    const CSP: &str = "default-src 'self'; img-src 'self' data:; \
        style-src 'self' 'unsafe-inline'; script-src 'self'; \
        frame-ancestors 'self'; object-src 'none'; base-uri 'self'";

    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("SAMEORIGIN"));
    response
}

mod charts;
mod infer;
mod pages;
use charts::*;
use infer::*;
use pages::*;

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// Handlers (thin)
// ---------------------------------------------------------------------------

async fn dashboard(State(state): State<AppState>, t: I18n) -> Markup {
    let now = OffsetDateTime::now_utc();
    let summary = state
        .registry
        .render_summary()
        .await
        .unwrap_or_else(|_| Summary::empty(now));
    let templates = state.registry.list_templates().await.unwrap_or_default();
    dashboard_page(&summary, &templates, now, &t)
}

async fn new_template(t: I18n) -> Markup {
    new_template_page(&t)
}

async fn templates_list(State(state): State<AppState>, t: I18n) -> Markup {
    let mut templates = state.registry.list_templates().await.unwrap_or_default();
    templates.sort_by_key(|tpl| tpl.full_name());
    templates_page(&templates, &t)
}

async fn template_detail(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    t: I18n,
) -> Response {
    let now = OffsetDateTime::now_utc();

    // Reference is `name` or `name:tag`; default to the "latest" tag.
    let (name, tag) = match reference.split_once(':') {
        Some((n, tg)) => (n.to_string(), tg.to_string()),
        None => (reference.clone(), "latest".to_string()),
    };
    let templates = state.registry.list_templates().await.unwrap_or_default();
    let info = templates
        .iter()
        .find(|tpl| tpl.name == name || tpl.full_name() == name);

    let (metadata, tags) = match info {
        Some(tpl) => (tpl.metadata.clone(), tpl.tags.clone()),
        None => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                format!("Template not found: {}", name),
            )
                .into_response();
        }
    };

    let source = state
        .registry
        .get_template_source(&reference)
        .await
        .unwrap_or_default();
    let recent = state
        .registry
        .list_template_renders(&name, 20)
        .await
        .unwrap_or_default();

    template_detail_page(&name, &tag, &metadata, &tags, &source, &recent, now, &t).into_response()
}

#[derive(Debug, Deserialize)]
struct RenderForm {
    data: String,
    /// Tag being test-rendered (defaults to "latest").
    #[serde(default = "default_tag")]
    tag: String,
}

async fn ui_render(
    State(state): State<AppState>,
    Path(name): Path<String>,
    t: I18n,
    Form(form): Form<RenderForm>,
) -> Markup {
    let data: serde_json::Value = match serde_json::from_str(form.data.trim()) {
        Ok(v) => v,
        Err(e) => {
            return render_error_fragment(&t.ta("invalid-json", &[("error", e.to_string())]), &t);
        }
    };
    let reference = format!("{}:{}", name, form.tag);
    // Returns just the PDF-preview fragment (swapped into #render-result). The
    // recent-renders table is worker-aggregated and wouldn't include this render
    // yet, so there's nothing to OOB-update here.
    match state.registry.render_and_store(&reference, &data).await {
        Ok(result) => render_result_fragment(&result.render_id, &t),
        Err(e) => render_error_fragment(&e.to_string(), &t),
    }
}

#[derive(Debug, Deserialize)]
struct PublishForm {
    main_typ: String,
    author: String,
    #[serde(default = "default_tag")]
    tag: String,
}

fn default_tag() -> String {
    "latest".to_string()
}

async fn ui_publish(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<PublishForm>,
) -> Response {
    publish(&state, &name, form.author, form.main_typ, form.tag).await
}

#[derive(Debug, Deserialize)]
struct DeleteForm {
    #[serde(default = "default_tag")]
    tag: String,
}

async fn ui_delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<DeleteForm>,
) -> Response {
    match state.registry.delete_version(&name, &form.tag).await {
        Ok(_) => Redirect::to("/templates").into_response(),
        Err(e) => (
            axum::http::StatusCode::BAD_REQUEST,
            format!("Delete failed: {}", e),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct NewTemplateForm {
    name: String,
    main_typ: String,
    author: String,
    #[serde(default = "default_tag")]
    tag: String,
}

async fn ui_create(State(state): State<AppState>, Form(form): Form<NewTemplateForm>) -> Response {
    let name = form.name.trim();
    if name.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "Template name is required".to_string(),
        )
            .into_response();
    }
    publish(&state, name, form.author, form.main_typ, form.tag).await
}

/// Shared publish → redirect-to-detail path for the create/publish forms.
async fn publish(
    state: &AppState,
    name: &str,
    author: String,
    main_typ: String,
    tag: String,
) -> Response {
    let metadata = TemplateMetadata::new(name.to_string(), author);
    let bundle = TemplateBundle::new(main_typ.into_bytes(), metadata);
    match state.registry.publish(bundle, name, &tag).await {
        Ok(_) => Redirect::to(&format!("/templates/{}", name)).into_response(),
        Err(e) => (
            axum::http::StatusCode::BAD_REQUEST,
            format!("Publish failed: {}", e),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Embedded assets
// ---------------------------------------------------------------------------

/// Hand-rolled stylesheet, embedded at compile time.
const APP_CSS: &[u8] = include_bytes!("../../../assets/app.css");
/// Vendored htmx (htmx@4.0.0-beta5), embedded at compile time.
const HTMX_JS: &[u8] = include_bytes!("../../../assets/htmx.min.js");
/// Template-editor web component, embedded at compile time. Served here rather
/// than inlined so the page carries no inline script.
const TEMPLATE_DETAIL_JS: &[u8] = include_bytes!("../../../assets/template-detail.js");
/// Paper-crane logo / favicon (SVG), embedded at compile time.
const LOGO_SVG: &[u8] = include_bytes!("../../../assets/logo.svg");

async fn app_css() -> impl IntoResponse {
    (
        [
            (CONTENT_TYPE, "text/css; charset=utf-8"),
            (CACHE_CONTROL, "public, max-age=3600"),
        ],
        APP_CSS,
    )
}

async fn htmx_js() -> impl IntoResponse {
    (
        [
            (CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (CACHE_CONTROL, "public, max-age=3600"),
        ],
        HTMX_JS,
    )
}

async fn template_detail_js() -> impl IntoResponse {
    (
        [
            (CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (CACHE_CONTROL, "public, max-age=3600"),
        ],
        TEMPLATE_DETAIL_JS,
    )
}

async fn logo_svg() -> impl IntoResponse {
    (
        [
            (CONTENT_TYPE, "image/svg+xml"),
            (CACHE_CONTROL, "public, max-age=3600"),
        ],
        LOGO_SVG,
    )
}
