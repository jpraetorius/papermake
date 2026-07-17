//! PDF rendering functionality
//!
//! This module provides the main template rendering functionality,
//! converting Typst templates with JSON data into PDF documents.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use typst::World;
use typst::WorldExt;
use typst_pdf::{PdfOptions, PdfStandards};

use crate::RenderFileSystem;
use crate::error::{CompilationError, ConfigError, PapermakeError, Result};
use crate::typst::PapermakeWorld;

/// Individual rendering error with location information
///
/// This struct captures detailed information about a single rendering error,
/// including its location in the source and a descriptive message.
#[derive(Debug, Serialize, Clone)]
pub struct RenderError {
    /// The error message
    pub message: String,
    /// Starting position in the source
    pub start: usize,
    /// Ending position in the source
    pub end: usize,
    /// Optional file path where the error occurred
    pub file: Option<String>,
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.file {
            Some(file) => write!(f, "{}:{}-{}: {}", file, self.start, self.end, self.message),
            None => write!(f, "{}:{}: {}", self.start, self.end, self.message),
        }
    }
}

/// Result of template rendering operation
///
/// Contains either the successfully generated PDF bytes or detailed error information.
/// Even when PDF generation succeeds, there may be warnings in the errors vector.
#[derive(Debug, Serialize)]
pub struct RenderResult {
    /// The generated PDF bytes (None if compilation failed)
    pub pdf: Option<Vec<u8>>,
    /// List of compilation errors and warnings
    pub errors: Vec<RenderError>,
    /// Whether the rendering was successful (PDF was generated)
    pub success: bool,
}

/// PDF standard the exported document should conform to.
///
/// Enforced by Typst's PDF exporter. The `V*` variants only pin the base PDF
/// version; the PDF/A variants produce archivable output (the `b`/`a` suffix is
/// the conformance level — `b` = visual reproduction, `a` = additionally
/// tagged/accessible), and `A3*` allows arbitrary embedded files, the basis for
/// hybrid e-invoice formats such as ZUGFeRD/Factur-X. `Ua1` is the PDF/UA-1
/// accessibility standard. Serde names match the Typst CLI (`1.7`, `2.0`,
/// `a-2b`, `a-3b`, `ua-1`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PdfStandard {
    /// PDF 1.7 (the default)
    #[serde(rename = "1.7")]
    V1_7,
    /// PDF 2.0
    #[serde(rename = "2.0")]
    V2_0,
    /// PDF/A-2a (archival, tagged/accessible)
    #[serde(rename = "a-2a")]
    A2a,
    /// PDF/A-2b (archival)
    #[serde(rename = "a-2b")]
    A2b,
    /// PDF/A-3a (archival, tagged/accessible, allows embedded files)
    #[serde(rename = "a-3a")]
    A3a,
    /// PDF/A-3b (archival, allows embedded files)
    #[serde(rename = "a-3b")]
    A3b,
    /// PDF/A-4 (archival, based on PDF 2.0)
    #[serde(rename = "a-4")]
    A4,
    /// PDF/UA-1 (universal accessibility). Requires the template to set a
    /// document title (`#set document(title: [...])`); otherwise export fails
    /// with "PDF/UA-1 error: missing document title".
    #[serde(rename = "ua-1")]
    Ua1,
}

impl PdfStandard {
    /// The canonical Typst-CLI name for this standard (matches the serde name).
    pub const fn as_str(&self) -> &'static str {
        match self {
            PdfStandard::V1_7 => "1.7",
            PdfStandard::V2_0 => "2.0",
            PdfStandard::A2a => "a-2a",
            PdfStandard::A2b => "a-2b",
            PdfStandard::A3a => "a-3a",
            PdfStandard::A3b => "a-3b",
            PdfStandard::A4 => "a-4",
            PdfStandard::Ua1 => "ua-1",
        }
    }
}

impl From<PdfStandard> for typst_pdf::PdfStandard {
    fn from(standard: PdfStandard) -> Self {
        match standard {
            PdfStandard::V1_7 => typst_pdf::PdfStandard::V_1_7,
            PdfStandard::V2_0 => typst_pdf::PdfStandard::V_2_0,
            PdfStandard::A2a => typst_pdf::PdfStandard::A_2a,
            PdfStandard::A2b => typst_pdf::PdfStandard::A_2b,
            PdfStandard::A3a => typst_pdf::PdfStandard::A_3a,
            PdfStandard::A3b => typst_pdf::PdfStandard::A_3b,
            PdfStandard::A4 => typst_pdf::PdfStandard::A_4,
            PdfStandard::Ua1 => typst_pdf::PdfStandard::Ua_1,
        }
    }
}

/// Options controlling PDF export.
#[derive(Debug, Clone, Default)]
pub struct RenderOptions {
    /// PDF standards the output must conform to (empty = plain PDF 1.7).
    pub pdf_standards: Vec<PdfStandard>,
}

impl RenderOptions {
    /// Options for PDF/A-3b output (e.g. as the base for ZUGFeRD/Factur-X e-invoices).
    pub fn pdf_a3b() -> Self {
        Self {
            pdf_standards: vec![PdfStandard::A3b],
        }
    }

    /// Canonical form used for render identity and PDF export. Explicit PDF
    /// 1.7 is the default, so it is represented by an empty standards list.
    pub fn canonicalized(&self) -> Self {
        if self.pdf_standards.as_slice() == [PdfStandard::V1_7] {
            Self::default()
        } else {
            self.clone()
        }
    }
}

/// Build typst-pdf export options from [`RenderOptions`].
fn pdf_options(options: &RenderOptions) -> Result<PdfOptions> {
    let options = options.canonicalized();
    let standards: Vec<typst_pdf::PdfStandard> = options
        .pdf_standards
        .iter()
        .copied()
        .map(Into::into)
        .collect();
    // `PdfStandards::new` rejects conflicting/duplicate standards. Its error
    // type isn't `Display`, so surface it via its debug form.
    let standards = PdfStandards::new(&standards).map_err(|e| {
        PapermakeError::Config(ConfigError::InvalidConfig {
            setting: "pdf_standards".to_string(),
            reason: format!("{e:?}"),
        })
    })?;
    // PDF/A conformance requires a document creation date. When the template
    // doesn't set one (`document.date` is auto), fall back to the current time,
    // like `typst compile` does. Plain PDF output stays timestamp-free (and thus
    // byte-reproducible).
    let timestamp = options
        .pdf_standards
        .iter()
        .any(PdfStandard::requires_fallback_timestamp)
        .then(current_timestamp)
        .flatten();
    Ok(PdfOptions {
        standards,
        timestamp,
        ..Default::default()
    })
}

impl PdfStandard {
    fn requires_fallback_timestamp(&self) -> bool {
        matches!(
            self,
            PdfStandard::A2a
                | PdfStandard::A2b
                | PdfStandard::A3a
                | PdfStandard::A3b
                | PdfStandard::A4
        )
    }
}

/// The current UTC time as a typst-pdf [`Timestamp`], if representable.
fn current_timestamp() -> Option<typst_pdf::Timestamp> {
    let now = time::OffsetDateTime::now_utc();
    let datetime = typst::foundations::Datetime::from_ymd_hms(
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
    )?;
    Some(typst_pdf::Timestamp::new_utc(datetime))
}

/// Render a Typst template to PDF
///
/// This is the main public API for template compilation. It takes a template string,
/// a file system for resolving imports, and JSON data to inject into the template.
///
/// # Arguments
///
/// * `main_typ` - The main Typst template content as a string
/// * `file_system` - File system abstraction for resolving imports and assets
/// * `data` - JSON data to inject into the template
///
/// # Returns
///
/// Returns a `RenderResult` containing either the PDF bytes (on success) or
/// detailed error information (on failure).
///
/// # Errors
///
/// This function can return various errors:
/// - `DataError` - JSON serialization issues
/// - `CompilationError` - Typst compilation failures
/// - `FileSystemError` - File access issues during import resolution
///
/// # Example
///
/// ```rust,no_run
/// use papermake::{render_template, typst::InMemoryFileSystem};
/// use std::sync::Arc;
///
/// let template = "Hello #data.name!";
/// let fs = Arc::new(InMemoryFileSystem::new());
/// let data = serde_json::json!({ "name": "World" });
///
/// let result = render_template(template.to_string(), fs, &data).unwrap();
/// if result.success {
///     println!("PDF generated: {} bytes", result.pdf.unwrap().len());
/// } else {
///     for error in result.errors {
///         println!("Error: {}", error);
///     }
/// }
/// ```
pub fn render_template(
    main_typ: String,
    file_system: Arc<dyn RenderFileSystem>,
    data: &serde_json::Value,
) -> Result<RenderResult> {
    render_template_with_options(main_typ, file_system, data, &RenderOptions::default())
}

/// Render a Typst template to PDF with explicit export options
///
/// Behaves like [`render_template`], but lets the caller control PDF export,
/// e.g. requesting PDF/A-3b conformant output:
///
/// ```rust,no_run
/// use papermake::{render_template_with_options, RenderOptions, typst::InMemoryFileSystem};
/// use std::sync::Arc;
///
/// let result = render_template_with_options(
///     "Hello #data.name!".to_string(),
///     Arc::new(InMemoryFileSystem::new()),
///     &serde_json::json!({ "name": "World" }),
///     &RenderOptions::pdf_a3b(),
/// ).unwrap();
/// ```
pub fn render_template_with_options(
    main_typ: String,
    file_system: Arc<dyn RenderFileSystem>,
    data: &serde_json::Value,
    options: &RenderOptions,
) -> Result<RenderResult> {
    let pdf_opts = pdf_options(options)?;
    let data_str = serde_json::to_string(&data)?;
    let world = PapermakeWorld::with_file_system(main_typ, data_str, file_system);
    Ok(compile_world(&world, &pdf_opts))
}

/// Render a template with additional font faces registered (e.g. fonts shipped
/// as template assets), on top of the process font set. See
/// [`render_template`] for the base behavior.
pub fn render_template_with_fonts(
    main_typ: String,
    file_system: Arc<dyn RenderFileSystem>,
    data: &serde_json::Value,
    extra_fonts: Vec<crate::Font>,
) -> Result<RenderResult> {
    render_template_with_fonts_and_options(
        main_typ,
        file_system,
        data,
        extra_fonts,
        &RenderOptions::default(),
    )
}

/// Render a template with both extra font faces and explicit PDF export options
/// (e.g. template-bundled fonts plus PDF/A-3b output).
pub fn render_template_with_fonts_and_options(
    main_typ: String,
    file_system: Arc<dyn RenderFileSystem>,
    data: &serde_json::Value,
    extra_fonts: Vec<crate::Font>,
    options: &RenderOptions,
) -> Result<RenderResult> {
    let pdf_opts = pdf_options(options)?;
    let data_str = serde_json::to_string(&data)?;
    let world =
        PapermakeWorld::with_file_system_and_fonts(main_typ, data_str, file_system, extra_fonts);
    Ok(compile_world(&world, &pdf_opts))
}

/// How much of Typst's global `comemo` memoization cache to retain after a
/// render. The cache is process-global and every `typst::compile` grows it, so
/// a long-running render worker accumulates it unboundedly (observed: multi-GB,
/// still held at idle) unless we evict.
///
/// `evict(max_age)` does NOT clear the cache — it only drops entries not
/// *touched* within the last `max_age` evict cycles. So the warm, reusable work
/// (template parse, font loading, data-independent layout) is touched on every
/// render and survives; only per-input memoized results — keyed on that item's
/// data, useless to the next item — age out. This is how Typst's `watch` mode
/// keeps a warm cache while staying bounded. Raise this to retain work that's
/// only reused sporadically (a few renders apart) at the cost of more memory.
const COMEMO_EVICT_MAX_AGE: usize = 16;

/// Compile a prepared world to a `RenderResult` (PDF bytes or diagnostics).
fn compile_world(world: &PapermakeWorld, pdf_opts: &PdfOptions) -> RenderResult {
    let compile_result = typst::compile(world);

    let mut errors = Vec::new();
    let mut pdf = None;
    let mut success = false;

    match compile_result.output {
        Ok(document) => {
            // Compilation succeeded, generate PDF
            match typst_pdf::pdf(&document, pdf_opts) {
                Ok(pdf_bytes) => {
                    pdf = Some(pdf_bytes);
                    success = true;
                }
                Err(pdf_error) => {
                    errors.push(RenderError {
                        message: format!("PDF generation failed: {:?}", pdf_error),
                        start: 0,
                        end: 0,
                        file: None,
                    });
                }
            }
        }
        Err(diagnostics) => {
            // Compilation failed, collect diagnostic information
            for diagnostic in diagnostics {
                let span = diagnostic.span;
                let mut render_error = RenderError {
                    message: diagnostic.message.to_string(),
                    start: 0,
                    end: 0,
                    file: None,
                };

                // Try to get source location information
                if let Some(id) = span.id()
                    && let Ok(_source) = world.source(id)
                {
                    render_error.file = Some(format!("{:?}", id));
                    if let Some(range) = world.range(span) {
                        render_error.start = range.start;
                        render_error.end = range.end;
                    }
                }

                errors.push(render_error);
            }
        }
    }

    // Bound Typst's global memoization cache (see COMEMO_EVICT_MAX_AGE).
    comemo::evict(COMEMO_EVICT_MAX_AGE);

    RenderResult {
        pdf,
        errors,
        success,
    }
}

/// Render a template with caching support
///
/// This function allows reusing a compiled world for multiple renders with different data,
/// which can improve performance when rendering the same template multiple times.
///
/// # Arguments
///
/// * `main_typ` - The main Typst template content as a string
/// * `file_system` - File system abstraction for resolving imports and assets
/// * `data` - JSON data to inject into the template
/// * `world_cache` - Optional cached world to reuse (will be updated with new data)
///
/// # Returns
///
/// Returns a `RenderResult` containing either the PDF bytes or error information.
///
/// # Performance Note
///
/// When providing a cached world, make sure the template content hasn't changed,
/// as this function only updates the data, not the template structure.
pub fn render_template_with_cache(
    main_typ: String,
    file_system: Arc<dyn RenderFileSystem>,
    data: serde_json::Value,
    world_cache: Option<&mut PapermakeWorld>,
) -> Result<RenderResult> {
    render_template_with_cache_and_options(
        main_typ,
        file_system,
        data,
        world_cache,
        &RenderOptions::default(),
    )
}

/// Render a cached template with explicit PDF export options.
///
/// Behaves like [`render_template_with_cache`], but lets the caller control PDF
/// export (e.g. request PDF/A-3b) on the warm-world path used by batch rendering.
/// The requested standards affect only PDF export — the cached, data-independent
/// compilation work is reused across renders exactly as before.
pub fn render_template_with_cache_and_options(
    main_typ: String,
    file_system: Arc<dyn RenderFileSystem>,
    data: serde_json::Value,
    world_cache: Option<&mut PapermakeWorld>,
    options: &RenderOptions,
) -> Result<RenderResult> {
    let pdf_opts = pdf_options(options)?;
    let data_str = serde_json::to_string(&data)?;

    let world = match world_cache {
        Some(cached_world) => {
            // Update the data in the existing world
            cached_world.update_data(data_str).map_err(|e| {
                PapermakeError::Compilation(CompilationError::DataInjection {
                    reason: format!("Failed to update cached world data: {}", e),
                })
            })?;
            cached_world
        }
        None => {
            // Create a new world if no cache is provided
            return render_template_with_options(main_typ, file_system, &data, options);
        }
    };

    // Compile the warm world and export with the requested options. Sharing
    // `compile_world` keeps the export path (and comemo eviction) identical to
    // the non-cached renders.
    Ok(compile_world(world, &pdf_opts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typst::InMemoryFileSystem;

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    fn render_with(options: &RenderOptions) -> RenderResult {
        render_template_with_options(
            "Hello #data.name!".to_string(),
            Arc::new(InMemoryFileSystem::new()),
            &serde_json::json!({ "name": "World" }),
            options,
        )
        .unwrap()
    }

    fn assert_successful_pdf(result: RenderResult) -> Vec<u8> {
        assert!(result.success, "render failed: {:?}", result.errors);
        let pdf = result.pdf.expect("successful render has PDF bytes");
        assert!(pdf.starts_with(b"%PDF"));
        pdf
    }

    #[test]
    fn default_render_is_plain_pdf() {
        let result = render_with(&RenderOptions::default());
        let pdf = assert_successful_pdf(result);
        // No PDF/A conformance metadata without a requested standard.
        assert!(!contains(&pdf, b"pdfaid"));
    }

    #[test]
    fn explicit_pdf_1_7_matches_default_render_bytes() {
        let default = assert_successful_pdf(render_with(&RenderOptions::default()));
        let explicit = assert_successful_pdf(render_with(&RenderOptions {
            pdf_standards: vec![PdfStandard::V1_7],
        }));

        assert_eq!(default, explicit);
    }

    #[test]
    fn pdf_a3b_render_declares_conformance() {
        let result = render_with(&RenderOptions::pdf_a3b());
        let pdf = assert_successful_pdf(result);
        // typst-pdf writes pdfaid:part / pdfaid:conformance into the XMP metadata.
        assert!(contains(&pdf, b"pdfaid"));
    }

    #[test]
    fn render_with_fonts_accepts_no_template_bundled_fonts() {
        let result = render_template_with_fonts(
            "Hello #data.name!".to_string(),
            Arc::new(InMemoryFileSystem::new()),
            &serde_json::json!({ "name": "World" }),
            Vec::new(),
        )
        .unwrap();

        assert_successful_pdf(result);
    }

    #[test]
    fn render_with_cache_reuses_supplied_world() {
        let fs = Arc::new(InMemoryFileSystem::new());
        let initial_data = serde_json::to_string(&serde_json::json!({ "name": "Initial" }))
            .expect("test data serializes");
        let mut world = PapermakeWorld::with_file_system(
            "Hello #data.name!".to_string(),
            initial_data,
            fs.clone(),
        );

        let result = render_template_with_cache(
            "ignored when cache is supplied".to_string(),
            fs,
            serde_json::json!({ "name": "Updated" }),
            Some(&mut world),
        )
        .unwrap();

        assert_successful_pdf(result);
    }

    #[test]
    fn every_supported_standard_renders() {
        // Each standard we expose must actually produce a PDF (guards against a
        // typst-pdf variant that rejects our template). The accessibility
        // profiles (PDF/UA-1, and the tagged PDF/A-*a levels for full
        // conformance) require a document title, so the template sets one.
        let template = "#set document(title: [Test])\nHello #data.name!".to_string();
        for std in [
            PdfStandard::V1_7,
            PdfStandard::V2_0,
            PdfStandard::A2a,
            PdfStandard::A2b,
            PdfStandard::A3a,
            PdfStandard::A3b,
            PdfStandard::A4,
            PdfStandard::Ua1,
        ] {
            let result = render_template_with_options(
                template.clone(),
                Arc::new(InMemoryFileSystem::new()),
                &serde_json::json!({ "name": "World" }),
                &RenderOptions {
                    pdf_standards: vec![std],
                },
            )
            .unwrap();
            assert!(
                result.success,
                "render for {} failed: {:?}",
                std.as_str(),
                result.errors
            );
            assert!(result.pdf.unwrap().starts_with(b"%PDF"));
        }
    }

    #[test]
    fn pdf_standard_as_str_matches_serde() {
        for std in [
            PdfStandard::V1_7,
            PdfStandard::V2_0,
            PdfStandard::A2a,
            PdfStandard::A2b,
            PdfStandard::A3a,
            PdfStandard::A3b,
            PdfStandard::A4,
            PdfStandard::Ua1,
        ] {
            let serde_name = serde_json::to_string(&std).unwrap();
            assert_eq!(serde_name, format!("\"{}\"", std.as_str()));
        }
    }

    #[test]
    fn conflicting_standards_are_rejected() {
        let options = RenderOptions {
            pdf_standards: vec![PdfStandard::A2b, PdfStandard::A3b],
        };
        let result = render_template_with_options(
            "Hello".to_string(),
            Arc::new(InMemoryFileSystem::new()),
            &serde_json::json!({}),
            &options,
        );
        assert!(matches!(result, Err(PapermakeError::Config(_))));
    }

    #[test]
    fn pdf_standard_serde_uses_typst_cli_names() {
        assert_eq!(
            serde_json::to_string(&PdfStandard::A3b).unwrap(),
            "\"a-3b\""
        );
        assert_eq!(
            serde_json::from_str::<PdfStandard>("\"a-2b\"").unwrap(),
            PdfStandard::A2b
        );
        assert_eq!(
            serde_json::from_str::<PdfStandard>("\"1.7\"").unwrap(),
            PdfStandard::V1_7
        );
    }
}
