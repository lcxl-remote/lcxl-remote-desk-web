//! Framed protocol between a document worker and its Typst sandbox process.
//!
//! Typst source is a program: evaluating it can allocate or loop without
//! bound, and a failed allocation aborts the process. Every Typst compilation
//! and page rendering therefore runs in a short-lived child process with OS
//! resource limits, and only this protocol crosses the boundary. The child
//! reads exactly one request frame from stdin and writes exactly one response
//! frame to stdout; nothing else may be written to stdout.
//!
//! A frame is a little-endian `u32` header length, a JSON header, a
//! little-endian `u32` payload length and the payload bytes. Both lengths are
//! bounded before any allocation.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    ConversionDetails, ConversionError, ConversionKind, ConversionWarning, ConvertOptions,
    MAX_OUTPUT_BYTES, MAX_TEXT_SOURCE_BYTES, SourceFormat,
};

/// Largest JSON header either side accepts.
pub const MAX_HEADER_BYTES: usize = 64 * 1024;
/// Largest payload either side accepts: a source going in or a generated PDF
/// or PNG coming out.
pub const MAX_PAYLOAD_BYTES: usize = if MAX_OUTPUT_BYTES > MAX_TEXT_SOURCE_BYTES {
    MAX_OUTPUT_BYTES
} else {
    MAX_TEXT_SOURCE_BYTES
};

/// One unit of Typst work. The payload carries the UTF-8 source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum SandboxRequest {
    /// Generate a PDF; the response payload is the PDF.
    Convert { kind: ConversionKind },
    /// Compile a preview and render one page; the response payload is a PNG.
    RenderPage { format: SourceFormat, page: u32 },
}

/// The child's answer. Only `Failed` can describe a failure: a crash, a
/// resource limit or a malformed frame is observed by the parent instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum SandboxResponse {
    Converted {
        details: ConversionDetails,
        warnings: Vec<ConversionWarning>,
    },
    RenderedPage {
        page_count: u32,
        template_version: String,
        font_set_sha256: String,
        engine: String,
        warnings: Vec<ConversionWarning>,
        width: u32,
        height: u32,
        pixels_per_point_milli: u32,
    },
    Failed {
        code: String,
        message: String,
    },
}

impl SandboxResponse {
    fn failed(error: ConversionError) -> Self {
        Self::Failed {
            code: error.code.into(),
            message: error.message,
        }
    }
}

/// Error codes the child may report. Anything else collapses to a generic
/// compile failure, so a child cannot inject arbitrary codes.
const KNOWN_CODES: [&str; 14] = [
    "document_compile_failed",
    "document_export_failed",
    "font_unavailable",
    "invalid_conversion_options",
    "invalid_markdown",
    "invalid_source_encoding",
    "markdown_limit_exceeded",
    "output_too_large",
    "page_limit_exceeded",
    "page_out_of_range",
    "preview_render_failed",
    "source_too_large",
    "unsupported_markdown_feature",
    "unsupported_source_format",
];

/// Maps a reported failure back to a [`ConversionError`] with a known code.
pub fn error_from_wire(code: &str, message: String) -> ConversionError {
    let code = KNOWN_CODES
        .iter()
        .find(|known| **known == code)
        .copied()
        .unwrap_or("document_compile_failed");
    let mut message = message;
    if message.len() > 4 * 1024 {
        let mut end = 4 * 1024;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    ConversionError::new(code, message)
}

pub fn write_frame(
    output: &mut impl Write,
    header: &impl Serialize,
    payload: &[u8],
) -> io::Result<()> {
    let header = serde_json::to_vec(header).map_err(io::Error::other)?;
    if header.len() > MAX_HEADER_BYTES || payload.len() > MAX_PAYLOAD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "sandbox frame exceeds its limit",
        ));
    }
    output.write_all(&(header.len() as u32).to_le_bytes())?;
    output.write_all(&header)?;
    output.write_all(&(payload.len() as u32).to_le_bytes())?;
    output.write_all(payload)?;
    output.flush()
}

pub fn read_frame<T: DeserializeOwned>(input: &mut impl Read) -> io::Result<(T, Vec<u8>)> {
    let header = read_bounded(input, MAX_HEADER_BYTES)?;
    let header = serde_json::from_slice(&header)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let payload = read_bounded(input, MAX_PAYLOAD_BYTES)?;
    Ok((header, payload))
}

fn read_bounded(input: &mut impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut length = [0_u8; 4];
    input.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "sandbox frame exceeds its limit",
        ));
    }
    let mut bytes = vec![0_u8; length];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Child side: serve exactly one request.
pub fn serve(input: &mut impl Read, output: &mut impl Write) -> io::Result<()> {
    let (request, source): (SandboxRequest, Vec<u8>) = read_frame(input)?;
    let (response, payload) = handle(request, &source);
    write_frame(output, &response, &payload)
}

fn handle(request: SandboxRequest, source: &[u8]) -> (SandboxResponse, Vec<u8>) {
    match request {
        SandboxRequest::Convert { kind } => {
            if !matches!(
                kind,
                ConversionKind::MarkdownToPdf
                    | ConversionKind::TextToPdf
                    | ConversionKind::TypstToPdf
            ) {
                return (
                    SandboxResponse::failed(ConversionError::new(
                        "invalid_conversion_options",
                        "only Typst-backed conversions run in the sandbox",
                    )),
                    Vec::new(),
                );
            }
            match crate::convert(
                source,
                &ConvertOptions {
                    kind,
                    pages: Vec::new(),
                    page_markers: None,
                },
            ) {
                Ok(output) => (
                    SandboxResponse::Converted {
                        details: output.details,
                        warnings: output.warnings,
                    },
                    output.bytes,
                ),
                Err(error) => (SandboxResponse::failed(error), Vec::new()),
            }
        }
        SandboxRequest::RenderPage { format, page } => {
            let rendered = crate::preview(source, format).and_then(|document| {
                let rendered = document.render_page(page)?;
                Ok((document, rendered))
            });
            match rendered {
                Ok((document, rendered)) => (
                    SandboxResponse::RenderedPage {
                        page_count: document.page_count,
                        template_version: document.template_version.into(),
                        font_set_sha256: document.font_set_sha256,
                        engine: document.engine.into(),
                        warnings: document.warnings,
                        width: rendered.width,
                        height: rendered.height,
                        pixels_per_point_milli: rendered.pixels_per_point_milli,
                    },
                    rendered.png,
                ),
                Err(error) => (SandboxResponse::failed(error), Vec::new()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(request: SandboxRequest, source: &[u8]) -> (SandboxResponse, Vec<u8>) {
        let mut input = Vec::new();
        write_frame(&mut input, &request, source).unwrap();
        let mut output = Vec::new();
        serve(&mut input.as_slice(), &mut output).unwrap();
        read_frame(&mut output.as_slice()).unwrap()
    }

    #[test]
    fn converts_and_renders_through_the_frame_protocol() {
        let (response, pdf) = round_trip(
            SandboxRequest::Convert {
                kind: ConversionKind::TextToPdf,
            },
            b"Hello",
        );
        assert!(matches!(response, SandboxResponse::Converted { .. }));
        assert!(pdf.starts_with(b"%PDF-"));
        let (response, png) = round_trip(
            SandboxRequest::RenderPage {
                format: SourceFormat::Markdown,
                page: 1,
            },
            b"# Title",
        );
        assert!(matches!(
            response,
            SandboxResponse::RenderedPage { page_count: 1, .. }
        ));
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn compile_errors_come_back_as_structured_failures() {
        let (response, payload) = round_trip(
            SandboxRequest::Convert {
                kind: ConversionKind::TypstToPdf,
            },
            "#panic(\"界\" * 1000)".as_bytes(),
        );
        assert!(payload.is_empty());
        let SandboxResponse::Failed { code, message } = response else {
            panic!("expected a failure");
        };
        let error = error_from_wire(&code, message);
        assert_eq!(error.code, "document_compile_failed");
    }

    #[test]
    fn oversized_frames_are_rejected_before_allocation() {
        let mut frame = Vec::new();
        frame.extend_from_slice(&((MAX_HEADER_BYTES as u32) + 1).to_le_bytes());
        assert!(read_frame::<SandboxRequest>(&mut frame.as_slice()).is_err());
        let mut frame = Vec::new();
        let header = br#"{"op":"convert","kind":"text_to_pdf"}"#;
        frame.extend_from_slice(&(header.len() as u32).to_le_bytes());
        frame.extend_from_slice(header);
        frame.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_frame::<SandboxRequest>(&mut frame.as_slice()).is_err());
    }

    #[test]
    fn unknown_codes_and_long_messages_are_normalized() {
        let error = error_from_wire("anything", "界".repeat(10_000));
        assert_eq!(error.code, "document_compile_failed");
        assert!(error.message.len() <= 4 * 1024);
        assert_eq!(
            error_from_wire("page_limit_exceeded", String::new()).code,
            "page_limit_exceeded"
        );
    }

    #[test]
    fn pdf_extraction_never_runs_in_the_sandbox() {
        let (response, _) = round_trip(
            SandboxRequest::Convert {
                kind: ConversionKind::PdfToText,
            },
            b"%PDF-1.4",
        );
        assert!(matches!(response, SandboxResponse::Failed { .. }));
    }
}
