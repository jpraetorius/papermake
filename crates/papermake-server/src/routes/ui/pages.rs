use super::*;

pub(crate) fn template_detail_component_script() -> Markup {
    // Served from /assets (see the router) rather than inlined, so the page needs
    // no inline script and the CSP can keep `script-src 'self'`.
    html! {
        script type="module" src="/assets/template-detail.js" {}
    }
}

// ---------------------------------------------------------------------------
// Pages (pure)
// ---------------------------------------------------------------------------

/// Dashboard: KPI cards, volume sparkline, per-template bars, recent renders.
/// `analytics_unavailable` shows a notice when the aggregate couldn't be loaded,
/// so an outage isn't misread as genuinely-zero activity.
pub fn dashboard_page(
    summary: &Summary,
    templates: &[TemplateInfo],
    analytics_unavailable: bool,
    now: OffsetDateTime,
    t: &I18n,
) -> Markup {
    let body = html! {
        div.split {
            h1 { (t.t("dashboard-title")) }
            a.btn.primary href="/templates/new" { (t.t("btn-new-template")) }
        }

        @if analytics_unavailable {
            div.callout.warn role="alert" { strong { (t.t("analytics-unavailable")) } }
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

/// htmx fragment shown after a failed test render. `message` carries a compile
/// diagnostic when there is one; it is omitted for errors with no safe detail.
pub fn render_error_fragment(message: &str, t: &I18n) -> Markup {
    html! {
        div.callout.warn role="alert" {
            strong { (t.t("render-failed")) }
            @if !message.is_empty() {
                p { (message) }
            }
        }
    }
}
