use unpdf::{
    ParseOptions,
    model::Document,
    render::{self, PageSelection, RenderOptions, TableFallback},
};

use crate::{
    ConversionDetails, ConversionError, ConversionKind, ConversionOutput, ConversionWarning,
    ConversionWarningCode, ConvertOptions, MAX_OUTPUT_BYTES, MAX_PAGE_RANGES, MAX_SELECTED_PAGES,
    MAX_SOURCE_PAGES, PageMarkerStyle, PageRange,
};

pub(crate) fn convert(
    source: &[u8],
    options: &ConvertOptions,
) -> Result<ConversionOutput, ConversionError> {
    if !matches!(
        options.kind,
        ConversionKind::PdfToMarkdown | ConversionKind::PdfToText
    ) {
        return Err(ConversionError::new(
            "invalid_conversion_options",
            "PDF extraction received a non-PDF conversion kind",
        ));
    }
    if matches!(options.kind, ConversionKind::PdfToText)
        && matches!(options.page_markers, Some(PageMarkerStyle::HtmlComment))
    {
        return Err(ConversionError::new(
            "invalid_conversion_options",
            "HTML comment page markers are not valid for plain text output",
        ));
    }
    crate::pdf_preflight::validate(source)?;
    let parse_options = ParseOptions::new().text_only().sequential();
    let document =
        unpdf::parse_bytes_with_options(source, parse_options).map_err(map_unpdf_error)?;
    let source_pages = document.page_count();
    if source_pages == 0 || source_pages > MAX_SOURCE_PAGES {
        return Err(ConversionError::new(
            "page_limit_exceeded",
            format!("PDF has {source_pages} pages; expected 1..={MAX_SOURCE_PAGES}"),
        ));
    }
    if document.metadata.encrypted || document.extraction_quality.encrypted {
        return Err(ConversionError::new(
            "encrypted_pdf",
            "encrypted PDFs are not supported",
        ));
    }
    if document.extraction_quality.pages_incomplete
        || document.extraction_quality.unresolved_page_nodes > 0
    {
        return Err(ConversionError::new(
            "incomplete_extraction",
            "the PDF page tree is damaged and extraction would omit pages",
        ));
    }
    let (ranges, selected_pages) = normalize_ranges(&options.pages, source_pages)?;
    let mut warnings = page_warnings(&document, &selected_pages);
    if document.extraction_quality.suppressed_text_runs > 0
        || document.extraction_quality.replacement_char_count > 0
    {
        warnings.push(ConversionWarning {
            code: ConversionWarningCode::FontDecodeIssue,
            page: None,
            detail: format!(
                "PDF extraction dropped {} unreadable text run(s) and produced {} replacement character(s)",
                document.extraction_quality.suppressed_text_runs,
                document.extraction_quality.replacement_char_count
            ),
        });
    }
    warnings.truncate(64);

    let mut rendered_pages = Vec::with_capacity(selected_pages.len());
    for page_number in &selected_pages {
        let page = document
            .get_page(*page_number)
            .ok_or_else(|| {
                ConversionError::new("page_out_of_range", "selected PDF page is missing")
            })?
            .clone();
        let mut one = Document::new();
        one.metadata = document.metadata.clone();
        one.add_page(page);
        let render_options = RenderOptions::new()
            .with_table_fallback(TableFallback::Ascii)
            .with_pages(PageSelection::All);
        let rendered = match options.kind {
            ConversionKind::PdfToMarkdown => render::to_markdown(&one, &render_options),
            ConversionKind::PdfToText => render::to_text(&one, &render_options),
            ConversionKind::MarkdownToPdf | ConversionKind::TextToPdf => unreachable!(),
        }
        .map_err(map_unpdf_error)?;
        rendered_pages.push((*page_number, rendered));
    }

    let has_text = rendered_pages
        .iter()
        .any(|(_, value)| !value.trim().is_empty());
    if !has_text {
        if warnings
            .iter()
            .any(|warning| warning.code == ConversionWarningCode::ScannedPage)
        {
            return Err(ConversionError::new(
                "ocr_required",
                "the selected PDF pages are image-only and require OCR",
            ));
        }
        return Err(ConversionError::new(
            "no_extractable_text",
            "the selected PDF pages contain no extractable text",
        ));
    }

    let marker = options.page_markers.unwrap_or(PageMarkerStyle::None);
    let mut output = String::new();
    for (index, (page, rendered)) in rendered_pages.into_iter().enumerate() {
        if index > 0 {
            output.push_str("\n\n");
        }
        match marker {
            PageMarkerStyle::None => {}
            PageMarkerStyle::HtmlComment => {
                output.push_str(&format!("<!-- lcxl-page: {page} -->\n\n"));
            }
            PageMarkerStyle::PlainText => {
                output.push_str(&format!("--- page {page} ---\n\n"));
            }
        }
        output.push_str(rendered.trim());
    }
    if output.len() > MAX_OUTPUT_BYTES {
        return Err(ConversionError::new(
            "output_too_large",
            "extracted document exceeds the 32 MiB output limit",
        ));
    }
    Ok(ConversionOutput {
        bytes: output.into_bytes(),
        media_type: options.kind.media_type(),
        details: ConversionDetails::PdfExtraction {
            source_pages,
            selected_page_ranges: ranges,
            converted_pages: selected_pages.len() as u32,
        },
        warnings,
    })
}

fn normalize_ranges(
    requested: &[PageRange],
    page_count: u32,
) -> Result<(Vec<PageRange>, Vec<u32>), ConversionError> {
    if requested.len() > MAX_PAGE_RANGES {
        return Err(ConversionError::new(
            "page_limit_exceeded",
            format!("at most {MAX_PAGE_RANGES} page ranges are supported"),
        ));
    }
    let mut ranges = if requested.is_empty() {
        vec![PageRange {
            start: 1,
            end: page_count,
        }]
    } else {
        requested.to_vec()
    };
    if ranges
        .iter()
        .any(|range| range.start == 0 || range.start > range.end || range.end > page_count)
    {
        return Err(ConversionError::new(
            "page_out_of_range",
            format!("page ranges must fall inside 1..={page_count}"),
        ));
    }
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut normalized: Vec<PageRange> = Vec::new();
    for range in ranges {
        if let Some(last) = normalized.last_mut()
            && range.start <= last.end.saturating_add(1)
        {
            last.end = last.end.max(range.end);
        } else {
            normalized.push(range);
        }
    }
    let selected = normalized
        .iter()
        .flat_map(|range| range.start..=range.end)
        .collect::<Vec<_>>();
    if selected.len() > MAX_SELECTED_PAGES {
        return Err(ConversionError::new(
            "page_limit_exceeded",
            format!("at most {MAX_SELECTED_PAGES} PDF pages may be converted at once"),
        ));
    }
    Ok((normalized, selected))
}

fn page_warnings(document: &Document, selected: &[u32]) -> Vec<ConversionWarning> {
    let mut warnings = Vec::new();
    for page_number in selected {
        let Some(page) = document.get_page(*page_number) else {
            continue;
        };
        if page.text_op_count == 0 && page.image_op_count > 0 {
            warnings.push(ConversionWarning {
                code: ConversionWarningCode::ScannedPage,
                page: Some(*page_number),
                detail: "page is image-only; OCR is not included in this conversion".into(),
            });
        } else if page.text_op_count == 0 && page.image_op_count == 0 {
            warnings.push(ConversionWarning {
                code: ConversionWarningCode::BlankPage,
                page: Some(*page_number),
                detail: "page has no text or image drawing operations".into(),
            });
        }
    }
    warnings
}

fn map_unpdf_error(error: unpdf::Error) -> ConversionError {
    let text = error.to_string();
    let lower = text.to_ascii_lowercase();
    if lower.contains("encrypt") || lower.contains("password") {
        ConversionError::new("encrypted_pdf", "encrypted PDFs are not supported")
    } else {
        ConversionError::new("invalid_pdf", format!("PDF extraction failed: {text}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_are_sorted_and_merged() {
        let (ranges, pages) = normalize_ranges(
            &[
                PageRange { start: 4, end: 5 },
                PageRange { start: 1, end: 2 },
                PageRange { start: 3, end: 3 },
            ],
            10,
        )
        .unwrap();
        assert_eq!(ranges, vec![PageRange { start: 1, end: 5 }]);
        assert_eq!(pages, vec![1, 2, 3, 4, 5]);
    }
}
