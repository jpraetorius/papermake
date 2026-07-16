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
    // The template list now lives on its own page, not the dashboard.
    assert!(!html.contains("Invoice Template"));
    // Navbar links to the dedicated templates page.
    assert!(html.contains("href=\"/templates\""));
    // Own assets are wired.
    assert!(html.contains("/assets/app.css"));
    assert!(html.contains("/assets/htmx.min.js"));
    // Favicon + logo.
    assert!(html.contains("rel=\"icon\""));
    assert!(html.contains("/assets/logo.svg"));
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
            VolumePoint {
                date: d,
                renders: 10,
                failures: 1,
            },
            VolumePoint {
                date: d,
                renders: 20,
                failures: 3,
            },
        ],
        duration_by_day: vec![
            DurationPoint {
                date: d,
                avg_duration_ms: 120.0,
                p90_duration_ms: 240,
                p95_duration_ms: 260,
                p99_duration_ms: 280,
            },
            DurationPoint {
                date: d,
                avg_duration_ms: 150.0,
                p90_duration_ms: 310,
                p95_duration_ms: 340,
                p99_duration_ms: 380,
            },
        ],
        duration_histogram: vec![
            DurationBucket {
                upper_ms: Some(100),
                count: 12,
            },
            DurationBucket {
                upper_ms: Some(250),
                count: 6,
            },
            DurationBucket {
                upper_ms: None,
                count: 2,
            },
        ],
        // 70 / 27 / 3 out of 100 → the 3% template folds into "Other".
        templates: vec![
            TemplateSummary {
                template_name: "invoice".to_string(),
                total_renders: 70,
                by_tag: vec![
                    TagCount {
                        tag: "latest".to_string(),
                        renders: 56,
                    },
                    TagCount {
                        tag: "v2".to_string(),
                        renders: 14,
                    },
                ],
                recent: vec![],
            },
            TemplateSummary {
                template_name: "letter".to_string(),
                total_renders: 27,
                by_tag: vec![TagCount {
                    tag: "latest".to_string(),
                    renders: 27,
                }],
                recent: vec![],
            },
            TemplateSummary {
                template_name: "tiny".to_string(),
                total_renders: 3,
                by_tag: vec![TagCount {
                    tag: "latest".to_string(),
                    renders: 3,
                }],
                recent: vec![],
            },
        ],
        recent: vec![],
        totals: Totals {
            renders_24h: 30,
            success_rate_24h: 0.9,
            p90_latency_ms_24h: 310,
        },
    };

    let html = dashboard_page(&summary, &[], now, &en()).into_string();
    // Chart regions render both bar/series charts and the template donut.
    assert!(html.contains("bar-chart"));
    assert!(html.contains("donut-chart"));
    assert!(html.contains("donut-svg"));
    assert!(html.contains("role=\"img\""));
}

#[test]
fn test_dashboard_page_german() {
    let now = datetime!(2026-07-09 12:00 UTC);
    let html = dashboard_page(&sample_summary(now), &[], now, &de()).into_string();
    assert!(html.contains("<html lang=\"de\""));
}

#[test]
fn test_templates_page_lists_templates() {
    let templates = vec![sample_template()];
    let html = templates_page(&templates, &en()).into_string();
    assert!(html.contains("<table"));
    assert!(html.contains("Invoice Template"));
    assert!(html.contains("/templates/invoice"));
    assert!(html.contains("a@b.com"));
}

#[test]
fn test_templates_page_empty_state_prompts_creation() {
    let html = templates_page(&[], &en()).into_string();
    assert!(html.contains("/templates/new"));
}

#[test]
fn test_new_template_page_has_form() {
    let html = new_template_page(&en()).into_string();
    assert!(html.contains("action=\"/ui/templates\""));
    assert!(html.contains("name=\"name\""));
    assert!(html.contains("name=\"main_typ\""));
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
    // The web component is loaded from an external asset (no inline script).
    assert!(html.contains(r#"<script type="module" src="/assets/template-detail.js">"#));
    assert!(html.contains("data-json-input"));
    assert!(html.contains("data-data-field=\"customer.name\""));
    assert!(html.contains("data-data-field=\"document_number\""));
    assert!(html.contains("class=\"data-fields"));
    assert!(html.contains("class=\"field-check\""));
    assert!(html.contains("id=\"template-renders\""));
    // Delete this version, guarded by a native <dialog> via Invoker Commands.
    assert!(html.contains("/ui/templates/invoice/delete"));
    assert!(html.contains("command=\"show-modal\""));
    assert!(html.contains("commandfor=\"confirm-delete\""));
    assert!(html.contains("<dialog id=\"confirm-delete\""));

    let render_target = html.find("id=\"render-result\"").unwrap();
    let publish_form = html.find("/ui/templates/invoice/publish").unwrap();
    let recent_renders = html.find("id=\"template-renders\"").unwrap();
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
}

#[test]
fn test_template_renders_section_renders_plain() {
    let now = datetime!(2026-07-09 12:00 UTC);
    let html = template_renders_section(&[], now, &en()).into_string();
    assert!(html.contains("id=\"template-renders\""));
    // No OOB: the render fragment only swaps the PDF preview.
    assert!(!html.contains("hx-swap-oob"));
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

#[tokio::test]
async fn ui_responses_carry_security_headers() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let app = router().with_state(crate::test_support::state(crate::test_support::registry()));
    let response = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    let headers = response.headers();
    assert_eq!(headers.get(X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
    assert_eq!(headers.get(X_FRAME_OPTIONS).unwrap(), "SAMEORIGIN");
    let csp = headers
        .get(CONTENT_SECURITY_POLICY)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(csp.contains("default-src 'self'"));
    assert!(csp.contains("frame-ancestors 'self'"));
    // Scripts are external-only: no 'unsafe-inline' on script-src.
    assert!(csp.contains("script-src 'self'"));
    assert!(!csp.contains("script-src 'self' 'unsafe-inline'"));
}

#[tokio::test]
async fn template_detail_script_is_served_as_an_asset() {
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    let app = router().with_state(crate::test_support::state(crate::test_support::registry()));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/assets/template-detail.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "text/javascript; charset=utf-8"
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(
        String::from_utf8_lossy(&body).contains("customElements.define('template-detail-page'")
    );
}
