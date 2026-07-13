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
    extract::{Form, Path, State},
    http::header::{CACHE_CONTROL, CONTENT_TYPE},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use serde::Deserialize;
use time::OffsetDateTime;

use papermake_registry::TemplateInfo;
use papermake_registry::bundle::{TemplateBundle, TemplateMetadata};
use papermake_registry::render_storage::summary::{DurationBucket, Summary, TemplateSummary};
use papermake_registry::render_storage::types::{DurationPoint, RenderRecord, VolumePoint};

use crate::AppState;
use crate::i18n::I18n;

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
fn layout(title: &str, active: Nav, t: &I18n, body: Markup) -> Markup {
    let current = |n: Nav| (active == n).then_some("page");
    html! {
        (DOCTYPE)
        html lang=(t.code()) {
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
                        a href="/" aria-current=[current(Nav::Dashboard)] { (t.t("nav-dashboard")) }
                        a href="/templates" aria-current=[current(Nav::Templates)] { (t.t("nav-templates")) }
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

fn template_renders_section(records: &[RenderRecord], now: OffsetDateTime, t: &I18n) -> Markup {
    html! {
        section #template-renders .stack {
            h2.eyebrow { (t.t("section-recent-renders")) }
            (renders_table(records, now, t))
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
fn status_badge(success: bool, t: &I18n) -> Markup {
    html! {
        @if success {
            span.badge.ok { (t.t("status-ok")) }
        } @else {
            span.badge.bad { (t.t("status-failed")) }
        }
    }
}

/// Human-friendly relative time (e.g. "3m ago"), localized.
fn relative_time(ts: OffsetDateTime, now: OffsetDateTime, t: &I18n) -> String {
    let secs = (now - ts).whole_seconds();
    if secs < 0 {
        return t.t("time-just-now");
    }
    let (id, n) = if secs < 60 {
        ("time-seconds", secs)
    } else if secs < 3600 {
        ("time-minutes", secs / 60)
    } else if secs < 86_400 {
        ("time-hours", secs / 3600)
    } else {
        ("time-days", secs / 86_400)
    };
    t.ta(id, &[("n", n.to_string())])
}

/// Show a date label on roughly every ceil(n/10)-th column to avoid crowding.
fn label_every(n: usize) -> usize {
    ((n as f64) / 10.0).ceil().max(1.0) as usize
}

/// Compact day label, e.g. "07-09" (month-day).
fn fmt_day(date: time::Date) -> String {
    format!("{:02}-{:02}", u8::from(date.month()), date.day())
}

/// Renders-per-day: one primary bar per day, labelled with the count.
fn volume_bars(points: &[VolumePoint], t: &I18n) -> Markup {
    let max = points.iter().map(|p| p.renders).max().unwrap_or(0);
    if max == 0 {
        return html! { p.muted { (t.t("no-data")) } };
    }
    let every = label_every(points.len());
    html! {
        div.bar-chart {
            @for (i, p) in points.iter().enumerate() {
                div.bar-col {
                    span.bar-val { (p.renders) }
                    div.bar-area {
                        div.bar style=(format!("height: {:.1}%;", p.renders as f64 / max as f64 * 100.0)) {
                            div.seg style="height: 100%; background: var(--primary);" {}
                        }
                    }
                    span.bar-date { @if i % every == 0 { (fmt_day(p.date)) } }
                }
            }
        }
    }
}

/// Failures-in-context: stacked ok/failed bar per day, labelled with the total.
fn outcome_bars(points: &[VolumePoint], t: &I18n) -> Markup {
    let max = points.iter().map(|p| p.renders).max().unwrap_or(0);
    if max == 0 {
        return html! { p.muted { (t.t("no-data")) } };
    }
    let every = label_every(points.len());
    html! {
        div.stack style="--gap: 0.5rem;" {
            div.bar-chart {
                @for (i, p) in points.iter().enumerate() {
                    @let ok = p.renders - p.failures;
                    div.bar-col title=(format!("{}: {} · {}: {}", t.t("status-ok"), ok, t.t("status-failed"), p.failures)) {
                        span.bar-val { (p.renders) }
                        div.bar-area {
                            div.bar style=(format!("height: {:.1}%;", p.renders as f64 / max as f64 * 100.0)) {
                                @if p.failures > 0 {
                                    div.seg style=(format!("height: {:.1}%; background: var(--danger);", p.failures as f64 / p.renders as f64 * 100.0)) {}
                                }
                                @if ok > 0 {
                                    div.seg style=(format!("height: {:.1}%; background: var(--primary);", ok as f64 / p.renders as f64 * 100.0)) {}
                                }
                            }
                        }
                        span.bar-date { @if i % every == 0 { (fmt_day(p.date)) } }
                    }
                }
            }
            div.cluster style="--gap: 0.9rem; font-size: 0.8rem;" {
                span.cluster style="--gap: 0.35rem;" { span.dot style="background: var(--primary);" {} span.muted { (t.t("status-ok")) } }
                span.cluster style="--gap: 0.35rem;" { span.dot style="background: var(--danger);" {} span.muted { (t.t("status-failed")) } }
            }
        }
    }
}

/// One legend key for the latency chart: colored dot + label + latest value.
fn trend_key(color: &str, label: &str, value: u32, t: &I18n) -> Markup {
    html! {
        span.cluster style="--gap: 0.35rem;" {
            span.dot style=(format!("background: {color};")) {}
            span.muted { (label) " " (value) " " (t.t("unit-ms")) }
        }
    }
}

/// Latency over days as stacked bars: each day's bar rises to that day's p99,
/// split into percentile bands `0–p90` / `p90–p95` / `p95–p99` (cool→hot) so the
/// tail spread is visible. Legend shows each series' latest value.
fn latency_trend(points: &[DurationPoint], t: &I18n) -> Markup {
    if points.is_empty() {
        return html! { p.muted { (t.t("no-data")) } };
    }
    let max = points.iter().map(|p| p.p99_duration_ms).max().unwrap_or(0).max(1);
    let every = label_every(points.len());
    let last = points.last().unwrap();
    html! {
        div.stack style="--gap: 0.5rem;" {
            div.bar-chart {
                @for (i, p) in points.iter().enumerate() {
                    // Percentiles are monotonic by construction; clamp defensively.
                    @let p90 = p.p90_duration_ms;
                    @let p95 = p.p95_duration_ms.max(p90);
                    @let p99 = p.p99_duration_ms.max(p95);
                    div.bar-col title=(format!(
                        "p90 {p90} · p95 {p95} · p99 {p99} {}", t.t("unit-ms")
                    )) {
                        span.bar-val { (p99) }
                        div.bar-area {
                            div.bar style=(format!("height: {:.1}%;", p99 as f64 / max as f64 * 100.0)) {
                                // top → bottom: hottest band first.
                                @if p99 > p95 {
                                    div.seg style=(format!("height: {:.1}%; background: var(--danger);", (p99 - p95) as f64 / p99 as f64 * 100.0)) {}
                                }
                                @if p95 > p90 {
                                    div.seg style=(format!("height: {:.1}%; background: var(--warn);", (p95 - p90) as f64 / p99 as f64 * 100.0)) {}
                                }
                                @if p90 > 0 {
                                    div.seg style=(format!("height: {:.1}%; background: var(--primary);", p90 as f64 / p99 as f64 * 100.0)) {}
                                }
                            }
                        }
                        span.bar-date { @if i % every == 0 { (fmt_day(p.date)) } }
                    }
                }
            }
            div.cluster style="--gap: 0.9rem; font-size: 0.8rem;" {
                (trend_key("var(--primary)", &t.t("legend-p90"), last.p90_duration_ms, t))
                (trend_key("var(--warn)", &t.t("legend-p95"), last.p95_duration_ms, t))
                (trend_key("var(--danger)", &t.t("legend-p99"), last.p99_duration_ms, t))
                span.muted { (t.t("legend-avg")) " " (last.avg_duration_ms as u32) " " (t.t("unit-ms")) }
            }
        }
    }
}

/// Format a duration edge compactly: "120 ms" below 1 s, else "1.5 s".
fn fmt_ms(ms: u32, t: &I18n) -> String {
    if ms < 1000 {
        format!("{} {}", ms, t.t("unit-ms"))
    } else {
        format!("{} {}", (ms as f64 / 1000.0), t.t("unit-s"))
    }
}

/// Latency distribution as a single stacked bar: one segment per bucket, width
/// proportional to its share, colored cool→hot (fast→slow), with a legend.
fn latency_histogram(buckets: &[DurationBucket], t: &I18n) -> Markup {
    let total: u64 = buckets.iter().map(|b| b.count).sum();
    if total == 0 {
        return html! { p.muted { (t.t("no-data")) } };
    }
    // (label, count, color) per bucket, hue ramped green (fast) → red (slow).
    let m = buckets.len().max(2) as f64;
    let mut lower = 0u32;
    let segs: Vec<(String, u64, String)> = buckets
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let label = match b.upper_ms {
                Some(upper) => {
                    let l = format!("{}–{}", lower, fmt_ms(upper, t));
                    lower = upper;
                    l
                }
                None => format!("≥ {}", fmt_ms(lower, t)),
            };
            let hue = 150.0 - (i as f64) * (125.0 / (m - 1.0));
            (label, b.count, format!("oklch(66% 0.15 {hue:.0})"))
        })
        .collect();

    html! {
        div.stack style="--gap: 0.7rem;" {
            div.stacked-bar {
                @for (label, count, color) in &segs {
                    @if *count > 0 {
                        span style=(format!(
                            "width: {:.2}%; background: {};",
                            *count as f64 / total as f64 * 100.0, color
                        ))
                        title=(format!("{}: {} · {:.0}%", label, count, *count as f64 / total as f64 * 100.0)) {}
                    }
                }
            }
            div.donut-legend.stack style="--gap: 0.35rem;" {
                @for (label, count, color) in &segs {
                    @if *count > 0 {
                        div.legend-row {
                            span.swatch style=(format!("background: {color};")) {}
                            div.cluster style="--gap: 0.4rem;" {
                                span { (label) }
                                span.muted { (count) " · " (format!("{:.0}%", *count as f64 / total as f64 * 100.0)) }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One rendered donut slice plus its legend data.
struct DonutSlice {
    name: String,
    count: u64,
    pct: f64,
    dashoffset: f64,
    color: String,
    tags: Vec<(String, u64)>,
}

/// Donut of per-template render share. Templates below 5% fold into "Other";
/// each real slice lists its tag breakdown in the legend.
fn donut(templates: &[TemplateSummary], t: &I18n) -> Markup {
    let total: u64 = templates.iter().map(|s| s.total_renders).sum();
    if total == 0 {
        return html! { p.muted { (t.t("no-data")) } };
    }

    // Split into kept templates and an "Other" bucket (< 5% share).
    let kept: Vec<&TemplateSummary> = templates
        .iter()
        .filter(|s| s.total_renders as f64 / total as f64 >= 0.05)
        .collect();
    let other_count: u64 = total - kept.iter().map(|s| s.total_renders).sum::<u64>();
    let other_n = templates.len() - kept.len();

    // Palette: rotate hue around the wheel; "Other" is the neutral track color.
    let n = kept.len().max(1);
    let color = |i: usize| format!("oklch(64% 0.16 {:.0})", 20.0 + (i as f64) * (300.0 / n as f64));

    let mut slices: Vec<DonutSlice> = Vec::new();
    let mut offset = 25.0_f64; // start the first slice at 12 o'clock
    for (i, s) in kept.iter().enumerate() {
        let pct = s.total_renders as f64 / total as f64 * 100.0;
        slices.push(DonutSlice {
            name: s.template_name.clone(),
            count: s.total_renders,
            pct,
            dashoffset: offset,
            color: color(i),
            tags: s.by_tag.iter().map(|c| (c.tag.clone(), c.renders)).collect(),
        });
        offset -= pct;
    }
    if other_count > 0 {
        let pct = other_count as f64 / total as f64 * 100.0;
        slices.push(DonutSlice {
            name: t.ta("chart-other", &[("n", other_n.to_string())]),
            count: other_count,
            pct,
            dashoffset: offset,
            color: "var(--track)".to_string(),
            tags: Vec::new(),
        });
    }

    html! {
        div.donut-chart {
            svg.donut-svg viewBox="0 0 42 42" role="img" {
                circle cx="21" cy="21" r="15.9155" fill="none" stroke="var(--track)" stroke-width="4" {}
                @for s in &slices {
                    circle cx="21" cy="21" r="15.9155" fill="none"
                        stroke=(s.color) stroke-width="4"
                        stroke-dasharray=(format!("{:.3} {:.3}", s.pct, 100.0 - s.pct))
                        stroke-dashoffset=(format!("{:.3}", s.dashoffset)) {}
                }
                text x="21" y="20.5" class="donut-total" text-anchor="middle" { (total) }
                text x="21" y="25" class="donut-label" text-anchor="middle" { (t.t("unit-renders")) }
            }
            div.donut-legend.stack style="--gap: 0.5rem;" {
                @for s in &slices {
                    div.legend-row {
                        span.swatch style=(format!("background: {};", s.color)) {}
                        div.stack style="--gap: 0.15rem;" {
                            div.cluster style="--gap: 0.4rem;" {
                                span { (s.name) }
                                span.muted { (s.count) " · " (format!("{:.0}%", s.pct)) }
                            }
                            @if !s.tags.is_empty() {
                                div.cluster style="--gap: 0.3rem;" {
                                    @for (tag, cnt) in &s.tags {
                                        span.badge {
                                            (tag) " "
                                            span.muted { (format!("{:.0}%", *cnt as f64 / s.count as f64 * 100.0)) }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A table of render records inside a scrollable card.
fn renders_table(records: &[RenderRecord], now: OffsetDateTime, t: &I18n) -> Markup {
    html! {
        @if records.is_empty() {
            p.muted { (t.t("no-renders")) }
        } @else {
            div.card.flush.scroll-x {
                table {
                    thead {
                        tr {
                            th { (t.t("th-render")) }
                            th { (t.t("th-template")) }
                            th { (t.t("th-status")) }
                            th { (t.t("th-duration")) }
                            th { (t.t("th-when")) }
                            th { (t.t("th-pdf")) }
                        }
                    }
                    tbody {
                        @for r in records {
                            tr {
                                td { code { (short_id(&r.render_id)) } }
                                td { (r.template_ref) }
                                td { (status_badge(r.success, t)) }
                                td.nowrap { (t.ta("duration-ms", &[("n", r.duration_ms.to_string())])) }
                                td.nowrap { (relative_time(r.timestamp, now, t)) }
                                td {
                                    @if r.success {
                                        a href=(format!("/api/renders/{}/pdf", r.render_id)) download { (t.t("link-download")) }
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

fn infer_data_fields(source: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut i = 0;

    while i < source.len() {
        let remaining = &source[i..];
        if remaining.starts_with("data.at(") {
            if let Some((field, consumed)) = parse_first_string_arg(remaining, "data.at(") {
                push_unique(&mut fields, field);
                i += consumed;
                continue;
            }
        } else if remaining.starts_with("data.") {
            if let Some((field, consumed)) = parse_data_path(remaining) {
                push_unique(&mut fields, field);
                i += consumed;
                continue;
            }
        } else if remaining.starts_with("field(")
            && let Some((field, consumed)) = parse_first_string_arg(remaining, "field(")
        {
            push_unique(&mut fields, field);
            i += consumed;
            continue;
        }

        i += remaining.chars().next().map(char::len_utf8).unwrap_or(1);
    }

    fields
}

fn parse_data_path(source: &str) -> Option<(String, usize)> {
    let mut offset = "data.".len();
    let (first, consumed) = parse_identifier(&source[offset..])?;
    if first == "at" {
        return None;
    }
    offset += consumed;

    let mut parts = vec![first];
    while source[offset..].starts_with('.') {
        let next_offset = offset + 1;
        let Some((part, consumed)) = parse_identifier(&source[next_offset..]) else {
            break;
        };
        parts.push(part);
        offset = next_offset + consumed;
    }

    Some((parts.join("."), offset))
}

fn parse_identifier(source: &str) -> Option<(String, usize)> {
    let mut chars = source.char_indices();
    let (_, first) = chars.next()?;
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return None;
    }

    let mut end = first.len_utf8();
    for (idx, ch) in chars {
        if ch == '_' || ch.is_ascii_alphanumeric() {
            end = idx + ch.len_utf8();
        } else {
            break;
        }
    }

    Some((source[..end].to_string(), end))
}

fn parse_first_string_arg(source: &str, prefix: &str) -> Option<(String, usize)> {
    let mut offset = prefix.len();
    while let Some(ch) = source[offset..].chars().next()
        && ch.is_whitespace()
    {
        offset += ch.len_utf8();
    }

    let quote = source[offset..].chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    offset += quote.len_utf8();

    let mut value = String::new();
    let mut escaped = false;
    for (idx, ch) in source[offset..].char_indices() {
        if escaped {
            value.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == quote {
            return Some((value, offset + idx + ch.len_utf8()));
        } else {
            value.push(ch);
        }
    }

    None
}

fn push_unique(fields: &mut Vec<String>, field: String) {
    if field.trim().is_empty() || fields.iter().any(|existing| existing == &field) {
        return;
    }
    fields.push(field);
}

fn sample_data_json(fields: &[String]) -> String {
    if fields.is_empty() {
        return "{}".to_string();
    }

    let mut root = serde_json::Map::new();
    for field in fields {
        let parts: Vec<&str> = field.split('.').filter(|part| !part.is_empty()).collect();
        insert_json_path(&mut root, &parts);
    }

    serde_json::to_string_pretty(&serde_json::Value::Object(root))
        .unwrap_or_else(|_| "{}".to_string())
}

fn insert_json_path(map: &mut serde_json::Map<String, serde_json::Value>, parts: &[&str]) {
    let Some((head, tail)) = parts.split_first() else {
        return;
    };

    if tail.is_empty() {
        map.entry((*head).to_string())
            .or_insert_with(|| serde_json::Value::String(String::new()));
        return;
    }

    let entry = map
        .entry((*head).to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !entry.is_object() {
        *entry = serde_json::Value::Object(serde_json::Map::new());
    }
    if let Some(child) = entry.as_object_mut() {
        insert_json_path(child, tail);
    }
}

fn template_detail_component_script() -> Markup {
    html! {
        script type="module" {
            (PreEscaped(r#"
if (!customElements.get('template-detail-page')) {
  customElements.define('template-detail-page', class extends HTMLElement {
    connectedCallback() {
      this.input = this.querySelector('[data-json-input]');
      this.fields = Array.from(this.querySelectorAll('[data-data-field]'));
      this.onInput = () => this.updateFields();
      this.input?.addEventListener('input', this.onInput);
      this.updateFields();
    }

    disconnectedCallback() {
      this.input?.removeEventListener('input', this.onInput);
    }

    updateFields() {
      let data = null;
      try {
        data = JSON.parse(this.input?.value || '{}');
      } catch (_) {
        data = null;
      }

      for (const field of this.fields) {
        const path = field.dataset.dataField || '';
        const used = data !== null && this.hasValueAtPath(data, path);
        field.toggleAttribute('data-used', used);
      }
    }

    hasValueAtPath(data, path) {
      const value = path.split('.').filter(Boolean).reduce((cursor, part) => {
        if (cursor && typeof cursor === 'object' && Object.hasOwn(cursor, part)) {
          return cursor[part];
        }
        return undefined;
      }, data);
      return this.hasMeaningfulValue(value);
    }

    hasMeaningfulValue(value) {
      if (value === undefined || value === null) return false;
      if (typeof value === 'string') return value.trim().length > 0;
      if (Array.isArray(value)) return value.some((item) => this.hasMeaningfulValue(item));
      if (typeof value === 'object') {
        return Object.values(value).some((item) => this.hasMeaningfulValue(item));
      }
      return true;
    }
  });
}
"#))
        }
    }
}

// ---------------------------------------------------------------------------
// Pages (pure)
// ---------------------------------------------------------------------------

/// Dashboard: KPI cards, volume sparkline, per-template bars, recent renders.
pub fn dashboard_page(
    summary: &Summary,
    templates: &[TemplateInfo],
    now: OffsetDateTime,
    t: &I18n,
) -> Markup {

    let body = html! {
        div.split {
            h1 { (t.t("dashboard-title")) }
            a.btn.primary href="/templates/new" { (t.t("btn-new-template")) }
        }

        // KPI cards.
        div.grid style="--min: 12rem;" {
            (stat_card(&t.t("kpi-renders-24h"), html! { (summary.totals.renders_24h) }))
            (stat_card(&t.t("kpi-success-24h"), html! {
                (format!("{:.0}%", summary.totals.success_rate_24h * 100.0))
            }))
            (stat_card(&t.t("kpi-p90-24h"), html! {
                (summary.totals.p90_latency_ms_24h) " " span.muted style="font-size: 1rem;" { (t.t("unit-ms")) }
            }))
            (stat_card(&t.t("kpi-templates"), html! { (templates.len()) }))
        }

        // Charts.
        div.grid style="--min: 22rem;" {
            div.card.stack {
                h2.eyebrow { (t.t("chart-volume")) }
                (volume_bars(&summary.volume_by_day, t))
            }
            div.card.stack {
                h2.eyebrow { (t.t("chart-errors")) }
                (outcome_bars(&summary.volume_by_day, t))
            }
            div.card.stack {
                h2.eyebrow { (t.t("chart-latency")) }
                (latency_trend(&summary.duration_by_day, t))
            }
            div.card.stack {
                h2.eyebrow { (t.t("chart-latency-dist")) }
                (latency_histogram(&summary.duration_histogram, t))
            }
            div.card.stack {
                h2.eyebrow { (t.t("chart-per-template")) }
                (donut(&summary.templates, t))
            }
        }

        (section(&t.t("section-recent-renders"), renders_table(&summary.recent, now, t)))
    };
    layout(&t.t("dashboard-title"), Nav::Dashboard, t, body)
}

/// Templates index: all templates in an alphabetical table.
pub fn templates_page(templates: &[TemplateInfo], t: &I18n) -> Markup {
    let body = html! {
        div.split {
            h1 { (t.t("templates-title")) }
            a.btn.primary href="/templates/new" { (t.t("btn-new-template")) }
        }

        @if templates.is_empty() {
            div.card.stack.center {
                p { strong { (t.t("no-templates")) } }
                p.muted { (t.t("templates-empty-hint")) }
                p { a.btn.primary href="/templates/new" { (t.t("btn-create-first")) } }
            }
        } @else {
            div.card.flush.scroll-x {
                table {
                    thead {
                        tr {
                            th { (t.t("th-template")) }
                            th { (t.t("th-name")) }
                            th { (t.t("th-author")) }
                            th { (t.t("th-tags")) }
                        }
                    }
                    tbody {
                        @for tpl in templates {
                            tr {
                                td { a href=(format!("/templates/{}", tpl.name)) { (tpl.full_name()) } }
                                td { (tpl.metadata.name) }
                                td.muted { (tpl.metadata.author) }
                                td {
                                    div.cluster style="--gap: 0.3rem;" {
                                        @for tag in &tpl.tags {
                                            a.badge href=(format!("/templates/{}:{}", tpl.name, tag)) { (tag) }
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
    layout(&t.t("templates-title"), Nav::Templates, t, body)
}

/// Template detail: metadata/tags, test-render, editor/publish, recent renders.
/// `tag` is the version currently being viewed (its source is shown / rendered).
#[allow(clippy::too_many_arguments)]
pub fn template_detail_page(
    name: &str,
    tag: &str,
    metadata: &TemplateMetadata,
    tags: &[String],
    source: &str,
    recent: &[RenderRecord],
    now: OffsetDateTime,
    t: &I18n,
) -> Markup {
    let data_fields = infer_data_fields(source);
    let sample_data = sample_data_json(&data_fields);
    let data_rows = if data_fields.len() > 4 { "10" } else { "6" };

    let detail = html! {
        div.split {
            div.stack style="--gap: 0.25rem;" {
                div.cluster style="--gap: 0.5rem;" {
                    h1 { (name) }
                    span.badge { (tag) }
                }
                span.muted { (t.ta("by-author", &[("author", metadata.author.clone())])) }
                // Confirmation via the native Invoker Commands API — no JS.
                button.danger.self-start type="button" command="show-modal" commandfor="confirm-delete" { (t.t("btn-delete-version")) }
                dialog #confirm-delete {
                    form.stack method="post" action=(format!("/ui/templates/{}/delete", name)) {
                        h3 { (t.ta("delete-confirm-title", &[("name", name.to_string()), ("tag", tag.to_string())])) }
                        p.muted { (t.t("danger-explain")) }
                        p.muted { (t.t("delete-confirm-body")) }
                        input type="hidden" name="tag" value=(tag);
                        div.cluster {
                            button type="button" command="close" commandfor="confirm-delete" { (t.t("btn-cancel")) }
                            button.danger type="submit" { (t.t("btn-delete")) }
                        }
                    }
                }
            }
            // Every tagged version — click to view/edit that specific one. The
            // delete action for the current version sits to the left of the list.
            div.stack style="--gap: 0.3rem;" {
                span.eyebrow { (t.t("section-versions")) }
                div.cluster {
                    @for v in tags {
                        a.badge href=(format!("/templates/{}:{}", name, v))
                            aria-current=[(v == tag).then_some("true")] { (v) }
                    }
                }
            }
        }

        div.grid style="--min: 24rem;" {
            // Test render (htmx: swaps the PDF iframe in without a reload).
            div.card.stack {
                h2.eyebrow { (t.ta("test-render-for", &[("tag", tag.to_string())])) }
                form.stack hx-post=(format!("/ui/templates/{}/render", name))
                     hx-target="#render-result" hx-swap="innerHTML" {
                    input type="hidden" name="tag" value=(tag);
                    label for="data" { (t.t("label-input-data")) }
                    textarea id="data" name="data" rows=(data_rows) data-json-input { (sample_data) }
                    @if !data_fields.is_empty() {
                        div.data-fields.stack style="--gap: 0.45rem;" {
                            span.eyebrow { (t.t("available-data-fields")) }
                            div.cluster style="--gap: 0.35rem;" {
                                @for field in &data_fields {
                                    span.data-field data-data-field=(field) {
                                        code { (field) }
                                        span.field-check aria-hidden="true" { "✓" }
                                    }
                                }
                            }
                        }
                    }
                    div { button.primary type="submit" { (t.t("btn-test-render")) } }
                }
            }

            // Source + publish.
            div.card.stack {
                h2.eyebrow { (t.ta("source-for", &[("tag", tag.to_string())])) }
                form.stack method="post" action=(format!("/ui/templates/{}/publish", name)) {
                    label for="main_typ" { "main.typ" }
                    textarea id="main_typ" name="main_typ" rows="14" class="mono" { (source) }
                    div.cluster style="--gap: 1rem;" {
                        div.stack style="--gap: 0.25rem;" {
                            label for="author" { (t.t("label-author")) }
                            input id="author" name="author" value=(metadata.author);
                        }
                        div.stack style="--gap: 0.25rem;" {
                            label for="tag" { (t.t("label-tag")) }
                            input id="tag" name="tag" value=(tag);
                        }
                    }
                    div { button type="submit" { (t.t("btn-publish")) } }
                }
            }
        }

        div id="render-result" {}

        (template_renders_section(recent, now, t))
    };
    let body = html! {
        template-detail-page {
            (detail)
        }
        (template_detail_component_script())
    };
    layout(name, Nav::Templates, t, body)
}

/// "New template" creation form.
pub fn new_template_page(t: &I18n) -> Markup {
    let body = html! {
        h1 { (t.t("new-template-title")) }
        p.muted { (t.t("new-template-intro")) }
        div.card {
            form.stack method="post" action="/ui/templates" {
                div.cluster style="--gap: 1rem;" {
                    div.stack style="--gap: 0.25rem;" {
                        label for="name" { (t.t("label-name")) }
                        input id="name" name="name" placeholder="invoice" required;
                    }
                    div.stack style="--gap: 0.25rem;" {
                        label for="author" { (t.t("label-author")) }
                        input id="author" name="author" placeholder="you@example.com" required;
                    }
                    div.stack style="--gap: 0.25rem;" {
                        label for="tag" { (t.t("label-tag")) }
                        input id="tag" name="tag" value="latest";
                    }
                }
                label for="main_typ" { "main.typ" }
                textarea id="main_typ" name="main_typ" rows="14" class="mono" { (STARTER_TYP) }
                div.cluster {
                    button.primary type="submit" { (t.t("btn-create")) }
                    a.btn.ghost href="/" { (t.t("btn-cancel")) }
                }
            }
        }
    };
    layout(&t.t("new-template-title"), Nav::Templates, t, body)
}

/// htmx fragment shown after a successful test render.
pub fn render_result_fragment(render_id: &str, t: &I18n) -> Markup {
    let pdf_url = format!("/api/renders/{}/pdf", render_id);
    html! {
        div.card.stack.render-preview style="--gap: 0.5rem;" {
            div.split {
                p.muted { (t.t("rendered")) " " code { (short_id(render_id)) } }
                a.btn.ghost href=(pdf_url) target="_blank" rel="noopener" {
                    (t.t("link-open-pdf"))
                }
            }
            div.pdf-preview {
                iframe src=(format!("/api/renders/{}/pdf#view=FitH", render_id))
                       title="Rendered PDF" {}
            }
        }
    }
}

/// htmx fragment shown after a failed test render.
pub fn render_error_fragment(message: &str, t: &I18n) -> Markup {
    html! {
        div.callout.warn role="alert" {
            strong { (t.t("render-failed")) }
            p { (message) }
        }
    }
}

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

    fn en() -> I18n {
        I18n::from_accept_language(None)
    }

    fn de() -> I18n {
        I18n::from_accept_language(Some("de"))
    }

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
            duration_histogram: vec![],
            templates: vec![TemplateSummary {
                template_name: "invoice".to_string(),
                total_renders: 5,
                by_tag: vec![papermake_registry::render_storage::summary::TagCount {
                    tag: "latest".to_string(),
                    renders: 5,
                }],
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
        let html = dashboard_page(&summary, &templates, now, &en()).into_string();
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
    fn test_dashboard_charts_render_donut_and_series() {
        use papermake_registry::render_storage::summary::{DurationBucket, TagCount};
        use papermake_registry::render_storage::types::{DurationPoint, VolumePoint};

        let now = datetime!(2026-07-09 12:00 UTC);
        let d = now.date();
        let summary = Summary {
            generated_at: now,
            volume_by_day: vec![
                VolumePoint { date: d, renders: 10, failures: 1 },
                VolumePoint { date: d, renders: 20, failures: 3 },
            ],
            duration_by_day: vec![
                DurationPoint { date: d, avg_duration_ms: 120.0, p90_duration_ms: 240, p95_duration_ms: 260, p99_duration_ms: 280 },
                DurationPoint { date: d, avg_duration_ms: 150.0, p90_duration_ms: 310, p95_duration_ms: 340, p99_duration_ms: 380 },
            ],
            duration_histogram: vec![
                DurationBucket { upper_ms: Some(100), count: 12 },
                DurationBucket { upper_ms: Some(250), count: 6 },
                DurationBucket { upper_ms: None, count: 2 },
            ],
            // 70 / 27 / 3 out of 100 → the 3% template folds into "Other".
            templates: vec![
                TemplateSummary {
                    template_name: "invoice".to_string(),
                    total_renders: 70,
                    by_tag: vec![
                        TagCount { tag: "latest".to_string(), renders: 56 },
                        TagCount { tag: "v2".to_string(), renders: 14 },
                    ],
                    recent: vec![],
                },
                TemplateSummary {
                    template_name: "letter".to_string(),
                    total_renders: 27,
                    by_tag: vec![TagCount { tag: "latest".to_string(), renders: 27 }],
                    recent: vec![],
                },
                TemplateSummary {
                    template_name: "tiny".to_string(),
                    total_renders: 3,
                    by_tag: vec![TagCount { tag: "latest".to_string(), renders: 3 }],
                    recent: vec![],
                },
            ],
            recent: vec![],
            totals: Totals { renders_24h: 30, success_rate_24h: 0.9, p90_latency_ms_24h: 310 },
        };

        let html = dashboard_page(&summary, &[], now, &en()).into_string();
        // All five chart titles present.
        for title in [
            "Renders per day",
            "Failures per day",
            "Render latency",
            "Latency distribution",
            "Renders per template",
        ] {
            assert!(html.contains(title), "missing chart: {title}");
        }
        // Donut rendered with arc slices and center total.
        assert!(html.contains("donut-svg"));
        assert!(html.contains("stroke-dasharray"));
        assert!(html.contains(">100<")); // center total = 70+27+3
        // Legend lists templates + their tag breakdown, and the "Other" rollup.
        assert!(html.contains("invoice"));
        assert!(html.contains("class=\"swatch\""));
        assert!(html.contains("Other (1)")); // one template below 5%
        // Latency legend shows both series.
        assert!(html.contains("p90"));
        assert!(html.contains("avg"));
    }

    #[test]
    fn test_dashboard_page_german() {
        let now = datetime!(2026-07-09 12:00 UTC);
        let html = dashboard_page(&sample_summary(now), &[], now, &de()).into_string();
        assert!(html.contains("<html lang=\"de\""));
        assert!(html.contains("Übersicht")); // dashboard title + nav
        assert!(html.contains("Vorlagen")); // templates nav
        assert!(html.contains("Erfolgsrate · 24 Std."));
    }

    #[test]
    fn test_templates_page_lists_templates() {
        let templates = vec![sample_template()];
        let html = templates_page(&templates, &en()).into_string();
        assert!(html.contains("Templates"));
        assert!(html.contains("<table"));
        assert!(html.contains("Invoice Template"));
        assert!(html.contains("/templates/invoice"));
        assert!(html.contains("a@b.com"));
    }

    #[test]
    fn test_templates_page_empty_state_prompts_creation() {
        let html = templates_page(&[], &en()).into_string();
        assert!(html.contains("No templates yet."));
        assert!(html.contains("/templates/new"));
    }

    #[test]
    fn test_new_template_page_has_form() {
        let html = new_template_page(&en()).into_string();
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
            r#"#let field(key, default) = data.at(key, default: default)
= #data.customer.name
#field("document_number", "")
"#,
            &[],
            now,
            &en(),
        )
        .into_string();
        assert!(html.contains("Test Render"));
        assert!(html.contains("hx-post=\"/ui/templates/invoice/render\""));
        assert!(html.contains("data.customer.name")); // source prefilled
        assert!(html.contains("/ui/templates/invoice/publish"));
        // Each version links to its tag-specific detail; the current tag is marked.
        assert!(html.contains("href=\"/templates/invoice:v2\""));
        assert!(html.contains("href=\"/templates/invoice:latest\""));
        assert!(html.contains("aria-current=\"true\""));
        // Test render + publish target the viewed tag.
        assert!(html.contains("name=\"tag\" value=\"v2\""));
        // Data references are inferred into a prefilled JSON skeleton + field list.
        assert!(html.contains("<template-detail-page>"));
        assert!(html.contains("customElements.define('template-detail-page'"));
        assert!(html.contains("data-json-input"));
        assert!(html.contains("Available data fields"));
        assert!(html.contains("customer"));
        assert!(html.contains("document_number"));
        assert!(html.contains("data-data-field=\"customer.name\""));
        assert!(html.contains("data-data-field=\"document_number\""));
        assert!(html.contains("class=\"data-fields"));
        assert!(html.contains("class=\"field-check\""));
        assert!(html.contains("id=\"template-renders\""));
        // Delete this version, guarded by a native <dialog> via Invoker Commands.
        assert!(html.contains("/ui/templates/invoice/delete"));
        assert!(html.contains("Delete this version"));
        assert!(html.contains("command=\"show-modal\""));
        assert!(html.contains("commandfor=\"confirm-delete\""));
        assert!(html.contains("<dialog id=\"confirm-delete\""));

        let render_target = html.find("id=\"render-result\"").unwrap();
        let publish_form = html.find("/ui/templates/invoice/publish").unwrap();
        let recent_renders = html.find("Recent renders").unwrap();
        assert!(publish_form < render_target);
        assert!(render_target < recent_renders);
    }

    #[test]
    fn test_infer_data_fields_from_typst_source() {
        let fields = infer_data_fields(
            r#"
#let field(key, default) = data.at(key, default: default)
#data.customer.name
#data.total
#data.at("fallback", default: "x")
#field("document_number", "")
#field("document_number", "")
"#,
        );
        assert_eq!(
            fields,
            vec![
                "customer.name".to_string(),
                "total".to_string(),
                "fallback".to_string(),
                "document_number".to_string(),
            ]
        );
        let sample = sample_data_json(&fields);
        assert!(sample.contains("\"customer\""));
        assert!(sample.contains("\"name\""));
        assert!(sample.contains("\"document_number\""));
    }

    #[test]
    fn test_active_nav_marks_current_and_templates_tags_link() {
        let now = datetime!(2026-07-09 12:00 UTC);
        let dash = dashboard_page(&sample_summary(now), &[], now, &en()).into_string();
        assert!(dash.contains("href=\"/\" aria-current=\"page\""));
        // Templates table renders each tag as a link to its tagged detail.
        let tpls = templates_page(&[sample_template()], &en()).into_string();
        assert!(tpls.contains("href=\"/templates/invoice:latest\""));
    }

    #[test]
    fn test_render_result_fragment_embeds_iframe() {
        let html = render_result_fragment("0192abcd-ef", &en()).into_string();
        assert!(html.contains("<iframe"));
        assert!(html.contains("/api/renders/0192abcd-ef/pdf#view=FitH"));
        assert!(html.contains("open PDF"));
        assert!(html.contains("class=\"card stack render-preview\""));
        assert!(html.contains("class=\"pdf-preview\""));
    }

    #[test]
    fn test_template_renders_section_renders_plain() {
        let now = datetime!(2026-07-09 12:00 UTC);
        let html = template_renders_section(&[], now, &en()).into_string();
        assert!(html.contains("id=\"template-renders\""));
        // No OOB: the render fragment only swaps the PDF preview.
        assert!(!html.contains("hx-swap-oob"));
        assert!(html.contains("Recent renders"));
    }

    #[test]
    fn test_embedded_assets() {
        assert!(APP_CSS.starts_with(b"/* Papermake UI"));
        assert!(HTMX_JS.starts_with(b"var htmx"));
        assert!(LOGO_SVG.starts_with(b"<svg"));
        let css = std::str::from_utf8(APP_CSS).unwrap();
        assert!(css.contains(".pdf-preview"));
        assert!(css.contains("aspect-ratio: 4 / 3"));
        assert!(css.contains("min-height: 32rem"));
        assert!(css.contains(".data-field[data-used] .field-check"));
        let logo = std::str::from_utf8(LOGO_SVG).unwrap();
        assert!(logo.contains("viewBox")); // scales in the favicon + navbar
        assert!(logo.contains("<path"));
    }

    #[test]
    fn test_relative_time_localized() {
        let now = datetime!(2026-07-09 12:00 UTC);
        assert_eq!(
            relative_time(now - time::Duration::seconds(30), now, &en()),
            "30s ago"
        );
        assert_eq!(
            relative_time(now - time::Duration::minutes(5), now, &en()),
            "5m ago"
        );
        assert_eq!(
            relative_time(now - time::Duration::hours(2), now, &de()),
            "vor 2 Std."
        );
    }
}
