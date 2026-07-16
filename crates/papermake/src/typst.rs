use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;
use typst::Library;
use typst::LibraryExt;
use typst::diag::{FileError, FileResult};
use typst::foundations::{Bytes, Datetime, Dict, Duration, IntoValue};
use typst::syntax::{FileId, Source};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst_kit::fonts::FontSource;

// Define a static lazy variable to hold the cached fonts
static CACHED_FONTS: Lazy<(FontBook, Vec<Font>)> = Lazy::new(|| {
    let mut book = FontBook::new();
    let mut fonts = Vec::new();

    // Embedded fonts are always available and load infallibly.
    for (font, info) in typst_kit::fonts::embedded() {
        book.push(info);
        fonts.push(font);
    }

    // System fonts, plus an optional user-provided directory (`FONTS_DIR`).
    // Sources are loaded eagerly so `book` and `fonts` stay index-aligned.
    for (path, info) in typst_kit::fonts::system() {
        if let Some(font) = path.load() {
            book.push(info);
            fonts.push(font);
        }
    }
    // FONTS_DIR may list several directories (OS path separator, `:` on Unix),
    // so a corporate image can bake fonts in AND an operator can mount extra
    // fonts via a volume — both scanned, no rebuild needed. Missing dirs are
    // skipped.
    if let Some(fonts_dir) = std::env::var_os("FONTS_DIR") {
        for dir in std::env::split_paths(&fonts_dir) {
            for (path, info) in typst_kit::fonts::scan(&dir) {
                if let Some(font) = path.load() {
                    book.push(info);
                    fonts.push(font);
                }
            }
        }
    }

    (book, fonts)
});

/// Eagerly load and cache the font set (embedded + system + `FONTS_DIR`).
///
/// Fonts are otherwise loaded lazily on the first render, making it slow. Call
/// this once at process startup so that cost is paid at boot and every render —
/// including the first — stays fast.
pub fn preload_fonts() {
    // Dereferencing the lazy static forces its initialization.
    let _ = &*CACHED_FONTS;
}

/// Parse every font face from raw TTF/OTF/TTC bytes — e.g. a font shipped as a
/// template asset. Malformed data yields an empty vec (never panics).
pub fn load_font_faces(data: &[u8]) -> Vec<Font> {
    let bytes = Bytes::new(data.to_vec());
    let face_count = typst::text::FontInfo::iter(data).count();
    (0..face_count as u32)
        .filter_map(|index| Font::new(bytes.clone(), index))
        .collect()
}

/// File system abstraction for Typst rendering
///
/// This trait provides file access to TypstWorld during rendering,
/// allowing integration with various storage backends.
pub trait RenderFileSystem: Send + Sync {
    /// Get file content by path
    fn get_file(&self, path: &str) -> Result<Vec<u8>, FileError>;
}

/// Main interface that determines the environment for Typst.
pub struct PapermakeWorld {
    /// The content of a source.
    source: Source,

    /// The standard library.
    library: LazyHash<Library>,

    /// Metadata about all known fonts.
    book: LazyHash<FontBook>,

    /// Metadata about all known fonts.
    fonts: Vec<Font>,

    /// Map of all known files.
    files: Arc<Mutex<HashMap<FileId, FileEntry>>>,

    /// Cache directory (e.g. where packages are downloaded to).
    #[allow(dead_code)]
    cache_directory: PathBuf,

    /// Datetime.
    time: time::OffsetDateTime,

    /// File system abstraction for loading template files/assets
    file_system: Option<Arc<dyn RenderFileSystem>>,
}

impl std::fmt::Debug for PapermakeWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TypstWorld")
            .field("source", &self.source)
            .field("library", &self.library)
            .field("book", &self.book)
            .field("fonts_count", &self.fonts.len())
            .field(
                "files_count",
                &self.files.lock().map(|f| f.len()).unwrap_or(0),
            )
            .field("cache_directory", &self.cache_directory)
            .field("time", &self.time)
            .field("has_file_system", &self.file_system.is_some())
            .finish()
    }
}

/// Whether the template source already declares a top-level `data` binding
/// (`#let data …` in markup or `let data …` in code), so the prelude binding
/// should be skipped to avoid shadowing it. A trailing identifier char (as in
/// `database` or the kebab-case `data-table`) does not count — Typst
/// identifiers may contain `-`, so a hyphen continues the name rather than
/// ending a bare `data` binding.
fn binds_data(source: &str) -> bool {
    source.lines().any(|line| {
        let trimmed = line.trim_start();
        for prefix in ["#let data", "let data"] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let next = rest.chars().next();
                if next.is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '-') {
                    return true;
                }
            }
        }
        false
    })
}

impl PapermakeWorld {
    /// Create a new TypstWorld with the given template content and data
    pub fn new(template_content: String, data: String) -> Self {
        // Use the cached fonts directly
        let (book, fonts) = CACHED_FONTS.clone();

        let mut inputs_dict = Dict::new();
        inputs_dict.insert("data".into(), data.as_str().into_value());

        let library = Library::builder().with_inputs(inputs_dict).build();

        // Bind `data` from the injected inputs — but only if the template does
        // not already define its own `data`. Injecting unconditionally would
        // double-bind (shadow) a template that declares `data` itself and shift
        // every reported error offset relative to the user's source.
        let source_text = if binds_data(&template_content) {
            template_content
        } else {
            format!(
                "#let data = json(bytes(sys.inputs.data))\n{}",
                template_content
            )
        };

        Self {
            library: LazyHash::new(library),
            book: LazyHash::new(book),
            fonts,
            source: Source::detached(source_text),
            time: time::OffsetDateTime::now_utc(),
            cache_directory: std::env::var_os("CACHE_DIRECTORY")
                .map(|os_path| os_path.into())
                .unwrap_or(std::env::temp_dir()),
            files: Arc::new(Mutex::new(HashMap::new())),
            file_system: None,
        }
    }

    /// Create TypstWorld with file system support for resolving imports
    pub fn with_file_system(
        template_content: String,
        data: String,
        file_system: Arc<dyn RenderFileSystem>,
    ) -> Self {
        let mut world = Self::new(template_content, data);
        world.file_system = Some(file_system);
        world
    }

    /// Like [`with_file_system`](Self::with_file_system) but also registers
    /// extra font faces (e.g. from a template's bundled font assets) on top of
    /// the process font set. Additive: same-family system/`FONTS_DIR` fonts
    /// remain available.
    pub fn with_file_system_and_fonts(
        template_content: String,
        data: String,
        file_system: Arc<dyn RenderFileSystem>,
        extra_fonts: Vec<Font>,
    ) -> Self {
        let mut world = Self::with_file_system(template_content, data, file_system);
        if !extra_fonts.is_empty() {
            let mut book = (*world.book).clone();
            for font in &extra_fonts {
                book.push(font.info().clone());
            }
            world.book = LazyHash::new(book);
            world.fonts.extend(extra_fonts);
        }
        world
    }

    /// Update the data available to the template
    pub fn update_data(&mut self, data: String) -> Result<(), crate::error::PapermakeError> {
        // Update the data in the inputs dictionary
        let mut inputs_dict = Dict::new();
        inputs_dict.insert("data".into(), data.as_str().into_value());

        // Create a new library with updated inputs
        let library = Library::builder().with_inputs(inputs_dict).build();
        self.library = LazyHash::new(library);

        Ok(())
    }
}

/// A File that will be stored in the HashMap.
#[derive(Clone, Debug)]
struct FileEntry {
    bytes: Bytes,
    source: Option<Source>,
}

impl FileEntry {
    fn new(bytes: Vec<u8>, source: Option<Source>) -> Self {
        Self {
            bytes: Bytes::new(bytes),
            source,
        }
    }

    fn source(&mut self, id: FileId) -> FileResult<Source> {
        let source = if let Some(source) = &self.source {
            source
        } else {
            let contents = std::str::from_utf8(&self.bytes).map_err(|_| FileError::InvalidUtf8)?;
            let contents = contents.trim_start_matches('\u{feff}');
            let source = Source::new(id, contents.into());
            self.source.insert(source)
        };
        Ok(source.clone())
    }
}

impl PapermakeWorld {
    /// Helper to handle file requests.
    ///
    /// Requests will be either in packages or a local file.
    fn file(&self, id: FileId) -> FileResult<FileEntry> {
        let mut files = self.files.lock().map_err(|_| FileError::AccessDenied)?;
        if let Some(entry) = files.get(&id) {
            return Ok(entry.clone());
        }

        // If we have a file system, try to resolve the file
        if let Some(fs) = &self.file_system {
            let path = self.id_to_path(id)?;

            let content = fs
                .get_file(&path)
                .map_err(|_| FileError::NotFound(path.into()))?;

            let entry = FileEntry::new(content, None);
            files.insert(id, entry.clone());
            return Ok(entry);
        }

        Err(FileError::NotFound(format!("{:?}", id).into()))
    }

    /// Convert FileId to file path
    fn id_to_path(&self, id: FileId) -> FileResult<String> {
        // Use the virtual path's rooted form (e.g. `/header.typ`). File system
        // backends are responsible for interpreting the leading slash.
        Ok(id.vpath().get_with_slash().to_string())
    }
}

/// This is the interface we have to implement such that `typst` can compile it.
impl typst::World for PapermakeWorld {
    /// Standard library.
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }

    /// Metadata about all known Books.
    fn book(&self) -> &LazyHash<FontBook> {
        &self.book
    }

    /// Accessing the main source file.
    fn main(&self) -> FileId {
        self.source.id()
    }

    /// Accessing a specified source file (based on `FileId`).
    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.source.id() {
            Ok(self.source.clone())
        } else {
            self.file(id)?.source(id)
        }
    }

    /// Accessing a specified file (non-file).
    fn file(&self, id: FileId) -> FileResult<Bytes> {
        self.file(id).map(|file| file.bytes.clone())
    }

    /// Accessing a specified font per index of font book.
    fn font(&self, id: usize) -> Option<Font> {
        self.fonts.get(id).cloned()
    }

    /// Get the current date.
    ///
    /// Optionally, an offset in hours is given.
    fn today(&self, offset: Option<Duration>) -> Option<Datetime> {
        let seconds = offset
            .map(|o| time::Duration::from(o).whole_seconds())
            .unwrap_or(0);
        let offset = time::UtcOffset::from_whole_seconds(seconds.try_into().ok()?).ok()?;
        let time = self.time.checked_to_offset(offset)?;
        Some(Datetime::Date(time.date()))
    }
}

/// Simple in-memory file system implementation for testing
pub struct InMemoryFileSystem {
    files: HashMap<String, Vec<u8>>,
}

impl Default for InMemoryFileSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryFileSystem {
    pub fn new() -> Self {
        Self {
            files: HashMap::new(),
        }
    }

    pub fn add_file<P: AsRef<str>>(&mut self, path: P, content: Vec<u8>) {
        self.files.insert(path.as_ref().to_string(), content);
    }
}

impl RenderFileSystem for InMemoryFileSystem {
    fn get_file(&self, path: &str) -> Result<Vec<u8>, FileError> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| FileError::NotFound(path.into()))
    }
}

#[cfg(test)]
mod tests {
    use crate::render::render_template;

    use super::*;
    use std::sync::Arc;

    #[test]
    fn font_helpers_are_safe_for_startup_and_malformed_template_font_data() {
        preload_fonts();
        assert!(load_font_faces(b"not a font").is_empty());
    }

    #[test]
    fn binds_data_detects_an_existing_data_binding() {
        assert!(binds_data("#let data = (name: \"x\")\nHello"));
        assert!(binds_data("  #let data=1"));
        assert!(binds_data("let data = 1")); // code mode
        // No `data` binding → prelude should be injected.
        assert!(!binds_data("Hello #data.name"));
        assert!(!binds_data("#let total = 1"));
        // A longer identifier must not be mistaken for `data`.
        assert!(!binds_data("#let database = connect()"));
        // Typst identifiers are kebab-case-legal: `data-table` is its own
        // binding, not a `data` binding, so the prelude must still be injected.
        assert!(!binds_data("#let data-table = (1, 2)"));
        assert!(!binds_data("#let data-2 = 1"));
    }

    #[test]
    fn new_injects_prelude_only_when_data_is_not_already_bound() {
        // Template without its own `data` gets the injected prelude.
        let injected = PapermakeWorld::new("Hello #data.name".to_string(), "{}".to_string());
        assert!(injected.source.text().starts_with("#let data = json("));

        // Template that binds `data` itself is left untouched (no double-bind).
        let own = PapermakeWorld::new(
            "#let data = (name: \"Ada\")\nHi".to_string(),
            "{}".to_string(),
        );
        assert!(!own.source.text().contains("json(bytes(sys.inputs.data))"));
    }

    #[tokio::test]
    async fn test_simple_template_rendering() {
        let template = r#"
            #set page(width: 200pt, height: 100pt)
            Hello #data.name!
        "#;

        let data = serde_json::json!({
            "name": "World"
        });

        let fs = Arc::new(InMemoryFileSystem::new());
        let result = render_template(template.to_string(), fs, &data);

        assert!(result.is_ok());
        let render_result = result.unwrap();
        assert!(render_result.success);
        assert!(render_result.pdf.is_some());
        let pdf_bytes = render_result.pdf.unwrap();
        assert!(!pdf_bytes.is_empty());
        assert!(pdf_bytes.starts_with(b"%PDF"));
    }

    #[tokio::test]
    async fn test_template_with_imports() {
        let main_template = r#"
            #import "header.typ": make_header
            #set page(width: 200pt, height: 100pt)
            #make_header(data.title)
            Content: #data.content
        "#;

        let header_template = r#"
            #let make_header(title) = [
                = #title
            ]
        "#;

        let mut fs = InMemoryFileSystem::new();
        fs.add_file("/header.typ", header_template.as_bytes().to_vec());

        let data = serde_json::json!({
            "title": "My Document",
            "content": "This is the content"
        });

        let result = render_template(main_template.to_string(), Arc::new(fs), &data);

        assert!(result.is_ok());
        let render_result = result.unwrap();
        assert!(
            render_result.success,
            "Render failed: {}",
            render_result
                .errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<String>>()
                .join(", ")
        );
        assert!(render_result.pdf.is_some());
        let pdf_bytes = render_result.pdf.unwrap();
        assert!(!pdf_bytes.is_empty());
        assert!(pdf_bytes.starts_with(b"%PDF"));
    }

    #[test]
    fn test_typst_world_creation() {
        let world = PapermakeWorld::new("Hello".to_string(), "{}".to_string());
        let source = typst::World::source(&world, typst::World::main(&world)).unwrap();
        assert!(
            source
                .text()
                .starts_with("#let data = json(bytes(sys.inputs.data))")
        );
        assert!(source.text().contains("Hello"));
    }

    #[test]
    fn test_typst_world_with_file_system() {
        let mut fs = InMemoryFileSystem::new();
        fs.add_file("/asset.txt", b"asset".to_vec());
        let world =
            PapermakeWorld::with_file_system("Hello".to_string(), "{}".to_string(), Arc::new(fs));
        let file_id = FileId::new(typst::syntax::RootedPath::new(
            typst::syntax::VirtualRoot::Project,
            typst::syntax::VirtualPath::new("/asset.txt").unwrap(),
        ));
        let bytes = typst::World::file(&world, file_id).unwrap();
        assert_eq!(bytes.as_slice(), b"asset");
    }

    #[test]
    fn test_error_display() {
        use crate::error::{CompilationError, PapermakeError};
        let error = PapermakeError::Compilation(CompilationError::TemplateCompilation {
            message: "test error".to_string(),
        });
        assert!(format!("{}", error).contains("test error"));
    }
}
