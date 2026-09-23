//! Bounded document conversion and ephemeral Typst preview state.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use desk_agent_protocol::computer_use::{
    ComputerActionOutput, ComputerActionResultClass, ComputerActionStepFact,
    CreatedFileArtifactOutput,
};
use desk_agent_protocol::data_lineage::ContentRef;
use desk_agent_protocol::document_conversion::{
    DocumentArtifactOutput, DocumentConversionDetails, DocumentConversionKind,
    DocumentConversionOptions, DocumentConvertAction, DocumentPageMarkerStyle, DocumentPageRange,
    DocumentPreviewOutput, DocumentPreviewPageOutput, DocumentPreviewPageParams,
    DocumentPreviewParams, DocumentWarning, DocumentWarningCode, MAX_DOCUMENT_OUTPUT_BYTES,
    MAX_DOCUMENT_PREVIEW_BYTES, MAX_DOCUMENT_SOURCE_BYTES,
};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_document_conversion as engine;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::file_reference_store::{self, publication};

const PREVIEW_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_PREVIEWS_PER_CONVERSATION: usize = 2;

#[derive(Clone)]
struct PreviewEntry {
    actor_id: String,
    device_id: String,
    conversation_id: String,
    created_at: Instant,
    last_accessed_at: Instant,
    document: engine::PreviewDocument,
}

#[derive(Default)]
struct PreviewStore {
    entries: HashMap<String, PreviewEntry>,
}

fn store() -> &'static Mutex<PreviewStore> {
    static STORE: OnceLock<Mutex<PreviewStore>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(PreviewStore::default()))
}

fn semaphore() -> Arc<Semaphore> {
    static SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    Arc::clone(SEMAPHORE.get_or_init(|| Arc::new(Semaphore::new(1))))
}

pub async fn acquire_slot() -> Result<OwnedSemaphorePermit, AgentError> {
    semaphore().acquire_owned().await.map_err(|_| {
        error(
            AgentErrorKind::Internal,
            "document conversion worker is shutting down",
        )
    })
}

pub fn create_preview(
    params: DocumentPreviewParams,
    actor_id: String,
    device_id: String,
) -> Result<DocumentPreviewOutput, AgentError> {
    if params.conversation_id.trim().is_empty() {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "document preview requires a conversation identity",
        ));
    }
    let source = file_reference_store::read_verified_document_bytes(
        &params.file,
        engine::MAX_TEXT_SOURCE_BYTES as u64,
    )?;
    let format = preview_format(&source.display_name)?;
    let document = engine::preview(&source.bytes, format).map_err(engine_error)?;
    let preview_id = uuid::Uuid::new_v4().to_string();
    let now = Instant::now();
    let first_page = document.render_page(1).map_err(engine_error)?;
    if first_page.png.len() as u64 > MAX_DOCUMENT_PREVIEW_BYTES {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "document preview page exceeds the image limit",
        ));
    }
    let output = DocumentPreviewOutput {
        preview_id: preview_id.clone(),
        source_digest_sha256: source.sha256.clone(),
        page_count: document.page_count,
        template_version: document.template_version.into(),
        font_set_sha256: document.font_set_sha256.clone(),
        engine: document.engine.into(),
        warnings: document.warnings.iter().cloned().map(map_warning).collect(),
        first_page: Some(DocumentPreviewPageOutput {
            preview_id: preview_id.clone(),
            page: 1,
            page_count: document.page_count,
            png: first_page.png,
            width: first_page.width,
            height: first_page.height,
            pixels_per_point_milli: first_page.pixels_per_point_milli,
        }),
    };
    let mut guard = store().lock().map_err(|_| {
        error(
            AgentErrorKind::Internal,
            "document preview store is unavailable",
        )
    })?;
    guard
        .entries
        .retain(|_, entry| now.duration_since(entry.created_at) < PREVIEW_TTL);
    while guard
        .entries
        .values()
        .filter(|entry| {
            entry.actor_id == actor_id
                && entry.device_id == device_id
                && entry.conversation_id == params.conversation_id
        })
        .count()
        >= MAX_PREVIEWS_PER_CONVERSATION
    {
        let oldest = guard
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry.actor_id == actor_id
                    && entry.device_id == device_id
                    && entry.conversation_id == params.conversation_id
            })
            .min_by_key(|(_, entry)| entry.last_accessed_at)
            .map(|(id, _)| id.clone());
        let Some(oldest) = oldest else { break };
        guard.entries.remove(&oldest);
    }
    guard.entries.insert(
        preview_id,
        PreviewEntry {
            actor_id,
            device_id,
            conversation_id: params.conversation_id,
            created_at: now,
            last_accessed_at: now,
            document,
        },
    );
    Ok(output)
}

pub fn render_preview_page(
    params: DocumentPreviewPageParams,
    actor_id: &str,
    device_id: &str,
) -> Result<DocumentPreviewPageOutput, AgentError> {
    if params.spec_version
        != desk_agent_protocol::document_conversion::DOCUMENT_PREVIEW_SPEC_VERSION
    {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "unsupported document preview page specification version",
        ));
    }
    let now = Instant::now();
    let mut guard = store().lock().map_err(|_| {
        error(
            AgentErrorKind::Internal,
            "document preview store is unavailable",
        )
    })?;
    guard
        .entries
        .retain(|_, entry| now.duration_since(entry.created_at) < PREVIEW_TTL);
    let entry = guard.entries.get_mut(&params.preview_id).ok_or_else(|| {
        error(
            AgentErrorKind::InvalidInput,
            "document preview expired or does not exist",
        )
    })?;
    if entry.actor_id != actor_id
        || entry.device_id != device_id
        || entry.conversation_id != params.conversation_id
    {
        return Err(error(
            AgentErrorKind::PermissionDenied,
            "document preview does not belong to this actor, device, and conversation",
        ));
    }
    let page_count = entry.document.page_count;
    let rendered = entry
        .document
        .render_page(params.page)
        .map_err(engine_error)?;
    if rendered.png.len() as u64 > MAX_DOCUMENT_PREVIEW_BYTES {
        return Err(error(
            AgentErrorKind::OutputLimitExceeded,
            "rendered preview exceeds the 400 KB page limit",
        ));
    }
    entry.last_accessed_at = now;
    Ok(DocumentPreviewPageOutput {
        preview_id: params.preview_id,
        page: params.page,
        page_count,
        png: rendered.png,
        width: rendered.width,
        height: rendered.height,
        pixels_per_point_milli: rendered.pixels_per_point_milli,
    })
}

pub fn convert_and_publish(
    action: &DocumentConvertAction,
    require_active: impl Fn() -> Result<(), AgentError>,
) -> publication::Receipt {
    if let Err(message) = action.validate() {
        return failed(message.into());
    }
    let source = match file_reference_store::read_verified_document_bytes(
        &action.source,
        MAX_DOCUMENT_SOURCE_BYTES,
    ) {
        Ok(source) => source,
        Err(error) => return failed(error.message),
    };
    if action
        .expected_source_sha256
        .as_ref()
        .is_some_and(|expected| expected != &source.sha256)
    {
        return failed("source digest changed after approval; no output was created".into());
    }
    if let Err(error) =
        validate_source_kind(&source.display_name, &source.bytes, action.conversion.kind)
    {
        return failed(error.message);
    }
    let options = map_options(&action.conversion);
    let converted = match engine::convert(&source.bytes, &options) {
        Ok(output) => output,
        Err(error) => return failed(engine_error(error).message),
    };
    if converted.bytes.len() as u64 > MAX_DOCUMENT_OUTPUT_BYTES {
        return failed("converted document exceeds the 32 MiB output limit".into());
    }
    if let Err(error) = require_active() {
        return failed(error.message);
    }
    let artifact = match publication::create_document_artifact(
        &action.destination_parent,
        &action.output_name,
        &converted.bytes,
    ) {
        Ok(artifact) => artifact,
        Err(failure) => return (failure.class, vec![], Some(failure.message), None),
    };
    let file = CreatedFileArtifactOutput {
        file: artifact.file.clone(),
        file_name: artifact.file_name.clone(),
        media_type: converted.media_type.into(),
        size_bytes: artifact.byte_len,
        digest_sha256: artifact.sha256.clone(),
        content: ContentRef::Artifact {
            artifact_id: artifact.file.token.clone(),
            sha256: artifact.sha256.clone(),
            size_bytes: artifact.byte_len,
            media_type: converted.media_type.into(),
        },
    };
    let output = DocumentArtifactOutput {
        artifact: file,
        conversion: action.conversion.kind,
        source_digest_sha256: source.sha256,
        details: map_details(converted.details),
        warnings: converted.warnings.into_iter().map(map_warning).collect(),
    };
    (
        ComputerActionResultClass::Verified,
        vec![ComputerActionStepFact {
            index: 0,
            changed: true,
            verified: true,
            summary: format!(
                "created {} ({} bytes, sha256={})",
                artifact.file_name, artifact.byte_len, artifact.sha256
            ),
        }],
        Some("document converted in process and published with create-new semantics".into()),
        Some(ComputerActionOutput::DocumentArtifact(output)),
    )
}

fn failed(message: String) -> publication::Receipt {
    (
        ComputerActionResultClass::Failed,
        vec![],
        Some(message),
        None,
    )
}

fn preview_format(name: &str) -> Result<engine::SourceFormat, AgentError> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".md") || lower.ends_with(".markdown") {
        Ok(engine::SourceFormat::Markdown)
    } else if lower.ends_with(".txt") {
        Ok(engine::SourceFormat::Text)
    } else {
        Err(error(
            AgentErrorKind::InvalidInput,
            "Typst preview supports only .md, .markdown, and .txt files",
        ))
    }
}

fn validate_source_kind(
    name: &str,
    bytes: &[u8],
    kind: DocumentConversionKind,
) -> Result<(), AgentError> {
    let lower = name.to_ascii_lowercase();
    let valid = match kind {
        DocumentConversionKind::PdfToMarkdown | DocumentConversionKind::PdfToText => {
            lower.ends_with(".pdf") && bytes.starts_with(b"%PDF-")
        }
        DocumentConversionKind::MarkdownToPdf => {
            lower.ends_with(".md") || lower.ends_with(".markdown")
        }
        DocumentConversionKind::TextToPdf => lower.ends_with(".txt"),
    };
    valid.then_some(()).ok_or_else(|| {
        error(
            AgentErrorKind::InvalidInput,
            "source file type does not match the requested document conversion",
        )
    })
}

fn map_options(options: &DocumentConversionOptions) -> engine::ConvertOptions {
    engine::ConvertOptions {
        kind: match options.kind {
            DocumentConversionKind::PdfToMarkdown => engine::ConversionKind::PdfToMarkdown,
            DocumentConversionKind::PdfToText => engine::ConversionKind::PdfToText,
            DocumentConversionKind::MarkdownToPdf => engine::ConversionKind::MarkdownToPdf,
            DocumentConversionKind::TextToPdf => engine::ConversionKind::TextToPdf,
        },
        pages: options
            .pages
            .iter()
            .map(|range| engine::PageRange {
                start: range.start,
                end: range.end,
            })
            .collect(),
        page_markers: options.page_markers.map(|style| match style {
            DocumentPageMarkerStyle::None => engine::PageMarkerStyle::None,
            DocumentPageMarkerStyle::HtmlComment => engine::PageMarkerStyle::HtmlComment,
            DocumentPageMarkerStyle::PlainText => engine::PageMarkerStyle::PlainText,
        }),
    }
}

fn map_details(details: engine::ConversionDetails) -> DocumentConversionDetails {
    match details {
        engine::ConversionDetails::PdfExtraction {
            source_pages,
            selected_page_ranges,
            converted_pages,
        } => DocumentConversionDetails::PdfExtraction {
            source_pages,
            selected_page_ranges: selected_page_ranges
                .into_iter()
                .map(|range| DocumentPageRange {
                    start: range.start,
                    end: range.end,
                })
                .collect(),
            converted_pages,
        },
        engine::ConversionDetails::PdfGeneration {
            output_pages,
            template_version,
            font_set_sha256,
            engine,
        } => DocumentConversionDetails::PdfGeneration {
            output_pages,
            template_version,
            font_set_sha256,
            engine,
        },
    }
}

fn map_warning(warning: engine::ConversionWarning) -> DocumentWarning {
    DocumentWarning {
        code: match warning.code {
            engine::ConversionWarningCode::BlankPage => DocumentWarningCode::BlankPage,
            engine::ConversionWarningCode::ScannedPage => DocumentWarningCode::ScannedPage,
            engine::ConversionWarningCode::FontDecodeIssue => DocumentWarningCode::FontDecodeIssue,
            engine::ConversionWarningCode::TableDegraded => DocumentWarningCode::TableDegraded,
            engine::ConversionWarningCode::LinkDegraded => DocumentWarningCode::LinkDegraded,
        },
        page: warning.page,
        detail: warning.detail,
    }
}

fn engine_error(cause: engine::ConversionError) -> AgentError {
    error(
        match cause.code {
            "source_too_large" | "output_too_large" | "page_limit_exceeded" => {
                AgentErrorKind::OutputLimitExceeded
            }
            _ => AgentErrorKind::InvalidInput,
        },
        format!("{}: {}", cause.code, cause.message),
    )
}

fn error(kind: AgentErrorKind, message: impl Into<String>) -> AgentError {
    AgentError {
        kind,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::document_conversion::DOCUMENT_PREVIEW_SPEC_VERSION;

    fn params(preview_id: &str, conversation_id: &str) -> DocumentPreviewPageParams {
        DocumentPreviewPageParams {
            preview_id: preview_id.into(),
            conversation_id: conversation_id.into(),
            page: 1,
            spec_version: DOCUMENT_PREVIEW_SPEC_VERSION,
        }
    }

    #[test]
    fn preview_pages_are_ephemeral_and_bound_to_actor_device_and_conversation() {
        let document = engine::preview(
            b"# Preview\n\nBound content",
            engine::SourceFormat::Markdown,
        )
        .unwrap();
        let now = Instant::now();
        let live_id = uuid::Uuid::new_v4().to_string();
        let expired_id = uuid::Uuid::new_v4().to_string();
        let expired_at = now
            .checked_sub(PREVIEW_TTL + Duration::from_secs(1))
            .unwrap();
        {
            let mut guard = store().lock().unwrap();
            guard.entries.insert(
                live_id.clone(),
                PreviewEntry {
                    actor_id: "owner".into(),
                    device_id: "device".into(),
                    conversation_id: "conversation".into(),
                    created_at: now,
                    last_accessed_at: now,
                    document: document.clone(),
                },
            );
            guard.entries.insert(
                expired_id.clone(),
                PreviewEntry {
                    actor_id: "owner".into(),
                    device_id: "device".into(),
                    conversation_id: "conversation".into(),
                    created_at: expired_at,
                    last_accessed_at: expired_at,
                    document,
                },
            );
        }

        let wrong_actor =
            render_preview_page(params(&live_id, "conversation"), "other", "device").unwrap_err();
        assert_eq!(wrong_actor.kind, AgentErrorKind::PermissionDenied);
        let wrong_device =
            render_preview_page(params(&live_id, "conversation"), "owner", "other").unwrap_err();
        assert_eq!(wrong_device.kind, AgentErrorKind::PermissionDenied);
        let wrong_conversation =
            render_preview_page(params(&live_id, "other"), "owner", "device").unwrap_err();
        assert_eq!(wrong_conversation.kind, AgentErrorKind::PermissionDenied);

        let page =
            render_preview_page(params(&live_id, "conversation"), "owner", "device").unwrap();
        assert_eq!(page.page, 1);
        assert_eq!(page.preview_id, live_id);
        assert!(!page.png.is_empty());

        let expired = render_preview_page(params(&expired_id, "conversation"), "owner", "device")
            .unwrap_err();
        assert_eq!(expired.kind, AgentErrorKind::InvalidInput);
    }
}
