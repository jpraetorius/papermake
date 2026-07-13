use std::sync::Arc;

use papermake::{InMemoryFileSystem, render_template};
use pdf::font::FontType;
use serde_json::json;

#[test]
fn test_render_pdf() {
    // Valid data
    let data = json!({
        "name": "World"
    });

    // Render
    let result = render_template(
        "#set text(font: \"Arial\")\nHello #data.name!".to_string(),
        Arc::new(InMemoryFileSystem::new()),
        &data,
    );
    assert!(result.is_ok());

    let pdf_bytes = result.unwrap();
    assert!(pdf_bytes.pdf.is_some());

    // Verify PDF structure instead of saving to file
    // 1. Check for PDF header
    let header = &pdf_bytes.pdf.as_ref().unwrap()[0..8];
    assert!(
        header == b"%PDF-1.7" || header == b"%PDF-1.6" || header == b"%PDF-1.5",
        "PDF should start with a valid header"
    );

    // Parse PDF and check for font
    let file = pdf::file::FileOptions::cached()
        .load(pdf_bytes.pdf.as_ref().unwrap().clone())
        .unwrap();
    let mut found_arial = false;
    let mut checked_embedded_fonts = 0;
    let mut fonts_without_embedded_data = Vec::new();

    // Check each page's resources for fonts
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

            if font_name.to_lowercase().contains("arial") {
                found_arial = true;
            }

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

    assert!(found_arial, "PDF should contain Arial font");
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
