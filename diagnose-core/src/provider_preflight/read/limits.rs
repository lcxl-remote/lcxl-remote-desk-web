//! Output limits use wire bytes and projected objects, not input JSON bytes.

use super::*;
use crate::seam::ToolRunOutput;
use desk_agent_protocol::{
    ContextKind, OperationOutput, ReadContextOutput, capability_grant::CapabilityGrantLimits,
    computer_use::OfficeSelectionProjection,
};

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
    if !requires_objects(&call.name) {
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
