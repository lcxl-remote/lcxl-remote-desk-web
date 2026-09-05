//! Output limits use wire bytes and projected objects, not input JSON bytes.

use super::*;
use crate::seam::ToolRunOutput;
use desk_agent_protocol::{
    ContextKind, OperationOutput, ReadContextOutput, capability_grant::CapabilityGrantLimits,
    computer_use::OfficeSelectionProjection,
};
use serde::Deserialize;

#[cfg(test)]
mod tests;

pub fn descriptor_limits(capability: &CapabilityDescriptor) -> CapabilityGrantLimits {
    CapabilityGrantLimits {
        max_bytes_per_call: capability.wire.limits.max_output_bytes,
        max_items_per_call: capability.wire.limits.max_objects,
        max_calls: 1,
    }
}

fn effective(
    registry: &ProviderRegistry,
    call: &ToolCall,
    grant: &CapabilityGrantLimits,
) -> Result<CapabilityGrantLimits, AgentError> {
    let descriptor = descriptor_limits(
        registry
            .capability_for_tool(&call.name)
            .ok_or_else(unavailable)?,
    );
    if grant.max_bytes_per_call == 0 || grant.max_items_per_call == 0 || grant.max_calls == 0 {
        return Err(unavailable());
    }
    Ok(CapabilityGrantLimits {
        max_bytes_per_call: grant.max_bytes_per_call.min(descriptor.max_bytes_per_call),
        max_items_per_call: grant.max_items_per_call.min(descriptor.max_items_per_call),
        max_calls: grant.max_calls,
    })
}

/// Apply after binding original references. Never drop selected roots silently.
pub fn bind(
    registry: &ProviderRegistry,
    call: &ToolCall,
    input: &mut OperationInput,
    grant: &CapabilityGrantLimits,
) -> Result<(), AgentError> {
    let limits = effective(registry, call, grant)?;
    let bytes = limits.max_bytes_per_call.min(u32::MAX as u64) as u32;
    let items = limits.max_items_per_call;
    let OperationInput::ReadContext(input) = input else {
        return Err(unavailable());
    };
    let roots = match &mut input.kind {
        ContextKind::OfficeDocumentInspect(p) => {
            p.max_bytes = p.max_bytes.min(bytes);
            p.max_objects = p.max_objects.min(items);
            1
        }
        ContextKind::DesktopUiInspect(p) => {
            p.max_bytes = p.max_bytes.min(bytes);
            p.max_nodes = p.max_nodes.min(items);
            1
        }
        ContextKind::SpreadsheetLiveInspect(p)
        | ContextKind::DocumentLiveInspect(p)
        | ContextKind::PresentationLiveInspect(p) => {
            p.max_bytes = p.max_bytes.min(bytes);
            1
        }
        ContextKind::FileMetadataInspect(p) => {
            p.max_bytes = p.max_bytes.min(bytes);
            p.max_entries = p.max_entries.min(items);
            p.roots.len()
        }
        ContextKind::FileContentRead(p) => {
            p.max_bytes = p.max_bytes.min(bytes);
            1
        }
        ContextKind::SpreadsheetFileInspect(p) => {
            p.max_bytes = p.max_bytes.min(bytes);
            p.max_workbooks = p.max_workbooks.min(items);
            p.files.len()
        }
        ContextKind::SpreadsheetMergePreview(p) => {
            p.max_bytes = p.max_bytes.min(bytes);
            p.files.len()
        }
        ContextKind::TerminalOutputInspect(p) => {
            p.max_bytes = p.max_bytes.min(bytes);
            p.roots.len()
        }
        _ => 1,
    };
    if roots > items as usize {
        return Err(unavailable());
    }
    Ok(())
}

/// Validate before shared retention and again before model use. Image transport
/// bytes are conservatively counted including the data-URL/base64 overhead.
pub fn validate_output(
    registry: &ProviderRegistry,
    call: &ToolCall,
    output: &ToolRunOutput,
    grant: &CapabilityGrantLimits,
) -> Result<(), AgentError> {
    let limits = effective(registry, call, grant)?;
    let bytes = output
        .content
        .len()
        .checked_add(output.image_data_url.as_ref().map_or(0, String::len))
        .ok_or_else(unavailable)?;
    if bytes as u64 > limits.max_bytes_per_call {
        return Err(unavailable());
    }
    if matches!(
        call.name.as_str(),
        crate::web_research::WEB_FETCH_TOOL_NAME | crate::web_research::WEB_SEARCH_TOOL_NAME
    ) {
        return validate_web_output(registry, call, output, &limits);
    }
    if !requires_objects(&call.name) && call.name != "inspect_desktop_ui" {
        return Ok(());
    }
    if output.image_data_url.is_some() {
        return Err(unavailable());
    }
    let typed: OperationOutput =
        serde_json::from_str(&output.content).map_err(|_| unavailable())?;
    let OperationOutput::ReadContext(typed) = typed else {
        return Err(unavailable());
    };
    let (_, OperationInput::ReadContext(expected)) = build_read_operation(call)? else {
        return Err(unavailable());
    };
    let items = match (expected.kind, typed) {
        (ContextKind::DesktopUiInspect(_), ReadContextOutput::DesktopUiInspect(p)) => p.nodes.len(),
        (ContextKind::OfficeDocumentInspect(_), ReadContextOutput::OfficeDocumentInspect(p)) => {
            match p.selection {
                OfficeSelectionProjection::Excel { cells, .. } => cells.len(),
                OfficeSelectionProjection::PowerPoint { slides, shapes, .. } => {
                    slides.len().saturating_add(shapes.len())
                }
            }
        }
        (ContextKind::FileMetadataInspect(_), ReadContextOutput::FileMetadataInspect(p)) => {
            p.entries.len().saturating_add(p.directory_entries.len())
        }
        (ContextKind::SpreadsheetFileInspect(_), ReadContextOutput::SpreadsheetFileInspect(p)) => {
            p.workbooks.len()
        }
        (
            ContextKind::SpreadsheetMergePreview(_),
            ReadContextOutput::SpreadsheetMergePreview(p),
        ) => p.input_digests_sha256.len(),
        (ContextKind::TerminalOutputInspect(_), ReadContextOutput::TerminalOutputInspect(p)) => {
            p.entries.len()
        }
        (ContextKind::FileContentRead(_), ReadContextOutput::FileContentRead(_))
        | (ContextKind::SpreadsheetLiveInspect(_), ReadContextOutput::SpreadsheetLiveInspect(_))
        | (ContextKind::DocumentLiveInspect(_), ReadContextOutput::DocumentLiveInspect(_))
        | (
            ContextKind::PresentationLiveInspect(_),
            ReadContextOutput::PresentationLiveInspect(_),
        ) => 1,
        _ => return Err(unavailable()),
    };
    if items > limits.max_items_per_call as usize {
        return Err(unavailable());
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchOutput {
    schema_version: u32,
    untrusted_external_content: bool,
    requested_url: String,
    final_url: String,
    title: Option<String>,
    published_at: Option<String>,
    fetched_at: String,
    content_type: String,
    body_bytes: usize,
    sha256: String,
    excerpt: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchOutput {
    schema_version: u32,
    configuration_revision: u64,
    web_search_call_id: String,
    untrusted_external_content: bool,
    connector: SearchConnector,
    query_sha256: String,
    searched_at: String,
    response_sha256: String,
    response_bytes: usize,
    result_count: usize,
    results: Vec<SearchResult>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchConnector {
    connector_id: String,
    display_name: String,
    requires_api_key: bool,
    experimental: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
    published_at: Option<String>,
}

fn validate_web_output(
    registry: &ProviderRegistry,
    call: &ToolCall,
    output: &ToolRunOutput,
    limits: &CapabilityGrantLimits,
) -> Result<(), AgentError> {
    if output.image_data_url.is_some() {
        return Err(unavailable());
    }
    match call.name.as_str() {
        crate::web_research::WEB_FETCH_TOOL_NAME => {
            let expected = crate::web_research::validate_fetch_arguments(call)?;
            let value: FetchOutput =
                serde_json::from_str(&output.content).map_err(|_| unavailable())?;
            let final_url = crate::web_research::validate_public_url(&value.final_url, false)?;
            if value.schema_version != 1
                || !value.untrusted_external_content
                || value.requested_url != expected.initial_url().as_str()
                || !expected.same_approved_origin(&final_url)
                || value
                    .title
                    .as_ref()
                    .is_some_and(|title| title.chars().count() > 2_000)
                || value
                    .published_at
                    .as_ref()
                    .is_some_and(|date| date.len() > 256 || date.chars().any(char::is_control))
                || value.fetched_at.len() > 64
                || !matches!(
                    value.content_type.as_str(),
                    "text/html" | "text/plain" | "application/xhtml+xml"
                )
                || value.body_bytes > 128 * 1024
                || !is_sha256(&value.sha256)
                || value.excerpt.chars().count() > 24_000
                || limits.max_items_per_call < 1
            {
                return Err(unavailable());
            }
        }
        crate::web_research::WEB_SEARCH_TOOL_NAME => {
            let expected = crate::web_research::validate_search_arguments(call)?;
            let value: SearchOutput =
                serde_json::from_str(&output.content).map_err(|_| unavailable())?;
            let expected_query_sha256 =
                format!("{:x}", sha2::Sha256::digest(expected.query().as_bytes()));
            if value.schema_version != 1
                || value.web_search_call_id != call.id
                || !value.untrusted_external_content
                || crate::web_research::search_connector_metadata(&value.connector.connector_id)
                    != Some((
                        value.connector.display_name.as_str(),
                        value.connector.requires_api_key,
                    ))
                || value.connector.experimental
                || value.query_sha256 != expected_query_sha256
                || value.searched_at.len() > 64
                || !is_sha256(&value.response_sha256)
                || value.response_bytes > 256 * 1024
                || value.result_count != value.results.len()
                || value.results.len() > usize::from(expected.max_results())
                || value.results.len() > limits.max_items_per_call as usize
            {
                return Err(unavailable());
            }
            if registry.web_search_binding().is_some_and(|binding| {
                binding.connector_id != value.connector.connector_id
                    || binding.revision != value.configuration_revision
            }) {
                return Err(unavailable());
            }
            for item in value.results {
                if item.title.trim().is_empty()
                    || item.title.chars().count() > 512
                    || item.url.len() > 2_048
                    || item.snippet.chars().count() > 2_000
                    || item
                        .published_at
                        .as_ref()
                        .is_some_and(|date| date.len() > 128 || date.chars().any(char::is_control))
                    || crate::web_research::validate_public_url(&item.url, false).is_err()
                {
                    return Err(unavailable());
                }
            }
        }
        _ => return Err(unavailable()),
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
