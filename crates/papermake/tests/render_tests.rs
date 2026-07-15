use std::sync::Arc;

use papermake::{InMemoryFileSystem, render_template};
use pdf::font::FontType;
use serde_json::json;

#[test]
fn test_render_pdf() {
    let data = json!({
        "name": "World"
    });

    let result = render_template(
        "Hello #data.name!".to_string(),
        Arc::new(InMemoryFileSystem::new()),
        &data,
    );
    assert!(result.is_ok());

    let render_result = result.unwrap();
    assert!(
        render_result.success,
        "render failed: {:?}",
        render_result.errors
    );
    let pdf_bytes = render_result.pdf.expect("successful render includes a PDF");

    assert!(
        pdf_bytes.starts_with(b"%PDF-"),
        "PDF should start with a valid header"
    );

    let file = pdf::file::FileOptions::cached().load(pdf_bytes).unwrap();
    let mut checked_embedded_fonts = 0;
    let mut fonts_without_embedded_data = Vec::new();

    if let Ok(page) = file.get_page(0)
        && let Ok(resources) = page.resources()
    {
        for (_, font_lazy) in resources.fonts.iter() {
            let Ok(font_ref) = font_lazy.load(&file) else {
                continue;
            };

            let font_name = font_ref
                .name
                .as_ref()
                .map(|name| name.to_string())
                .unwrap_or_else(|| "<unnamed>".to_string());

            if !matches!(font_ref.subtype, FontType::Type3) {
                checked_embedded_fonts += 1;
                match font_ref.embedded_data(&file) {
                    Some(Ok(data)) if !data.is_empty() => {}
                    Some(Ok(_)) => {
                        fonts_without_embedded_data
                            .push(format!("{} has an empty embedded font stream", font_name));
                    }
                    Some(Err(err)) => {
                        fonts_without_embedded_data.push(format!(
                            "{} failed to load embedded font data: {err}",
                            font_name
                        ));
                    }
                    None => {
                        fonts_without_embedded_data
                            .push(format!("{} has no embedded font stream", font_name));
                    }
                }
            }
        }
    }

    assert!(
        checked_embedded_fonts > 0,
        "PDF should contain at least one embeddable font"
    );
    assert!(
        fonts_without_embedded_data.is_empty(),
        "PDF fonts should embed their font program data: {}",
        fonts_without_embedded_data.join(", ")
    );
}
