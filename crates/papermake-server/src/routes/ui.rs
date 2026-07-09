//! Server-side-rendered UI (maud + vendored KelpUI + a tiny htmx sprinkle).
//!
//! Rendering is split into pure `*_page`/`*_fragment` functions (unit-testable
//! without any infra) and thin handlers that fetch data then call them.
//!
//! Styling leans on KelpUI's semantic conventions: bare elements (`button`,
//! `table`, `input`, `textarea`, `nav`) are auto-styled; layout uses `.grid`
//! (+ `.grid-s|m|l`), `.cluster`, `.stack`, `.split`; panels use `.callout`;
//! status uses `.badge` combined with a state class (`.success`/`.warning`);
//! `.h1`–`.h6` are size utilities applied to any element.

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

/// Starter template pre-filled in the "New template" editor.
const STARTER_TYP: &str = "#let data = json(bytes(sys.inputs.data))\n\n\
= Hello #data.name\n\n\
Welcome to Papermake.\n";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(dashboard))
        .route("/templates/new", get(new_template))
        .route("/templates/{reference}", get(template_detail))
        .route("/ui/templates", post(ui_create))
        .route("/ui/templates/{name}/render", post(ui_render))
        .route("/ui/templates/{name}/publish", post(ui_publish))
        // Vendored assets embedded in the binary (no filesystem dependency —
        // works under distroless and regardless of the working directory).
        .route("/assets/kelp.css", get(kelp_css))
        .route("/assets/htmx.min.js", get(htmx_js))
}

// ---------------------------------------------------------------------------
// Layout + small helpers (pure)
// ---------------------------------------------------------------------------

/// Shared page shell: Kelp stylesheet, htmx, and a navbar.
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
                nav.navbar {
                    a.h4 href="/" style="text-decoration: none;" { "📄 Papermake" }
                    ul.menu.list-inline {
                        li { a href="/" { "Dashboard" } }
                        li { a.btn href="/templates/new" { "＋ New template" } }
                    }
                }
                main.stack { (body) }
            }
        }
    }
}

/// Eyebrow label + section wrapper for a dashboard block.
fn section(title: &str, inner: Markup) -> Markup {
    html! {
        section.stack {
            h2.h5.text-muted.text-uppercase { (title) }
            (inner)
        }
    }
}

/// A single KPI stat card.
fn stat_card(label: &str, value: Markup) -> Markup {
    html! {
        div.callout.stack style="--gap: var(--size-2xs);" {
            span.text-muted.text-uppercase style="font-size: var(--size-xs);" { (label) }
            span.h1 { (value) }
        }
    }
}

/// Status badge for a render record.
fn status_badge(success: bool) -> Markup {
    html! {
        @if success {
            span.badge.success { "✓ ok" }
        } @else {
            span.badge.warning { "✗ failed" }
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

/// Inline SVG area sparkline over a series of values.
fn sparkline(values: &[u64]) -> Markup {
    if values.is_empty() {
        return html! { p.text-muted { "No data yet." } };
    }
    let (w, h) = (600.0_f64, 80.0_f64);
    let max = (*values.iter().max().unwrap_or(&1)).max(1) as f64;
    let n = values.len();
    let step = if n > 1 { w / (n as f64 - 1.0) } else { 0.0 };
    let pt = |i: usize, v: u64| {
        let x = if n > 1 { i as f64 * step } else { w / 2.0 };
        let y = h - (v as f64 / max) * (h - 6.0) - 3.0;
        (x, y)
    };
    let line: String = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let (x, y) = pt(i, *v);
            format!("{:.1},{:.1}", x, y)
        })
        .collect::<Vec<_>>()
        .join(" ");
    let (x0, _) = pt(0, values[0]);
    let (xn, _) = pt(n - 1, values[n - 1]);
    let area = format!("{:.1},{:.1} {} {:.1},{:.1}", x0, h, line, xn, h);
    html! {
        svg viewBox=(format!("0 0 {} {}", w, h)) width="100%" height=(h)
            preserveAspectRatio="none" role="img" style="color: var(--color-primary-fill-vivid, currentColor);" {
            polygon points=(area) fill="currentColor" opacity="0.12";
            polyline points=(line) fill="none" stroke="currentColor" stroke-width="2";
        }
    }
}

/// Horizontal bar chart from labelled counts.
fn bars(items: &[(String, u64)]) -> Markup {
    if items.is_empty() {
        return html! { p.text-muted { "No data yet." } };
    }
    let max = items.iter().map(|(_, c)| *c).max().unwrap_or(1).max(1) as f64;
    html! {
        div.stack style="--gap: var(--size-xs);" {
            @for (label, count) in items {
                div.stack style="--gap: var(--size-4xs);" {
                    div.split {
                        span { (label) }
                        span.text-muted { (count) }
                    }
                    div style="height: 0.5rem; background: var(--color-fill-muted, #e5e7eb); border-radius: var(--border-radius-s);" {
                        div style=(format!(
                            "height: 100%; width: {:.1}%; background: var(--color-primary-fill-vivid, currentColor); border-radius: var(--border-radius-s);",
                            (*count as f64 / max) * 100.0
                        )) {}
                    }
                }
            }
        }
    }
}

/// A table of render records inside a callout.
fn renders_table(records: &[RenderRecord], now: OffsetDateTime) -> Markup {
    html! {
        @if records.is_empty() {
            p.text-muted { "No renders yet." }
        } @else {
            div.callout style="--padding: 0; overflow-x: auto;" {
                table.table-striped {
                    thead {
                        tr { th { "Render" } th { "Template" } th { "Status" } th { "Duration" } th { "When" } th { "PDF" } }
                    }
                    tbody {
                        @for r in records {
                            tr {
                                td { code { (short_id(&r.render_id)) } }
                                td { (r.template_ref) }
                                td { (status_badge(r.success)) }
                                td.no-wrap { (r.duration_ms) " ms" }
                                td.no-wrap { (relative_time(r.timestamp, now)) }
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
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

// ---------------------------------------------------------------------------
// Pages (pure)
// ---------------------------------------------------------------------------

/// Dashboard: KPI cards, volume sparkline, per-template bars, recent renders,
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
        div.split {
            h1 { "Dashboard" }
            a.btn href="/templates/new" { "＋ New template" }
        }

        // KPI cards.
        div.grid.grid-s {
            (stat_card("Renders · 24h", html! { (summary.totals.renders_24h) }))
            (stat_card("Success rate · 24h", html! {
                (format!("{:.0}%", summary.totals.success_rate_24h * 100.0))
            }))
            (stat_card("p90 latency · 24h", html! {
                (summary.totals.p90_latency_ms_24h) span.h4.text-muted { " ms" }
            }))
            (stat_card("Templates", html! { (templates.len()) }))
        }

        // Charts row.
        div.grid.grid-l {
            div.callout.stack {
                h2.h5.text-muted.text-uppercase { "Render volume" }
                (sparkline(&volume))
            }
            div.callout.stack {
                h2.h5.text-muted.text-uppercase { "Renders per template" }
                (bars(&tpl_bars))
            }
        }

        (section("Recent renders", renders_table(&summary.recent, now)))

        (section("Templates", html! {
            @if templates.is_empty() {
                div.callout.stack.text-center {
                    p { strong { "No templates yet." } }
                    p.text-muted { "Create one to publish, edit, and test-render it here." }
                    p { a.btn.vivid href="/templates/new" { "Create your first template" } }
                }
            } @else {
                div.grid.grid-m {
                    @for t in templates {
                        a.callout.stack href=(format!("/templates/{}", t.name))
                            style="--gap: var(--size-3xs); text-decoration: none;" {
                            strong.h5 { (t.full_name()) }
                            span.text-muted { (t.metadata.name) }
                            div.cluster {
                                @for tag in &t.tags { span.badge { (tag) } }
                            }
                        }
                    }
                }
            }
        }))
    };
    layout("Dashboard", body)
}

/// Template detail: metadata/tags, test-render, editor/publish, recent renders.
pub fn template_detail_page(
    name: &str,
    metadata: &TemplateMetadata,
    tags: &[String],
    source: &str,
    recent: &[RenderRecord],
    now: OffsetDateTime,
) -> Markup {
    let body = html! {
        div.split {
            div.stack style="--gap: var(--size-3xs);" {
                h1 { (name) }
                span.text-muted { "by " (metadata.author) }
            }
            div.cluster {
                @for tag in tags { span.badge { (tag) } }
            }
        }

        div.grid.grid-l {
            // Test render (htmx: swaps the PDF iframe in without a reload).
            div.callout.stack {
                h2.h5.text-muted.text-uppercase { "Test render" }
                form.stack hx-post=(format!("/ui/templates/{}/render", name))
                     hx-target="#render-result" hx-swap="innerHTML" {
                    label for="data" { "Input data (JSON)" }
                    textarea id="data" name="data" rows="6" { "{}" }
                    div { button.vivid type="submit" { "Test Render" } }
                }
                div id="render-result" {}
            }

            // Source + publish.
            div.callout.stack {
                h2.h5.text-muted.text-uppercase { "Source" }
                form.stack method="post" action=(format!("/ui/templates/{}/publish", name)) {
                    label for="main_typ" { "main.typ" }
                    textarea id="main_typ" name="main_typ" rows="14"
                        style="font-family: var(--font-mono, monospace);" { (source) }
                    div.cluster {
                        div.stack style="--gap: var(--size-4xs);" {
                            label for="author" { "Author" }
                            input id="author" name="author" value=(metadata.author);
                        }
                        div.stack style="--gap: var(--size-4xs);" {
                            label for="tag" { "Tag" }
                            input id="tag" name="tag" value="latest";
                        }
                    }
                    div { button type="submit" { "Publish" } }
                }
            }
        }

        (section("Recent renders", renders_table(recent, now)))
    };
    layout(name, body)
}

/// "New template" creation form.
pub fn new_template_page() -> Markup {
    let body = html! {
        h1 { "New template" }
        p.text-muted { "Publish a template, then edit and test-render it — all from here." }
        div.callout {
            form.stack method="post" action="/ui/templates" {
                div.cluster {
                    div.stack style="--gap: var(--size-4xs);" {
                        label for="name" { "Name" }
                        input id="name" name="name" placeholder="invoice" required;
                    }
                    div.stack style="--gap: var(--size-4xs);" {
                        label for="author" { "Author" }
                        input id="author" name="author" placeholder="you@example.com" required;
                    }
                    div.stack style="--gap: var(--size-4xs);" {
                        label for="tag" { "Tag" }
                        input id="tag" name="tag" value="latest";
                    }
                }
                label for="main_typ" { "main.typ" }
                textarea id="main_typ" name="main_typ" rows="14"
                    style="font-family: var(--font-mono, monospace);" { (STARTER_TYP) }
                div.cluster {
                    button.vivid type="submit" { "Create template" }
                    a.btn.btn-link href="/" { "Cancel" }
                }
            }
        }
    };
    layout("New template", body)
}

/// htmx fragment shown after a successful test render.
pub fn render_result_fragment(render_id: &str) -> Markup {
    html! {
        div.stack style="--gap: var(--size-2xs);" {
            p.text-muted { "Rendered " code { (short_id(render_id)) } }
            iframe src=(format!("/api/renders/{}/pdf", render_id))
                   title="Rendered PDF"
                   style="width: 100%; height: 600px; border: 1px solid var(--color-border, #ccc); border-radius: var(--border-radius-s);" {}
        }
    }
}

/// htmx fragment shown after a failed test render.
pub fn render_error_fragment(message: &str) -> Markup {
    html! {
        div.callout.warning role="alert" {
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

async fn new_template() -> Markup {
    new_template_page()
}

async fn template_detail(State(state): State<AppState>, Path(reference): Path<String>) -> Response {
    let now = OffsetDateTime::now_utc();

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
    publish(&state, &name, form.author, form.main_typ, form.tag).await
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

    fn sample_template() -> TemplateInfo {
        TemplateInfo::new(
            "invoice".to_string(),
            None,
            vec!["latest".to_string()],
            "sha256:m".to_string(),
            TemplateMetadata::new("Invoice Template", "a@b.com"),
        )
    }

    #[test]
    fn test_dashboard_page_contains_metrics_and_templates() {
        let now = datetime!(2026-07-09 12:00 UTC);
        let summary = sample_summary(now);
        let templates = vec![sample_template()];
        let html = dashboard_page(&summary, &templates, now).into_string();
        assert!(html.contains("Dashboard"));
        assert!(html.contains("Renders · 24h"));
        assert!(html.contains("Success rate · 24h"));
        assert!(html.contains("p90 latency · 24h"));
        assert!(html.contains("Invoice Template"));
        assert!(html.contains("/templates/invoice"));
        // Kelp classes + assets are wired.
        assert!(html.contains("class=\"callout"));
        assert!(html.contains("/assets/kelp.css"));
        assert!(html.contains("/assets/htmx.min.js"));
    }

    #[test]
    fn test_dashboard_empty_state_prompts_creation() {
        let now = datetime!(2026-07-09 12:00 UTC);
        let summary = Summary::empty(now);
        let html = dashboard_page(&summary, &[], now).into_string();
        assert!(html.contains("No templates yet."));
        assert!(html.contains("/templates/new"));
    }

    #[test]
    fn test_new_template_page_has_form() {
        let html = new_template_page().into_string();
        assert!(html.contains("New template"));
        assert!(html.contains("action=\"/ui/templates\""));
        assert!(html.contains("name=\"name\""));
        assert!(html.contains("name=\"main_typ\""));
        // Starter source is prefilled.
        assert!(html.contains("sys.inputs.data"));
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
    fn test_render_result_fragment_embeds_iframe() {
        let html = render_result_fragment("0192abcd-ef").into_string();
        assert!(html.contains("<iframe"));
        assert!(html.contains("/api/renders/0192abcd-ef/pdf"));
    }

    #[test]
    fn test_vendored_assets_are_embedded() {
        // Non-empty and recognizably the right files (compile-time embedded).
        assert!(KELP_CSS.starts_with(b"/*! kelpui"));
        assert!(HTMX_JS.starts_with(b"var htmx"));
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
