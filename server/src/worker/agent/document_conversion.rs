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
use desk_document_conversion::{self as engine, sandbox};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::file_reference_store::{self, publication};

const PREVIEW_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_PREVIEWS_PER_CONVERSATION: usize = 2;
/// Previews keep only their source; every page is compiled and rendered in the
/// sandbox on demand. These bound what the worker retains across all previews.
const MAX_PREVIEW_ENTRIES: usize = 8;
const MAX_PREVIEW_SOURCE_BYTES_TOTAL: usize = 16 * 1024 * 1024;
/// Sandbox work one preview may spend over its lifetime.
const MAX_RENDERS_PER_PREVIEW: u32 = 20;
const MAX_RENDER_TIME_PER_PREVIEW: Duration = Duration::from_secs(120);

/// Runs one Typst request out of process. Production uses
/// [`crate::typst_sandbox::run`]; tests substitute an in-memory runner.
pub(crate) type SandboxRunner = dyn Fn(
        &sandbox::SandboxRequest,
        &[u8],
    ) -> Result<(sandbox::SandboxResponse, Vec<u8>), crate::typst_sandbox::SandboxFailure>
    + Sync;

fn sandbox_runner() -> &'static SandboxRunner {
    &crate::typst_sandbox::run
}

#[derive(Clone)]
struct PreviewEntry {
    actor_id: String,
    device_id: String,
    conversation_id: String,
    created_at: Instant,
    last_accessed_at: Instant,
    format: engine::SourceFormat,
    source: Arc<[u8]>,
    page_count: u32,
    renders: u32,
    render_time: Duration,
}

#[derive(Default)]
struct PreviewStore {
    entries: HashMap<String, PreviewEntry>,
}

impl PreviewStore {
    fn prune(&mut self, now: Instant) {
        self.entries
            .retain(|_, entry| now.duration_since(entry.created_at) < PREVIEW_TTL);
    }

    fn source_bytes(&self) -> usize {
        self.entries.values().map(|entry| entry.source.len()).sum()
    }

    /// Evicts least recently used entries matching `filter` while `over`
    /// holds, stopping when nothing matching is left.
    fn evict_while(
        &mut self,
        filter: impl Fn(&PreviewEntry) -> bool,
        over: impl Fn(&Self) -> bool,
    ) {
        while over(self) {
            let oldest = self
                .entries
                .iter()
                .filter(|(_, entry)| filter(entry))
                .min_by_key(|(_, entry)| entry.last_accessed_at)
                .map(|(id, _)| id.clone());
            let Some(oldest) = oldest else { break };
            self.entries.remove(&oldest);
        }
    }
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

struct RenderedPreviewPage {
    page_count: u32,
    template_version: String,
    font_set_sha256: String,
    engine: String,
    warnings: Vec<engine::ConversionWarning>,
    png: Vec<u8>,
    width: u32,
    height: u32,
    pixels_per_point_milli: u32,
}

fn render_in_sandbox(
    runner: &SandboxRunner,
    source: &[u8],
    format: engine::SourceFormat,
    page: u32,
) -> Result<RenderedPreviewPage, AgentError> {
    let (response, png) = runner(
        &sandbox::SandboxRequest::RenderPage { format, page },
        source,
    )
    .map_err(|failure| engine_error(failure.into_conversion_error()))?;
    match response {
        sandbox::SandboxResponse::RenderedPage {
            page_count,
            template_version,
            font_set_sha256,
            engine,
            warnings,
            width,
            height,
            pixels_per_point_milli,
        } if png.starts_with(b"\x89PNG\r\n\x1a\n") => Ok(RenderedPreviewPage {
            page_count,
            template_version,
            font_set_sha256,
            engine,
            warnings,
            png,
            width,
            height,
            pixels_per_point_milli,
        }),
        sandbox::SandboxResponse::Failed { code, message } => {
            Err(engine_error(sandbox::error_from_wire(&code, message)))
        }
        _ => Err(engine_error(
            crate::typst_sandbox::SandboxFailure::Protocol("unexpected preview response".into())
                .into_conversion_error(),
        )),
    }
}

pub fn create_preview(
    params: DocumentPreviewParams,
    actor_id: String,
    device_id: String,
) -> Result<DocumentPreviewOutput, AgentError> {
    create_preview_with(params, actor_id, device_id, sandbox_runner())
}

fn create_preview_with(
    params: DocumentPreviewParams,
    actor_id: String,
    device_id: String,
    runner: &SandboxRunner,
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
    let started = Instant::now();
    let first_page = render_in_sandbox(runner, &source.bytes, format, 1)?;
    let elapsed = started.elapsed();
    if first_page.png.len() as u64 > MAX_DOCUMENT_PREVIEW_BYTES {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "document preview page exceeds the image limit",
        ));
    }
    let preview_id = uuid::Uuid::new_v4().to_string();
    let now = Instant::now();
    let output = DocumentPreviewOutput {
        preview_id: preview_id.clone(),
        source_digest_sha256: source.sha256.clone(),
        page_count: first_page.page_count,
        template_version: first_page.template_version,
        font_set_sha256: first_page.font_set_sha256,
        engine: first_page.engine,
        warnings: first_page.warnings.into_iter().map(map_warning).collect(),
        first_page: Some(DocumentPreviewPageOutput {
            preview_id: preview_id.clone(),
            page: 1,
            page_count: first_page.page_count,
            png: first_page.png,
            width: first_page.width,
            height: first_page.height,
            pixels_per_point_milli: first_page.pixels_per_point_milli,
        }),
    };
    let source: Arc<[u8]> = source.bytes.into();
    let mut guard = store().lock().map_err(|_| {
        error(
            AgentErrorKind::Internal,
            "document preview store is unavailable",
        )
    })?;
    guard.prune(now);
    let same_owner = |entry: &PreviewEntry| {
        entry.actor_id == actor_id
            && entry.device_id == device_id
            && entry.conversation_id == params.conversation_id
    };
    guard.evict_while(same_owner, |store| {
        store
            .entries
            .values()
            .filter(|entry| same_owner(entry))
            .count()
            >= MAX_PREVIEWS_PER_CONVERSATION
    });
    let incoming = source.len();
    guard.evict_while(
        |_| true,
        |store| {
            store.entries.len() >= MAX_PREVIEW_ENTRIES
                || store.source_bytes() + incoming > MAX_PREVIEW_SOURCE_BYTES_TOTAL
        },
    );
    guard.entries.insert(
        preview_id,
        PreviewEntry {
            actor_id,
            device_id,
            conversation_id: params.conversation_id,
            created_at: now,
            last_accessed_at: now,
            format,
            source,
            page_count: output.page_count,
            renders: 1,
            render_time: elapsed,
        },
    );
    Ok(output)
}

pub fn render_preview_page(
    params: DocumentPreviewPageParams,
    actor_id: &str,
    device_id: &str,
) -> Result<DocumentPreviewPageOutput, AgentError> {
    render_preview_page_with(params, actor_id, device_id, sandbox_runner())
}

fn render_preview_page_with(
    params: DocumentPreviewPageParams,
    actor_id: &str,
    device_id: &str,
    runner: &SandboxRunner,
) -> Result<DocumentPreviewPageOutput, AgentError> {
    if params.spec_version
        != desk_agent_protocol::document_conversion::DOCUMENT_PREVIEW_SPEC_VERSION
    {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "unsupported document preview page specification version",
        ));
    }
    // Copy what the render needs and release the lock: a render may take
    // seconds and must not block other previews or their eviction.
    let (source, format) = {
        let now = Instant::now();
        let mut guard = store().lock().map_err(|_| {
            error(
                AgentErrorKind::Internal,
                "document preview store is unavailable",
            )
        })?;
        guard.prune(now);
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
        if params.page == 0 || params.page > entry.page_count {
            return Err(error(
                AgentErrorKind::InvalidInput,
                "preview page is outside the document",
            ));
        }
        if entry.renders >= MAX_RENDERS_PER_PREVIEW
            || entry.render_time >= MAX_RENDER_TIME_PER_PREVIEW
        {
            return Err(error(
                AgentErrorKind::OutputLimitExceeded,
                "this preview has used its page rendering budget; create a new preview",
            ));
        }
        // Count the attempt before rendering, so concurrent requests cannot
        // overrun the budget.
        entry.renders += 1;
        entry.last_accessed_at = now;
        (Arc::clone(&entry.source), entry.format)
    };
    let started = Instant::now();
    let rendered = render_in_sandbox(runner, &source, format, params.page);
    let elapsed = started.elapsed();
    if let Ok(mut guard) = store().lock()
        && let Some(entry) = guard.entries.get_mut(&params.preview_id)
    {
        entry.render_time = entry.render_time.saturating_add(elapsed);
    }
    let rendered = rendered?;
    if rendered.png.len() as u64 > MAX_DOCUMENT_PREVIEW_BYTES {
        return Err(error(
            AgentErrorKind::OutputLimitExceeded,
            "rendered preview exceeds the 400 KB page limit",
        ));
    }
    Ok(DocumentPreviewPageOutput {
        preview_id: params.preview_id,
        page: params.page,
        page_count: rendered.page_count,
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
    let converted = match convert_source(&source.bytes, &options, sandbox_runner()) {
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
        Some("document converted and published with create-new semantics".into()),
        Some(ComputerActionOutput::DocumentArtifact(output)),
    )
}

/// Typst-backed conversions run in the resource-limited sandbox. PDF text
/// extraction has no evaluation step and stays in process.
fn convert_source(
    source: &[u8],
    options: &engine::ConvertOptions,
    runner: &SandboxRunner,
) -> Result<engine::ConversionOutput, engine::ConversionError> {
    match options.kind {
        engine::ConversionKind::PdfToMarkdown | engine::ConversionKind::PdfToText => {
            engine::convert(source, options)
        }
        engine::ConversionKind::MarkdownToPdf
        | engine::ConversionKind::TextToPdf
        | engine::ConversionKind::TypstToPdf => {
            if !options.pages.is_empty() || options.page_markers.is_some() {
                return Err(engine::ConversionError::new(
                    "invalid_conversion_options",
                    "page ranges and page markers are only valid for PDF extraction",
                ));
            }
            let (response, bytes) = runner(
                &sandbox::SandboxRequest::Convert { kind: options.kind },
                source,
            )
            .map_err(crate::typst_sandbox::SandboxFailure::into_conversion_error)?;
            match response {
                sandbox::SandboxResponse::Converted { details, warnings }
                    if bytes.starts_with(b"%PDF-") && bytes.len() <= engine::MAX_OUTPUT_BYTES =>
                {
                    Ok(engine::ConversionOutput {
                        bytes,
                        media_type: options.kind.media_type(),
                        details,
                        warnings,
                    })
                }
                sandbox::SandboxResponse::Failed { code, message } => {
                    Err(sandbox::error_from_wire(&code, message))
                }
                _ => Err(crate::typst_sandbox::SandboxFailure::Protocol(
                    "unexpected conversion response".into(),
                )
                .into_conversion_error()),
            }
        }
    }
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
        DocumentConversionKind::TypstToPdf => lower.ends_with(".typ"),
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
            DocumentConversionKind::TypstToPdf => engine::ConversionKind::TypstToPdf,
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
            "source_too_large"
            | "output_too_large"
            | "page_limit_exceeded"
            | "document_resource_limit_exceeded" => AgentErrorKind::OutputLimitExceeded,
            "document_conversion_timeout" => AgentErrorKind::Timeout,
            "document_sandbox_unavailable" => AgentErrorKind::Internal,
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

    #[test]
    fn typst_source_requires_typ_extension() {
        assert!(
            validate_source_kind("report.TYP", b"Hello", DocumentConversionKind::TypstToPdf)
                .is_ok()
        );
        assert!(
            validate_source_kind("report.txt", b"Hello", DocumentConversionKind::TypstToPdf)
                .is_err()
        );
    }

    fn params(preview_id: &str, conversation_id: &str) -> DocumentPreviewPageParams {
        DocumentPreviewPageParams {
            preview_id: preview_id.into(),
            conversation_id: conversation_id.into(),
            page: 1,
            spec_version: DOCUMENT_PREVIEW_SPEC_VERSION,
        }
    }

    fn in_process(
        request: &sandbox::SandboxRequest,
        source: &[u8],
    ) -> Result<(sandbox::SandboxResponse, Vec<u8>), crate::typst_sandbox::SandboxFailure> {
        let mut input = Vec::new();
        sandbox::write_frame(&mut input, request, source).unwrap();
        let mut output = Vec::new();
        sandbox::serve(&mut input.as_slice(), &mut output)
            .map_err(|error| crate::typst_sandbox::SandboxFailure::Protocol(error.to_string()))?;
        sandbox::read_frame(&mut output.as_slice())
            .map_err(|error| crate::typst_sandbox::SandboxFailure::Protocol(error.to_string()))
    }

    fn entry(conversation: &str, created_at: Instant, source: &[u8]) -> PreviewEntry {
        PreviewEntry {
            actor_id: "owner".into(),
            device_id: "device".into(),
            conversation_id: conversation.into(),
            created_at,
            last_accessed_at: created_at,
            format: engine::SourceFormat::Markdown,
            source: source.into(),
            page_count: 1,
            renders: 1,
            render_time: Duration::ZERO,
        }
    }

    #[test]
    fn preview_pages_are_ephemeral_and_bound_to_actor_device_and_conversation() {
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
                entry("conversation", now, b"# Preview\n\nBound content"),
            );
            guard.entries.insert(
                expired_id.clone(),
                entry("conversation", expired_at, b"# Expired"),
            );
        }
        let render = |id: &str, conversation: &str, actor: &str, device: &str| {
            render_preview_page_with(params(id, conversation), actor, device, &in_process)
        };

        let wrong_actor = render(&live_id, "conversation", "other", "device").unwrap_err();
        assert_eq!(wrong_actor.kind, AgentErrorKind::PermissionDenied);
        let wrong_device = render(&live_id, "conversation", "owner", "other").unwrap_err();
        assert_eq!(wrong_device.kind, AgentErrorKind::PermissionDenied);
        let wrong_conversation = render(&live_id, "other", "owner", "device").unwrap_err();
        assert_eq!(wrong_conversation.kind, AgentErrorKind::PermissionDenied);

        let page = render(&live_id, "conversation", "owner", "device").unwrap();
        assert_eq!(page.page, 1);
        assert_eq!(page.preview_id, live_id);
        assert!(page.png.starts_with(b"\x89PNG"));

        let expired = render(&expired_id, "conversation", "owner", "device").unwrap_err();
        assert_eq!(expired.kind, AgentErrorKind::InvalidInput);
        store().lock().unwrap().entries.remove(&live_id);
    }

    #[test]
    fn a_preview_stops_rendering_after_its_budget() {
        let id = uuid::Uuid::new_v4().to_string();
        let mut spent = entry("budget", Instant::now(), b"# Budget");
        spent.renders = MAX_RENDERS_PER_PREVIEW;
        store().lock().unwrap().entries.insert(id.clone(), spent);
        let error = render_preview_page_with(params(&id, "budget"), "owner", "device", &in_process)
            .unwrap_err();
        assert_eq!(error.kind, AgentErrorKind::OutputLimitExceeded);
        let mut slow = entry("budget", Instant::now(), b"# Budget");
        slow.render_time = MAX_RENDER_TIME_PER_PREVIEW;
        store().lock().unwrap().entries.insert(id.clone(), slow);
        assert!(
            render_preview_page_with(params(&id, "budget"), "owner", "device", &in_process)
                .is_err()
        );
        store().lock().unwrap().entries.remove(&id);
    }

    #[test]
    fn a_sandbox_failure_is_a_definite_non_publication() {
        let failing = |_: &sandbox::SandboxRequest, _: &[u8]| {
            Err(crate::typst_sandbox::SandboxFailure::ResourceLimit)
        };
        let options = engine::ConvertOptions {
            kind: engine::ConversionKind::TypstToPdf,
            pages: vec![],
            page_markers: None,
        };
        let error = convert_source(b"#let x = 1", &options, &failing).unwrap_err();
        assert_eq!(error.code, "document_resource_limit_exceeded");
        assert_eq!(
            engine_error(error).kind,
            AgentErrorKind::OutputLimitExceeded
        );
        let converted = convert_source(b"Hello", &options, &in_process).unwrap();
        assert!(converted.bytes.starts_with(b"%PDF-"));
        // PDF extraction never reaches the sandbox runner.
        let extraction = engine::ConvertOptions {
            kind: engine::ConversionKind::PdfToText,
            pages: vec![],
            page_markers: None,
        };
        assert_ne!(
            convert_source(b"not a pdf", &extraction, &failing)
                .unwrap_err()
                .code,
            "document_resource_limit_exceeded"
        );
    }

    #[test]
    fn preview_store_evicts_least_recently_used_sources_within_global_bounds() {
        let now = Instant::now();
        let mut store = PreviewStore::default();
        for index in 0..MAX_PREVIEW_ENTRIES {
            let mut item = entry(
                &format!("c{index}"),
                now,
                &vec![0_u8; MAX_PREVIEW_SOURCE_BYTES_TOTAL / MAX_PREVIEW_ENTRIES],
            );
            item.last_accessed_at = now + Duration::from_millis(index as u64);
            store.entries.insert(format!("p{index}"), item);
        }
        let incoming = 1024;
        store.evict_while(
            |_| true,
            |store| {
                store.entries.len() >= MAX_PREVIEW_ENTRIES
                    || store.source_bytes() + incoming > MAX_PREVIEW_SOURCE_BYTES_TOTAL
            },
        );
        assert_eq!(store.entries.len(), MAX_PREVIEW_ENTRIES - 1);
        assert!(!store.entries.contains_key("p0"));
        assert!(store.source_bytes() + incoming <= MAX_PREVIEW_SOURCE_BYTES_TOTAL);
    }
}
