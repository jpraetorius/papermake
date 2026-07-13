//! Server-side-rendered UI (maud + a small hand-rolled stylesheet + a tiny
//! htmx sprinkle).
//!
//! Rendering is split into pure `*_page`/`*_fragment` functions (unit-testable
//! without any infra) and thin handlers that fetch data then call them.
//!
//! Styling (see `assets/app.css`) is semantic-first: bare elements are styled
//! by default; a few layout helpers (`.stack`/`.cluster`/`.grid`/`.split`),
//! components (`.card`/`.badge`/`.btn`) and modifiers (`.primary`/`.ok`/`.bad`)
//! do the rest.

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
        .route("/assets/logo.svg", get(logo_svg))
}

// ---------------------------------------------------------------------------
// Layout + small helpers (pure)
// ---------------------------------------------------------------------------

/// Which top-level nav item is the current location.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Nav {
    Dashboard,
    Templates,
}

/// Shared page shell: stylesheet, htmx, and a navbar with the current path
/// highlighted (bold, accent-colored, with a caret pointing at the content).
fn layout(title: &str, active: Nav, body: Markup) -> Markup {
    let current = |n: Nav| (active == n).then_some("page");
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · Papermake" }
                link rel="icon" type="image/svg+xml" href="/assets/logo.svg";
                link rel="stylesheet" href="/assets/app.css";
                script src="/assets/htmx.min.js" {}
            }
            body {
                header.navbar {
                    a.brand href="/" {
                        img src="/assets/logo.svg" alt="" width="64" height="64";
                        span { "Papermake" }
                    }
                    nav {
                        a href="/" aria-current=[current(Nav::Dashboard)] { "Dashboard" }
                        a href="/templates" aria-current=[current(Nav::Templates)] { "Templates" }
                    }
                }
                main.container.stack { (body) }
            }
        }
    }
}

/// Eyebrow-labelled section block.
fn section(title: &str, inner: Markup) -> Markup {
    html! {
        section.stack {
            h2.eyebrow { (title) }
            (inner)
        }
    }
}

/// A single KPI stat card.
fn stat_card(label: &str, value: Markup) -> Markup {
    html! {
        div.card.kpi.stack style="--gap: 0.35rem;" {
            span.eyebrow { (label) }
            span.num { (value) }
        }
    }
}

/// Status badge for a render record.
fn status_badge(success: bool) -> Markup {
    html! {
        @if success {
            span.badge.ok { "✓ ok" }
        } @else {
            span.badge.bad { "✗ failed" }
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
        return html! { p.muted { "No data yet." } };
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
        svg.spark viewBox=(format!("0 0 {} {}", w, h)) preserveAspectRatio="none" role="img" {
            polygon points=(area) fill="currentColor" opacity="0.12";
            polyline points=(line) fill="none" stroke="currentColor" stroke-width="2";
        }
    }
}

/// Horizontal bar chart from labelled counts.
fn bars(items: &[(String, u64)]) -> Markup {
    if items.is_empty() {
        return html! { p.muted { "No data yet." } };
    }
    let max = items.iter().map(|(_, c)| *c).max().unwrap_or(1).max(1) as f64;
    html! {
        div.stack style="--gap: 0.6rem;" {
            @for (label, count) in items {
                div.stack style="--gap: 0.25rem;" {
                    div.split {
                        span { (label) }
                        span.muted { (count) }
                    }
                    div.bar-track {
                        div.bar-fill style=(format!("width: {:.1}%;", (*count as f64 / max) * 100.0)) {}
                    }
                }
            }
        }
    }
}

/// A table of render records inside a scrollable card.
fn renders_table(records: &[RenderRecord], now: OffsetDateTime) -> Markup {
    html! {
        @if records.is_empty() {
            p.muted { "No renders yet." }
        } @else {
            div.card.flush.scroll-x {
                table {
                    thead {
                        tr { th { "Render" } th { "Template" } th { "Status" } th { "Duration" } th { "When" } th { "PDF" } }
                    }
                    tbody {
                        @for r in records {
                            tr {
                                td { code { (short_id(&r.render_id)) } }
                                td { (r.template_ref) }
                                td { (status_badge(r.success)) }
                                td.nowrap { (r.duration_ms) " ms" }
                                td.nowrap { (relative_time(r.timestamp, now)) }
                                td {
                                    @if r.success {
                                        a href=(format!("/api/renders/{}/pdf", r.render_id)) download { "download" }
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
            a.btn.primary href="/templates/new" { "＋ New template" }
        }

        // KPI cards.
        div.grid style="--min: 12rem;" {
            (stat_card("Renders · 24h", html! { (summary.totals.renders_24h) }))
            (stat_card("Success rate · 24h", html! {
                (format!("{:.0}%", summary.totals.success_rate_24h * 100.0))
            }))
            (stat_card("p90 latency · 24h", html! {
                (summary.totals.p90_latency_ms_24h) " " span.muted style="font-size: 1rem;" { "ms" }
            }))
            (stat_card("Templates", html! { (templates.len()) }))
        }

        // Charts row.
        div.grid style="--min: 22rem;" {
            div.card.stack {
                h2.eyebrow { "Render volume" }
                (sparkline(&volume))
            }
            div.card.stack {
                h2.eyebrow { "Renders per template" }
                (bars(&tpl_bars))
            }
        }

        (section("Recent renders", renders_table(&summary.recent, now)))
    };
    layout("Dashboard", Nav::Dashboard, body)
}

/// Templates index: all templates in an alphabetical table.
pub fn templates_page(templates: &[TemplateInfo]) -> Markup {
    let body = html! {
        div.split {
            h1 { "Templates" }
            a.btn.primary href="/templates/new" { "＋ New template" }
        }

        @if templates.is_empty() {
            div.card.stack.center {
                p { strong { "No templates yet." } }
                p.muted { "Create one to publish, edit, and test-render it here." }
                p { a.btn.primary href="/templates/new" { "Create your first template" } }
            }
        } @else {
            div.card.flush.scroll-x {
                table {
                    thead {
                        tr { th { "Template" } th { "Name" } th { "Author" } th { "Tags" } }
                    }
                    tbody {
                        @for t in templates {
                            tr {
                                td { a href=(format!("/templates/{}", t.name)) { (t.full_name()) } }
                                td { (t.metadata.name) }
                                td.muted { (t.metadata.author) }
                                td {
                                    div.cluster style="--gap: 0.3rem;" {
                                        @for tag in &t.tags {
                                            a.badge href=(format!("/templates/{}:{}", t.name, tag)) { (tag) }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    layout("Templates", Nav::Templates, body)
}

/// Template detail: metadata/tags, test-render, editor/publish, recent renders.
/// `tag` is the version currently being viewed (its source is shown / rendered).
pub fn template_detail_page(
    name: &str,
    tag: &str,
    metadata: &TemplateMetadata,
    tags: &[String],
    source: &str,
    recent: &[RenderRecord],
    now: OffsetDateTime,
) -> Markup {
    let body = html! {
        div.split {
            div.stack style="--gap: 0.25rem;" {
                div.cluster style="--gap: 0.5rem;" {
                    h1 { (name) }
                    span.badge { (tag) }
                }
                span.muted { "by " (metadata.author) }
            }
            // Every tagged version — click to view/edit that specific one.
            div.stack style="--gap: 0.3rem;" {
                span.eyebrow { "Versions" }
                div.cluster {
                    @for t in tags {
                        a.badge href=(format!("/templates/{}:{}", name, t))
                            aria-current=[(t == tag).then_some("true")] { (t) }
                    }
                }
            }
        }

        div.grid style="--min: 24rem;" {
            // Test render (htmx: swaps the PDF iframe in without a reload).
            div.card.stack {
                h2.eyebrow { "Test render · " (tag) }
                form.stack hx-post=(format!("/ui/templates/{}/render", name))
                     hx-target="#render-result" hx-swap="innerHTML" {
                    input type="hidden" name="tag" value=(tag);
                    label for="data" { "Input data (JSON)" }
                    textarea id="data" name="data" rows="6" { "{}" }
                    div { button.primary type="submit" { "Test Render" } }
                }
                div id="render-result" {}
            }

            // Source + publish.
            div.card.stack {
                h2.eyebrow { "Source · " (tag) }
                form.stack method="post" action=(format!("/ui/templates/{}/publish", name)) {
                    label for="main_typ" { "main.typ" }
                    textarea id="main_typ" name="main_typ" rows="14" class="mono" { (source) }
                    div.cluster style="--gap: 1rem;" {
                        div.stack style="--gap: 0.25rem;" {
                            label for="author" { "Author" }
                            input id="author" name="author" value=(metadata.author);
                        }
                        div.stack style="--gap: 0.25rem;" {
                            label for="tag" { "Tag" }
                            input id="tag" name="tag" value=(tag);
                        }
                    }
                    div { button type="submit" { "Publish" } }
                }
            }
        }

        (section("Recent renders", renders_table(recent, now)))

        (section("Danger zone", html! {
            div.card.stack {
                p.muted {
                    "Delete version " strong { (name) ":" (tag) } ". Assets not shared with "
                    "other versions are removed too; shared assets are kept."
                }
                form method="post" action=(format!("/ui/templates/{}/delete", name))
                     onsubmit=(format!("return confirm('Delete {}:{}? This cannot be undone.')", name, tag)) {
                    input type="hidden" name="tag" value=(tag);
                    button.danger type="submit" { "Delete this version" }
                }
            }
        }))
    };
    layout(name, Nav::Templates, body)
}

/// "New template" creation form.
pub fn new_template_page() -> Markup {
    let body = html! {
        h1 { "New template" }
        p.muted { "Publish a template, then edit and test-render it — all from here." }
        div.card {
            form.stack method="post" action="/ui/templates" {
                div.cluster style="--gap: 1rem;" {
                    div.stack style="--gap: 0.25rem;" {
                        label for="name" { "Name" }
                        input id="name" name="name" placeholder="invoice" required;
                    }
                    div.stack style="--gap: 0.25rem;" {
                        label for="author" { "Author" }
                        input id="author" name="author" placeholder="you@example.com" required;
                    }
                    div.stack style="--gap: 0.25rem;" {
                        label for="tag" { "Tag" }
                        input id="tag" name="tag" value="latest";
                    }
                }
                label for="main_typ" { "main.typ" }
                textarea id="main_typ" name="main_typ" rows="14" class="mono" { (STARTER_TYP) }
                div.cluster {
                    button.primary type="submit" { "Create template" }
                    a.btn.ghost href="/" { "Cancel" }
                }
            }
        }
    };
    layout("New template", Nav::Templates, body)
}

/// htmx fragment shown after a successful test render.
pub fn render_result_fragment(render_id: &str) -> Markup {
    html! {
        div.stack style="--gap: 0.5rem;" {
            p.muted { "Rendered " code { (short_id(render_id)) } }
            iframe src=(format!("/api/renders/{}/pdf", render_id))
                   title="Rendered PDF"
                   style="width: 100%; height: 600px; border: 1px solid var(--border); border-radius: var(--radius);" {}
        }
    }
}

/// htmx fragment shown after a failed test render.
pub fn render_error_fragment(message: &str) -> Markup {
    html! {
        div.callout.warn role="alert" {
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

async fn templates_list(State(state): State<AppState>) -> Markup {
    let mut templates = state.registry.list_templates().await.unwrap_or_default();
    templates.sort_by_key(|t| t.full_name());
    templates_page(&templates)
}

async fn template_detail(State(state): State<AppState>, Path(reference): Path<String>) -> Response {
    let now = OffsetDateTime::now_utc();

    // Reference is `name` or `name:tag`; default to the "latest" tag.
    let (name, tag) = match reference.split_once(':') {
        Some((n, t)) => (n.to_string(), t.to_string()),
        None => (reference.clone(), "latest".to_string()),
    };
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

    template_detail_page(&name, &tag, &metadata, &tags, &source, &recent, now).into_response()
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
    Form(form): Form<RenderForm>,
) -> Markup {
    let data: serde_json::Value = match serde_json::from_str(form.data.trim()) {
        Ok(v) => v,
        Err(e) => return render_error_fragment(&format!("Invalid JSON: {}", e)),
    };
    let reference = format!("{}:{}", name, form.tag);
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
const APP_CSS: &[u8] = include_bytes!("../../assets/app.css");
/// Vendored htmx (htmx@4.0.0-beta5), embedded at compile time.
const HTMX_JS: &[u8] = include_bytes!("../../assets/htmx.min.js");
/// Paper-crane logo / favicon (SVG), embedded at compile time.
const LOGO_SVG: &[u8] = include_bytes!("../../assets/logo.svg");

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

async fn logo_svg() -> impl IntoResponse {
    (
        [
            (CONTENT_TYPE, "image/svg+xml"),
            (CACHE_CONTROL, "public, max-age=3600"),
        ],
        LOGO_SVG,
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
    fn test_dashboard_page_shows_metrics_not_template_list() {
        let now = datetime!(2026-07-09 12:00 UTC);
        let summary = sample_summary(now);
        let templates = vec![sample_template()];
        let html = dashboard_page(&summary, &templates, now).into_string();
        assert!(html.contains("Dashboard"));
        assert!(html.contains("Renders · 24h"));
        assert!(html.contains("Success rate · 24h"));
        assert!(html.contains("p90 latency · 24h"));
        // The template list now lives on its own page, not the dashboard.
        assert!(!html.contains("Invoice Template"));
        // Navbar links to the dedicated templates page.
        assert!(html.contains("href=\"/templates\""));
        // Own classes + assets are wired.
        assert!(html.contains("class=\"card"));
        assert!(html.contains("/assets/app.css"));
        assert!(html.contains("/assets/htmx.min.js"));
        // Favicon + logo.
        assert!(html.contains("rel=\"icon\""));
        assert!(html.contains("/assets/logo.svg"));
        // No Kelp remnants.
        assert!(!html.contains("kelp"));
    }

    #[test]
    fn test_templates_page_lists_templates() {
        let templates = vec![sample_template()];
        let html = templates_page(&templates).into_string();
        assert!(html.contains("Templates"));
        assert!(html.contains("<table"));
        assert!(html.contains("Invoice Template"));
        assert!(html.contains("/templates/invoice"));
        assert!(html.contains("a@b.com"));
    }

    #[test]
    fn test_templates_page_empty_state_prompts_creation() {
        let html = templates_page(&[]).into_string();
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
            "v2",
            &meta,
            &["latest".to_string(), "v2".to_string()],
            "= Hello",
            &[],
            now,
        )
        .into_string();
        assert!(html.contains("Test Render"));
        assert!(html.contains("hx-post=\"/ui/templates/invoice/render\""));
        assert!(html.contains("= Hello")); // source prefilled
        assert!(html.contains("/ui/templates/invoice/publish"));
        // Each version links to its tag-specific detail; the current tag is marked.
        assert!(html.contains("href=\"/templates/invoice:v2\""));
        assert!(html.contains("href=\"/templates/invoice:latest\""));
        assert!(html.contains("aria-current=\"true\""));
        // Test render + publish target the viewed tag.
        assert!(html.contains("name=\"tag\" value=\"v2\""));
        // Delete this version.
        assert!(html.contains("/ui/templates/invoice/delete"));
        assert!(html.contains("Delete this version"));
    }

    #[test]
    fn test_active_nav_marks_current_and_templates_tags_link() {
        // Active nav highlight.
        let dash = dashboard_page(
            &sample_summary(datetime!(2026-07-09 12:00 UTC)),
            &[],
            datetime!(2026-07-09 12:00 UTC),
        )
        .into_string();
        assert!(dash.contains("href=\"/\" aria-current=\"page\""));
        // Templates table renders each tag as a link to its tagged detail.
        let tpls = templates_page(&[sample_template()]).into_string();
        assert!(tpls.contains("href=\"/templates/invoice:latest\""));
    }

    #[test]
    fn test_render_result_fragment_embeds_iframe() {
        let html = render_result_fragment("0192abcd-ef").into_string();
        assert!(html.contains("<iframe"));
        assert!(html.contains("/api/renders/0192abcd-ef/pdf"));
    }

    #[test]
    fn test_embedded_assets() {
        assert!(APP_CSS.starts_with(b"/* Papermake UI"));
        assert!(HTMX_JS.starts_with(b"var htmx"));
        assert!(LOGO_SVG.starts_with(b"<svg"));
        let logo = std::str::from_utf8(LOGO_SVG).unwrap();
        assert!(logo.contains("viewBox")); // scales in the favicon + navbar
        assert!(logo.contains("<path"));
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
