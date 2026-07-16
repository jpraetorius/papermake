use super::*;

// ---------------------------------------------------------------------------
// Layout + small helpers (pure)
// ---------------------------------------------------------------------------

/// Which top-level nav item is the current location.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Nav {
    Dashboard,
    Templates,
}

/// Shared page shell: stylesheet, htmx, and a navbar with the current path
/// highlighted (bold, accent-colored, with a caret pointing at the content).
pub(crate) fn layout(title: &str, active: Nav, t: &I18n, body: Markup) -> Markup {
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
pub(crate) fn section(title: &str, inner: Markup) -> Markup {
    html! {
        section.stack {
            h2.eyebrow { (title) }
            (inner)
        }
    }
}

pub(crate) fn template_renders_section(
    records: &[RenderRecord],
    now: OffsetDateTime,
    t: &I18n,
) -> Markup {
    html! {
        section #template-renders .stack {
            h2.eyebrow { (t.t("section-recent-renders")) }
            (renders_table(records, now, t))
        }
    }
}

/// A single KPI stat card.
pub(crate) fn stat_card(label: &str, value: Markup) -> Markup {
    html! {
        div.card.kpi.stack style="--gap: 0.35rem;" {
            span.eyebrow { (label) }
            span.num { (value) }
        }
    }
}

/// Status badge for a render record.
pub(crate) fn status_badge(success: bool, t: &I18n) -> Markup {
    html! {
        @if success {
            span.badge.ok { (t.t("status-ok")) }
        } @else {
            span.badge.bad { (t.t("status-failed")) }
        }
    }
}

/// Human-friendly relative time (e.g. "3m ago"), localized.
pub(crate) fn relative_time(ts: OffsetDateTime, now: OffsetDateTime, t: &I18n) -> String {
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
pub(crate) fn label_every(n: usize) -> usize {
    ((n as f64) / 10.0).ceil().max(1.0) as usize
}

/// Compact day label, e.g. "07-09" (month-day).
pub(crate) fn fmt_day(date: time::Date) -> String {
    format!("{:02}-{:02}", u8::from(date.month()), date.day())
}

/// Renders-per-day: one primary bar per day, labelled with the count.
pub(crate) fn volume_bars(points: &[VolumePoint], t: &I18n) -> Markup {
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
pub(crate) fn outcome_bars(points: &[VolumePoint], t: &I18n) -> Markup {
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
pub(crate) fn trend_key(color: &str, label: &str, value: u32, t: &I18n) -> Markup {
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
pub(crate) fn latency_trend(points: &[DurationPoint], t: &I18n) -> Markup {
    if points.is_empty() {
        return html! { p.muted { (t.t("no-data")) } };
    }
    let max = points
        .iter()
        .map(|p| p.p99_duration_ms)
        .max()
        .unwrap_or(0)
        .max(1);
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
pub(crate) fn fmt_ms(ms: u32, t: &I18n) -> String {
    if ms < 1000 {
        format!("{} {}", ms, t.t("unit-ms"))
    } else {
        format!("{} {}", (ms as f64 / 1000.0), t.t("unit-s"))
    }
}

/// Latency distribution as a single stacked bar: one segment per bucket, width
/// proportional to its share, colored cool→hot (fast→slow), with a legend.
pub(crate) fn latency_histogram(buckets: &[DurationBucket], t: &I18n) -> Markup {
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
pub(crate) fn donut(templates: &[TemplateSummary], t: &I18n) -> Markup {
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
    let color = |i: usize| {
        format!(
            "oklch(64% 0.16 {:.0})",
            20.0 + (i as f64) * (300.0 / n as f64)
        )
    };

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
            tags: s
                .by_tag
                .iter()
                .map(|c| (c.tag.clone(), c.renders))
                .collect(),
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
pub(crate) fn renders_table(records: &[RenderRecord], now: OffsetDateTime, t: &I18n) -> Markup {
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

pub(crate) fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}
