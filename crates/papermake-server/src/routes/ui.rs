//! Server-side-rendered UI (maud + vendored KelpUI + a tiny htmx sprinkle).
//!
//! Rendering is split into pure `*_page`/`*_fragment` functions (unit-testable
//! without any infra) and thin handlers that fetch data then call them.

use axum::{
    Router,
    extract::{Form, Path, State},
    http::header::{CACHE_CONTROL, CONTENT_TYPE},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use maud::{DOCTYPE, Markup, html};
use serde::Deserialize;
use time::OffsetDateTime;

use papermake_registry::TemplateInfo;
use papermake_registry::bundle::{TemplateBundle, TemplateMetadata};
use papermake_registry::render_storage::summary::Summary;
use papermake_registry::render_storage::types::RenderRecord;

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(dashboard))
        .route("/templates/{reference}", get(template_detail))
        .route("/ui/templates/{name}/render", post(ui_render))
        .route("/ui/templates/{name}/publish", post(ui_publish))
        // Vendored assets embedded in the binary (no filesystem dependency —
        // works under distroless and regardless of the working directory).
        .route("/assets/kelp.css", get(kelp_css))
        .route("/assets/htmx.min.js", get(htmx_js))
}

/// Vendored KelpUI stylesheet (kelpui@1.17.2), embedded at compile time.
const KELP_CSS: &[u8] = include_bytes!("../../assets/kelp.css");
/// Vendored htmx (htmx@4.0.0-beta5), embedded at compile time.
const HTMX_JS: &[u8] = include_bytes!("../../assets/htmx.min.js");

async fn kelp_css() -> impl IntoResponse {
    (
        [
            (CONTENT_TYPE, "text/css; charset=utf-8"),
            (CACHE_CONTROL, "public, max-age=3600"),
        ],
        KELP_CSS,
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

// ---------------------------------------------------------------------------
// Layout + small helpers (pure)
// ---------------------------------------------------------------------------

/// Shared page shell: Kelp stylesheet, htmx, and a nav header.
fn layout(title: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · Papermake" }
                link rel="stylesheet" href="/assets/kelp.css";
                script src="/assets/htmx.min.js" {}
            }
            body {
                header class="cluster" style="justify-content: space-between; padding: 1rem 0;" {
                    strong { a href="/" { "📄 Papermake" } }
                    nav { a href="/" { "Dashboard" } }
                }
                main { (body) }
            }
        }
    }
}

/// Human-friendly relative time (e.g. "3m ago").
fn relative_time(ts: OffsetDateTime, now: OffsetDateTime) -> String {
    let secs = (now - ts).whole_seconds();
    if secs < 0 {
        return "just now".to_string();
    }
    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// Inline SVG sparkline over a series of values.
fn sparkline(values: &[u64]) -> Markup {
    if values.is_empty() {
        return html! { span class="text-muted" { "no data" } };
    }
    let (w, h) = (240.0_f64, 40.0_f64);
    let max = *values.iter().max().unwrap_or(&1) as f64;
    let max = if max <= 0.0 { 1.0 } else { max };
    let n = values.len();
    let step = if n > 1 { w / (n as f64 - 1.0) } else { 0.0 };
    let points: String = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = i as f64 * step;
            let y = h - (*v as f64 / max) * h;
            format!("{:.1},{:.1}", x, y)
        })
        .collect::<Vec<_>>()
        .join(" ");
    html! {
        svg viewBox=(format!("0 0 {} {}", w, h)) width=(w) height=(h) role="img" {
            polyline points=(points) fill="none" stroke="currentColor" stroke-width="2";
        }
    }
}

/// Simple horizontal bar chart from labelled counts.
fn bars(items: &[(String, u64)]) -> Markup {
    let max = items.iter().map(|(_, c)| *c).max().unwrap_or(1).max(1) as f64;
    html! {
        div class="stack" style="gap: 0.35rem;" {
            @for (label, count) in items {
                div class="cluster" style="gap: 0.5rem; align-items: center;" {
                    span style="min-width: 10rem;" { (label) }
                    div style=(format!(
                        "height: 0.8rem; width: {:.1}%; background: currentColor; border-radius: 3px;",
                        (*count as f64 / max) * 100.0
                    )) {}
                    span class="text-muted" { (count) }
                }
            }
        }
    }
}

/// A table of render records.
fn renders_table(records: &[RenderRecord], now: OffsetDateTime) -> Markup {
    html! {
        @if records.is_empty() {
            p class="text-muted" { "No renders yet." }
        } @else {
            table {
                thead {
                    tr { th { "Render" } th { "Template" } th { "Status" } th { "Duration" } th { "When" } th { "PDF" } }
                }
                tbody {
                    @for r in records {
                        tr {
                            td { code { (short_id(&r.render_id)) } }
                            td { (r.template_ref) }
                            td {
                                @if r.success { span { "✓ ok" } }
                                @else { span { "✗ failed" } }
                            }
                            td { (r.duration_ms) "ms" }
                            td { (relative_time(r.timestamp, now)) }
                            td {
                                @if r.success {
                                    a href=(format!("/api/renders/{}/pdf", r.render_id)) { "download" }
                                } @else { "—" }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

// ---------------------------------------------------------------------------
// Pages (pure)
// ---------------------------------------------------------------------------

/// Dashboard: totals, volume sparkline, per-template bars, recent renders,
/// template list.
pub fn dashboard_page(
    summary: &Summary,
    templates: &[TemplateInfo],
    now: OffsetDateTime,
) -> Markup {
    let volume: Vec<u64> = summary.volume_by_day.iter().map(|v| v.renders).collect();
    let tpl_bars: Vec<(String, u64)> = summary
        .templates
        .iter()
        .map(|t| (t.template_name.clone(), t.total_renders))
        .collect();

    let body = html! {
        h1 { "Dashboard" }

        section class="grid" style="--kelp-grid-min: 12rem;" {
            div class="card" { h3 { "Renders (24h)" } p class="text-xl" { (summary.totals.renders_24h) } }
            div class="card" {
                h3 { "Success rate (24h)" }
                p class="text-xl" { (format!("{:.0}%", summary.totals.success_rate_24h * 100.0)) }
            }
            div class="card" {
                h3 { "p90 latency (24h)" }
                p class="text-xl" { (summary.totals.p90_latency_ms_24h) "ms" }
            }
        }

        section {
            h2 { "Render volume" }
            (sparkline(&volume))
        }

        section {
            h2 { "Renders per template" }
            @if tpl_bars.is_empty() { p class="text-muted" { "No data yet." } }
            @else { (bars(&tpl_bars)) }
        }

        section {
            h2 { "Recent renders" }
            (renders_table(&summary.recent, now))
        }

        section {
            h2 { "Templates" }
            @if templates.is_empty() { p class="text-muted" { "No templates published." } }
            @else {
                ul {
                    @for t in templates {
                        li {
                            a href=(format!("/templates/{}", t.name)) { (t.full_name()) }
                            " — " (t.metadata.name)
                            span class="text-muted" { " [" (t.tags.join(", ")) "]" }
                        }
                    }
                }
            }
        }
    };
    layout("Dashboard", body)
}

/// Template detail: metadata/tags, recent renders, editor + test-render, publish.
pub fn template_detail_page(
    name: &str,
    metadata: &TemplateMetadata,
    tags: &[String],
    source: &str,
    recent: &[RenderRecord],
    now: OffsetDateTime,
) -> Markup {
    let body = html! {
        h1 { (name) }
        p class="text-muted" { "by " (metadata.author) " · tags: " (tags.join(", ")) }

        section {
            h2 { "Test render" }
            form hx-post=(format!("/ui/templates/{}/render", name))
                 hx-target="#render-result" hx-swap="innerHTML" {
                label for="data" { "Input data (JSON)" }
                textarea id="data" name="data" rows="6" { "{}" }
                button type="submit" { "Test Render" }
            }
            div id="render-result" style="margin-top: 1rem;" {}
        }

        section {
            h2 { "Source" }
            form method="post" action=(format!("/ui/templates/{}/publish", name)) {
                label for="main_typ" { "main.typ" }
                textarea id="main_typ" name="main_typ" rows="16" { (source) }
                label for="author" { "Author" }
                input id="author" name="author" value=(metadata.author);
                label for="tag" { "Tag" }
                input id="tag" name="tag" value="latest";
                button type="submit" { "Publish" }
            }
        }

        section {
            h2 { "Recent renders" }
            (renders_table(recent, now))
        }
    };
    layout(name, body)
}

/// htmx fragment shown after a successful test render.
pub fn render_result_fragment(render_id: &str) -> Markup {
    html! {
        p { "Rendered " code { (short_id(render_id)) } }
        iframe src=(format!("/api/renders/{}/pdf", render_id))
               style="width: 100%; height: 600px; border: 1px solid #ccc;" {}
    }
}

/// htmx fragment shown after a failed test render.
pub fn render_error_fragment(message: &str) -> Markup {
    html! {
        div class="notice" role="alert" {
            strong { "Render failed" }
            p { (message) }
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers (thin)
// ---------------------------------------------------------------------------

async fn dashboard(State(state): State<AppState>) -> Markup {
    let now = OffsetDateTime::now_utc();
    let summary = state
        .registry
        .render_summary()
        .await
        .unwrap_or_else(|_| Summary::empty(now));
    let templates = state.registry.list_templates().await.unwrap_or_default();
    dashboard_page(&summary, &templates, now)
}

async fn template_detail(State(state): State<AppState>, Path(reference): Path<String>) -> Response {
    let now = OffsetDateTime::now_utc();

    // Resolve the display name/tag and look up metadata.
    let name = reference
        .split(':')
        .next()
        .unwrap_or(&reference)
        .to_string();
    let templates = state.registry.list_templates().await.unwrap_or_default();
    let info = templates
        .iter()
        .find(|t| t.name == name || t.full_name() == name);

    let (metadata, tags) = match info {
        Some(t) => (t.metadata.clone(), t.tags.clone()),
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

    template_detail_page(&name, &metadata, &tags, &source, &recent, now).into_response()
}

#[derive(Debug, Deserialize)]
struct RenderForm {
    data: String,
}

async fn ui_render(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<RenderForm>,
) -> Markup {
    let data: serde_json::Value = match serde_json::from_str(form.data.trim()) {
        Ok(v) => v,
        Err(e) => return render_error_fragment(&format!("Invalid JSON: {}", e)),
    };
    let reference = format!("{}:latest", name);
    match state.registry.render_and_store(&reference, &data).await {
        Ok(result) => render_result_fragment(&result.render_id),
        Err(e) => render_error_fragment(&e.to_string()),
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
    let metadata = TemplateMetadata::new(name.clone(), form.author);
    let bundle = TemplateBundle::new(form.main_typ.into_bytes(), metadata);
    match state.registry.publish(bundle, &name, &form.tag).await {
        Ok(_) => Redirect::to(&format!("/templates/{}", name)).into_response(),
        Err(e) => (
            axum::http::StatusCode::BAD_REQUEST,
            format!("Publish failed: {}", e),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use papermake_registry::render_storage::summary::{Summary, TemplateSummary, Totals};
    use time::macros::datetime;

    fn sample_summary(now: OffsetDateTime) -> Summary {
        let rec = RenderRecord::success(
            "invoice:latest".to_string(),
            "invoice".to_string(),
            "latest".to_string(),
            "sha256:m".to_string(),
            "sha256:d".to_string(),
            "sha256:p".to_string(),
            123,
            1024,
        );
        Summary {
            generated_at: now,
            volume_by_day: vec![],
            duration_by_day: vec![],
            templates: vec![TemplateSummary {
                template_name: "invoice".to_string(),
                total_renders: 5,
                recent: vec![rec.clone()],
            }],
            recent: vec![rec],
            totals: Totals {
                renders_24h: 7,
                success_rate_24h: 1.0,
                p90_latency_ms_24h: 200,
            },
        }
    }

    #[test]
    fn test_dashboard_page_contains_metrics_and_templates() {
        let now = datetime!(2026-07-09 12:00 UTC);
        let summary = sample_summary(now);
        let templates = vec![TemplateInfo::new(
            "invoice".to_string(),
            None,
            vec!["latest".to_string()],
            "sha256:m".to_string(),
            TemplateMetadata::new("Invoice Template", "a@b.com"),
        )];
        let html = dashboard_page(&summary, &templates, now).into_string();
        assert!(html.contains("Dashboard"));
        assert!(html.contains("Renders (24h)"));
        assert!(html.contains("Success rate (24h)"));
        assert!(html.contains("p90 latency (24h)"));
        assert!(html.contains("Invoice Template"));
        assert!(html.contains("/templates/invoice"));
        // Assets are wired.
        assert!(html.contains("/assets/kelp.css"));
        assert!(html.contains("/assets/htmx.min.js"));
    }

    #[test]
    fn test_template_detail_page_has_editor_and_htmx() {
        let now = datetime!(2026-07-09 12:00 UTC);
        let meta = TemplateMetadata::new("Invoice", "a@b.com");
        let html = template_detail_page(
            "invoice",
            &meta,
            &["latest".to_string()],
            "= Hello",
            &[],
            now,
        )
        .into_string();
        assert!(html.contains("Test Render"));
        assert!(html.contains("hx-post=\"/ui/templates/invoice/render\""));
        assert!(html.contains("= Hello")); // source prefilled
        assert!(html.contains("/ui/templates/invoice/publish"));
    }

    #[test]
    fn test_vendored_assets_are_embedded() {
        // Non-empty and recognizably the right files (compile-time embedded).
        assert!(KELP_CSS.starts_with(b"/*! kelpui"));
        assert!(HTMX_JS.starts_with(b"var htmx"));
    }

    #[test]
    fn test_render_result_fragment_embeds_iframe() {
        let html = render_result_fragment("0192abcd-ef").into_string();
        assert!(html.contains("<iframe"));
        assert!(html.contains("/api/renders/0192abcd-ef/pdf"));
    }

    #[test]
    fn test_relative_time() {
        let now = datetime!(2026-07-09 12:00 UTC);
        assert_eq!(
            relative_time(now - time::Duration::seconds(30), now),
            "30s ago"
        );
        assert_eq!(
            relative_time(now - time::Duration::minutes(5), now),
            "5m ago"
        );
        assert_eq!(relative_time(now - time::Duration::hours(2), now), "2h ago");
    }
}
