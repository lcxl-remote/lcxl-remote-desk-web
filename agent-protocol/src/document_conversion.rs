//! Closed contracts for bounded, in-process document conversion and preview.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

use crate::computer_use::{CreatedFileArtifactOutput, ObjectKind, ObjectRef};

pub const DOCUMENT_CONVERSION_SCHEMA_VERSION: u16 = 1;
pub const DOCUMENT_CONVERSION_ADAPTER_VERSION: &str = "document-conversion-in-process/v1";
pub const DOCUMENT_PREVIEW_SPEC_VERSION: u16 = 1;
pub const MAX_DOCUMENT_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_DOCUMENT_OUTPUT_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_DOCUMENT_PREVIEW_BYTES: u64 = 400_000;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DocumentSourceFormat {
    Pdf,
    Markdown,
    Text,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DocumentConversionKind {
    PdfToMarkdown,
    PdfToText,
    MarkdownToPdf,
    TextToPdf,
}

impl DocumentConversionKind {
    pub const fn source_format(self) -> DocumentSourceFormat {
        match self {
            Self::PdfToMarkdown | Self::PdfToText => DocumentSourceFormat::Pdf,
            Self::MarkdownToPdf => DocumentSourceFormat::Markdown,
            Self::TextToPdf => DocumentSourceFormat::Text,
        }
    }

    pub const fn output_suffix(self) -> &'static str {
        match self {
            Self::PdfToMarkdown => ".md",
            Self::PdfToText => ".txt",
            Self::MarkdownToPdf | Self::TextToPdf => ".pdf",
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DocumentPageMarkerStyle {
    None,
    HtmlComment,
    PlainText,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct DocumentPageRange {
    pub start: u32,
    pub end: u32,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct DocumentConversionOptions {
    pub kind: DocumentConversionKind,
    #[serde(default)]
    pub pages: Vec<DocumentPageRange>,
    #[serde(default)]
    pub page_markers: Option<DocumentPageMarkerStyle>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DocumentWarningCode {
    BlankPage,
    ScannedPage,
    FontDecodeIssue,
    TableDegraded,
    LinkDegraded,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct DocumentWarning {
    pub code: DocumentWarningCode,
    pub page: Option<u32>,
    pub detail: String,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct DocumentPreviewParams {
    /// Resolved by the orchestrator from an authenticated same-conversation
    /// result. Models never provide this opaque edge reference.
    pub file: ObjectRef,
    pub conversation_id: String,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct DocumentPreviewOutput {
    pub preview_id: String,
    pub source_digest_sha256: String,
    pub page_count: u32,
    pub template_version: String,
    pub font_set_sha256: String,
    pub engine: String,
    pub warnings: Vec<DocumentWarning>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_page: Option<DocumentPreviewPageOutput>,
}

/// Live, owner-only presentation payload. It is not persisted in model history
/// and does not participate in screenshot evidence verification.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct DocumentPreviewFrame {
    pub descriptor: DocumentPreviewOutput,
    pub page: DocumentPreviewPageDescriptor,
    pub preview_data_url: String,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct DocumentPreviewPageParams {
    pub preview_id: String,
    pub conversation_id: String,
    pub page: u32,
    pub spec_version: u16,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct DocumentPreviewPageOutput {
    pub preview_id: String,
    pub page: u32,
    pub page_count: u32,
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub pixels_per_point_milli: u32,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct DocumentPreviewPageDescriptor {
    pub preview_id: String,
    pub page: u32,
    pub page_count: u32,
    pub width: u32,
    pub height: u32,
    pub pixels_per_point_milli: u32,
}

/// One UI-only rendered page returned by an explicit preview-page request.
/// It deliberately omits the source descriptor: the browser already holds the
/// descriptor emitted when the preview was created, while this response only
/// refreshes one bounded page image.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct DocumentPreviewPageFrame {
    pub page: DocumentPreviewPageDescriptor,
    pub preview_data_url: String,
}

/// Owner UI request for a page from an existing ephemeral preview. This is a
/// browser control message, not an AI tool call and never enters model history.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct DocumentPreviewPageRequest {
    pub conversation_id: String,
    pub preview_id: String,
    pub page: u32,
    pub spec_version: u16,
}

impl DocumentPreviewPageRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.conversation_id.trim().is_empty()
            || self.conversation_id.len() > 256
            || self.preview_id.trim().is_empty()
            || self.preview_id.len() > 256
            || self.page == 0
            || self.spec_version != DOCUMENT_PREVIEW_SPEC_VERSION
        {
            return Err("invalid document preview page request");
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct DocumentPreviewPageResponse {
    pub conversation_id: String,
    pub preview_id: String,
    pub page: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<DocumentPreviewPageFrame>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct DocumentConvertAction {
    pub source: ObjectRef,
    pub destination_parent: ObjectRef,
    pub expected_source_sha256: Option<String>,
    pub output_name: String,
    pub conversion: DocumentConversionOptions,
}

impl DocumentConvertAction {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.source.object_kind != ObjectKind::File
            || self.destination_parent.object_kind != ObjectKind::Directory
            || self.source.token.is_empty()
            || self.destination_parent.token.is_empty()
            || self.output_name.is_empty()
            || self.output_name.len() > 200
            || matches!(self.output_name.as_str(), "." | "..")
            || self.output_name.ends_with(['.', ' '])
            || self
                .output_name
                .chars()
                .any(|character| character.is_control() || "\\/:*?\"<>|".contains(character))
            || !self
                .output_name
                .to_ascii_lowercase()
                .ends_with(self.conversion.kind.output_suffix())
            || self.expected_source_sha256.as_ref().is_some_and(|digest| {
                digest.len() != 64
                    || !digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
            || self.conversion.pages.len() > 64
            || self
                .conversion
                .pages
                .iter()
                .any(|range| range.start == 0 || range.end < range.start)
        {
            return Err("invalid document conversion action");
        }
        match self.conversion.kind {
            DocumentConversionKind::PdfToMarkdown => {}
            DocumentConversionKind::PdfToText => {
                if self.conversion.page_markers == Some(DocumentPageMarkerStyle::HtmlComment) {
                    return Err("HTML page markers are invalid for text output");
                }
            }
            DocumentConversionKind::MarkdownToPdf | DocumentConversionKind::TextToPdf => {
                if !self.conversion.pages.is_empty() || self.conversion.page_markers.is_some() {
                    return Err("PDF extraction options are invalid for PDF generation");
                }
            }
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DocumentConversionDetails {
    PdfExtraction {
        source_pages: u32,
        selected_page_ranges: Vec<DocumentPageRange>,
        converted_pages: u32,
    },
    PdfGeneration {
        output_pages: u32,
        template_version: String,
        font_set_sha256: String,
        engine: String,
    },
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct DocumentArtifactOutput {
    pub artifact: CreatedFileArtifactOutput,
    pub conversion: DocumentConversionKind,
    pub source_digest_sha256: String,
    pub details: DocumentConversionDetails,
    pub warnings: Vec<DocumentWarning>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object_ref(object_kind: ObjectKind) -> ObjectRef {
        ObjectRef {
            token: "opaque".into(),
            snapshot_id: "snapshot".into(),
            object_kind,
            expires_at: "2099-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn conversion_options_reject_unknown_fields_and_cross_direction_options() {
        let unknown = serde_json::json!({
            "kind": "pdf_to_text",
            "pages": [],
            "page_markers": "plain_text",
            "source_format": "pdf"
        });
        assert!(serde_json::from_value::<DocumentConversionOptions>(unknown).is_err());

        let action = DocumentConvertAction {
            source: object_ref(ObjectKind::File),
            destination_parent: object_ref(ObjectKind::Directory),
            expected_source_sha256: None,
            output_name: "report.pdf".into(),
            conversion: DocumentConversionOptions {
                kind: DocumentConversionKind::MarkdownToPdf,
                pages: vec![DocumentPageRange { start: 1, end: 1 }],
                page_markers: None,
            },
        };
        assert_eq!(
            action.validate(),
            Err("PDF extraction options are invalid for PDF generation")
        );
    }

    #[test]
    fn preview_page_request_is_versioned_and_bounded() {
        let valid = DocumentPreviewPageRequest {
            conversation_id: "conversation".into(),
            preview_id: "preview".into(),
            page: 1,
            spec_version: DOCUMENT_PREVIEW_SPEC_VERSION,
        };
        assert_eq!(valid.validate(), Ok(()));

        let mut invalid = valid;
        invalid.page = 0;
        assert!(invalid.validate().is_err());
        invalid.page = 1;
        invalid.spec_version += 1;
        assert!(invalid.validate().is_err());
    }
}
