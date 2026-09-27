use sha2::{Digest, Sha256};
use std::sync::{Arc, OnceLock};
use typst::{
    Library, LibraryExt, World,
    diag::{FileError, FileResult},
    foundations::{Bytes, Datetime},
    layout::PagedDocument,
    syntax::{FileId, Source, VirtualPath},
    text::{Font, FontBook},
    utils::LazyHash,
};

use crate::{ConversionError, ConversionWarning, MAX_PREVIEW_BYTES, RenderedPage, SourceFormat};

const TEMPLATE: &str = r#"
#set page(paper: "a4", margin: (x: 22mm, y: 20mm))
#set text(font: "Noto Sans CJK SC", size: 10.5pt, lang: "zh")
#set par(leading: 0.7em)
#set heading(outlined: true)
"#;

#[derive(Debug, Clone)]
pub(crate) struct CompiledDocument {
    pub document: PagedDocument,
    pub font_set_sha256: String,
    pub warnings: Vec<ConversionWarning>,
}

pub(crate) fn compile(
    source: &str,
    format: SourceFormat,
) -> Result<CompiledDocument, ConversionError> {
    let rendered = match format {
        SourceFormat::Markdown => crate::markdown::to_typst(source)?,
        SourceFormat::Text => crate::markdown::RenderedMarkdown {
            typst: crate::markdown::text_to_typst(source)?,
            warnings: Vec::new(),
        },
        SourceFormat::Typst => crate::markdown::RenderedMarkdown {
            typst: source.into(),
            warnings: Vec::new(),
        },
        SourceFormat::Pdf => {
            return Err(ConversionError::new(
                "unsupported_source_format",
                "PDF is not a Typst source format",
            ));
        }
    };
    let source = if format == SourceFormat::Typst {
        rendered.typst
    } else {
        format!("{TEMPLATE}\n{}", rendered.typst)
    };
    let world = MemoryWorld::new(source)?;
    let result = typst::compile::<PagedDocument>(&world);
    let document = result.output.map_err(|diagnostics| {
        ConversionError::new(
            "document_compile_failed",
            bounded_diagnostics(diagnostics.iter().map(|item| item.message.as_str())),
        )
    })?;
    let mut warnings = rendered.warnings;
    if !result.warnings.is_empty() {
        return Err(ConversionError::new(
            "document_compile_failed",
            bounded_diagnostics(result.warnings.iter().map(|item| item.message.as_str())),
        ));
    }
    warnings.truncate(64);
    Ok(CompiledDocument {
        document,
        font_set_sha256: world.font_set_sha256,
        warnings,
    })
}

pub(crate) fn export_pdf(compiled: &CompiledDocument) -> Result<Vec<u8>, ConversionError> {
    typst_pdf::pdf(&compiled.document, &typst_pdf::PdfOptions::default()).map_err(|diagnostics| {
        ConversionError::new(
            "document_export_failed",
            bounded_diagnostics(diagnostics.iter().map(|item| item.message.as_str())),
        )
    })
}

pub(crate) fn render_page(
    compiled: &CompiledDocument,
    page: u32,
) -> Result<RenderedPage, ConversionError> {
    if page == 0 || page as usize > compiled.document.pages.len() {
        return Err(ConversionError::new(
            "page_out_of_range",
            format!(
                "page {page} is outside 1..={}",
                compiled.document.pages.len()
            ),
        ));
    }
    let page = &compiled.document.pages[page as usize - 1];
    for pixels_per_point_milli in [2_000u32, 1_333, 1_000] {
        let scale = pixels_per_point_milli as f32 / 1_000.0;
        let pixmap = typst_render::render(page, scale);
        if pixmap.width() > 2_500 || pixmap.height() > 3_500 {
            continue;
        }
        let png = pixmap.encode_png().map_err(|error| {
            ConversionError::new(
                "preview_render_failed",
                format!("failed to encode preview PNG: {error}"),
            )
        })?;
        if png.len() <= MAX_PREVIEW_BYTES {
            return Ok(RenderedPage {
                png,
                width: pixmap.width(),
                height: pixmap.height(),
                pixels_per_point_milli,
            });
        }
    }
    Err(ConversionError::new(
        "preview_render_failed",
        "preview page exceeds the 400,000 byte limit at the minimum supported resolution",
    ))
}

struct MemoryWorld {
    library: LazyHash<Library>,
    fixed_fonts: Arc<FixedFonts>,
    main: FileId,
    source: Source,
    font_set_sha256: String,
}

impl MemoryWorld {
    fn new(text: String) -> Result<Self, ConversionError> {
        let main = FileId::new(None, VirtualPath::new("/main.typ"));
        let source = Source::new(main, text);
        let fixed_fonts = fixed_fonts()?;
        Ok(Self {
            library: LazyHash::new(Library::default()),
            fixed_fonts: fixed_fonts.clone(),
            main,
            source,
            font_set_sha256: fixed_fonts.sha256.clone(),
        })
    }
}

struct FixedFonts {
    book: LazyHash<FontBook>,
    fonts: Vec<Font>,
    sha256: String,
}

fn fixed_fonts() -> Result<Arc<FixedFonts>, ConversionError> {
    static FIXED_FONTS: OnceLock<Result<Arc<FixedFonts>, ConversionError>> = OnceLock::new();
    FIXED_FONTS.get_or_init(load_fixed_fonts).clone()
}

fn load_fixed_fonts() -> Result<Arc<FixedFonts>, ConversionError> {
    const NOTO_SANS_SC: &[u8] = include_bytes!("../assets/NotoSansCJKsc-Regular.otf");
    let mut digest = Sha256::new();
    digest.update((NOTO_SANS_SC.len() as u64).to_le_bytes());
    digest.update(NOTO_SANS_SC);
    let fonts = Font::iter(Bytes::new(NOTO_SANS_SC)).collect::<Vec<_>>();
    if fonts.is_empty() {
        return Err(ConversionError::new(
            "font_unavailable",
            "the fixed Typst font set is unavailable",
        ));
    }
    let book = FontBook::from_fonts(&fonts);
    if !book.contains_family("noto sans cjk sc") {
        return Err(ConversionError::new(
            "font_unavailable",
            "the fixed Typst font set does not contain Noto Sans CJK SC",
        ));
    }
    Ok(Arc::new(FixedFonts {
        book: LazyHash::new(book),
        fonts,
        sha256: format!("{:x}", digest.finalize()),
    }))
}

impl World for MemoryWorld {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &self.fixed_fonts.book
    }

    fn main(&self) -> FileId {
        self.main
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.main {
            Ok(self.source.clone())
        } else {
            Err(FileError::NotFound(id.vpath().as_rootless_path().into()))
        }
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        Err(FileError::NotFound(id.vpath().as_rootless_path().into()))
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.fixed_fonts.fonts.get(index).cloned()
    }

    fn today(&self, _offset: Option<i64>) -> Option<Datetime> {
        None
    }
}

const MAX_DIAGNOSTIC_BYTES: usize = 2_048;

/// Truncates to at most `max` bytes without splitting a UTF-8 code point:
/// diagnostics echo arbitrary user text, so a byte-count cut can land inside a
/// multi-byte character and `String::truncate` would panic there.
fn truncate_at_char_boundary(text: &mut String, max: usize) {
    if text.len() <= max {
        return;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

fn bounded_diagnostics<'a>(messages: impl Iterator<Item = &'a str>) -> String {
    let mut output = String::new();
    for message in messages.take(8) {
        if !output.is_empty() {
            output.push_str("; ");
        }
        output.push_str(message);
        if output.len() >= MAX_DIAGNOSTIC_BYTES {
            truncate_at_char_boundary(&mut output, MAX_DIAGNOSTIC_BYTES);
            break;
        }
    }
    if output.is_empty() {
        "document compiler returned an unspecified error".into()
    } else {
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_text_without_filesystem_access() {
        let compiled = compile("Hello", SourceFormat::Text).unwrap();
        assert_eq!(compiled.document.pages.len(), 1);
        let pdf = export_pdf(&compiled).unwrap();
        assert!(pdf.starts_with(b"%PDF-"));
        let png = render_page(&compiled, 1).unwrap();
        assert!(png.png.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn bounded_diagnostics_never_splits_a_code_point() {
        for prefix in 0..4 {
            let message = format!("{}{}", "a".repeat(prefix), "界".repeat(1_000));
            let output = bounded_diagnostics(std::iter::once(message.as_str()));
            assert!(output.len() <= MAX_DIAGNOSTIC_BYTES);
            assert!(output.len() > MAX_DIAGNOSTIC_BYTES - 4);
        }
        let emoji = "😀".repeat(600);
        let output = bounded_diagnostics([emoji.as_str(), emoji.as_str()].into_iter());
        assert!(output.len() <= MAX_DIAGNOSTIC_BYTES);
        assert!(output.ends_with('😀'));
    }

    #[test]
    fn long_unicode_compile_error_is_a_controlled_failure() {
        let error = compile("#panic(\"界\" * 1000)", SourceFormat::Typst).unwrap_err();
        assert_eq!(error.code, "document_compile_failed");
        assert!(error.message.len() <= MAX_DIAGNOSTIC_BYTES);
    }

    #[test]
    fn rejects_typst_injection_as_plain_text() {
        let compiled = compile("#include \"secrets.txt\"", SourceFormat::Text).unwrap();
        assert_eq!(compiled.document.pages.len(), 1);
    }
}
