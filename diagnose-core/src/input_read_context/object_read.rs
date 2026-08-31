//! Exact object operations, bounds and lineage shared by both central hosts.

use super::ReadContextSelection;
use crate::{
    chat::ToolCall,
    context_attachment::{ContextAttachment, ContextAttachmentKind},
    read_tools::build_read_operation,
    seam::ToolRunOutput,
};
use desk_agent_protocol::{
    AgentError, AgentErrorKind, ContextKind, OperationInput, ReadContextInput,
    computer_use::{ObjectKind, ObjectRef},
    data_lineage::{ContentRef, DataEnvelope, DestinationIdentity, RetentionBoundary},
};

/// Pure projection of an original selection validated by the owning store.
/// Runtimes must separately recheck durable input, subject and current objects
/// around I/O; this value neither grants authority nor owns a transaction.
pub struct ObjectReadBinding<'a> {
    pub original: &'a ReadContextSelection,
    pub destination: &'a DestinationIdentity,
    pub now_unix_ms: u64,
}

pub fn requires_objects(name: &str) -> bool {
    matches!(
        name,
        "inspect_selected_file_metadata"
            | "read_selected_text_file"
            | "inspect_selected_spreadsheets"
            | "preview_spreadsheet_merge"
            | "inspect_selected_terminal_output"
            | "inspect_selected_numbers_with_iwork"
            | "inspect_selected_pages_with_iwork"
            | "inspect_selected_keynote_with_iwork"
    )
}

/// Object selection enables only reads; live-document and batch tools still
/// require their separate explicit capability selection.
pub fn implicit_object_tool(name: &str, objects: &[ContextAttachment]) -> bool {
    match name {
        "inspect_selected_terminal_output" => objects
            .iter()
            .any(|object| object.kind == ContextAttachmentKind::TerminalSessionRef),
        "inspect_selected_file_metadata"
        | "read_selected_text_file"
        | "inspect_selected_spreadsheets"
        | "preview_spreadsheet_merge" => objects.iter().any(|object| {
            matches!(
                object.kind,
                ContextAttachmentKind::File | ContextAttachmentKind::DirectorySelection
            )
        }),
        _ => false,
    }
}

impl ObjectReadBinding<'_> {
    fn selected(&self, call: &ToolCall) -> Result<Vec<&ContextAttachment>, AgentError> {
        if !self.original.tool_names.contains(&call.name) {
            return Err(denied());
        }
        let terminal = call.name == "inspect_selected_terminal_output";
        let mut selected: Vec<_> = self
            .original
            .object_attachments
            .iter()
            .filter(|object| {
                if terminal {
                    object.kind == ContextAttachmentKind::TerminalSessionRef
                } else {
                    matches!(
                        object.kind,
                        ContextAttachmentKind::File | ContextAttachmentKind::DirectorySelection
                    )
                }
            })
            .collect();
        if matches!(
            call.name.as_str(),
            "read_selected_text_file"
                | "inspect_selected_numbers_with_iwork"
                | "inspect_selected_pages_with_iwork"
                | "inspect_selected_keynote_with_iwork"
        ) {
            selected.retain(|object| object.kind == ContextAttachmentKind::File);
            if selected.len() != 1 {
                return Err(denied());
            }
        }
        if selected.is_empty() {
            return Err(denied());
        }
        for object in &selected {
            if !object.is_active_at(self.now_unix_ms)
                || object.envelope.allowed_destinations.as_slice() != [(*self.destination).clone()]
            {
                return Err(denied());
            }
        }
        Ok(selected)
    }

    pub fn bind(&self, call: &ToolCall, input: &mut OperationInput) -> Result<(), AgentError> {
        let selected = self.selected(call)?;
        let refs = selected
            .iter()
            .map(|object| {
                serde_json::from_str::<ObjectRef>(&object.object_ref.opaque_token)
                    .map_err(|_| denied())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let bytes = selected
            .iter()
            .map(|object| object.bounds.max_bytes as u32)
            .min()
            .ok_or_else(denied)?;
        let objects = selected
            .iter()
            .map(|object| object.bounds.max_objects)
            .min()
            .ok_or_else(denied)?;
        if refs.len() > objects as usize {
            return Err(denied());
        }
        let OperationInput::ReadContext(ReadContextInput { kind }) = input else {
            return Err(denied());
        };
        match (call.name.as_str(), kind) {
            ("inspect_selected_file_metadata", ContextKind::FileMetadataInspect(params)) => {
                params.roots = refs;
                params.max_bytes = params.max_bytes.min(bytes);
                params.max_entries = params.max_entries.min(objects);
                params.enumerate_directories = true;
            }
            ("read_selected_text_file", ContextKind::FileContentRead(params)) => {
                params.file = refs[0].clone();
                params.max_bytes = params.max_bytes.min(bytes);
            }
            ("inspect_selected_spreadsheets", ContextKind::SpreadsheetFileInspect(params)) => {
                params.files = refs;
                params.max_bytes = params.max_bytes.min(bytes);
                params.max_workbooks = params.max_workbooks.min(objects);
            }
            ("preview_spreadsheet_merge", ContextKind::SpreadsheetMergePreview(params)) => {
                params.files = refs;
                params.max_bytes = params.max_bytes.min(bytes);
            }
            ("inspect_selected_terminal_output", ContextKind::TerminalOutputInspect(params)) => {
                params.roots = refs;
                params.max_bytes = params.max_bytes.min(bytes);
            }
            (
                "inspect_selected_numbers_with_iwork",
                ContextKind::SpreadsheetLiveInspect(params),
            )
            | ("inspect_selected_pages_with_iwork", ContextKind::DocumentLiveInspect(params))
            | (
                "inspect_selected_keynote_with_iwork",
                ContextKind::PresentationLiveInspect(params),
            ) => {
                if refs[0].object_kind != ObjectKind::File {
                    return Err(denied());
                }
                params.target = None;
                params.batch_file = Some(refs[0].clone());
                params.max_bytes = params.max_bytes.min(bytes);
            }
            _ => return Err(denied()),
        }
        Ok(())
    }

    pub fn expiry(&self, call: &ToolCall) -> Result<u64, AgentError> {
        let expiry = self
            .selected(call)?
            .iter()
            .map(|object| {
                object.expires_at_unix_ms.min(
                    object
                        .envelope
                        .retention
                        .expires_at_unix_ms
                        .unwrap_or(u64::MAX),
                )
            })
            .min()
            .ok_or_else(denied)?;
        let expiry = if let Some(scope_expiry) = &self.original.expires_at {
            expiry.min(
                u64::try_from(
                    chrono::DateTime::parse_from_rfc3339(scope_expiry)
                        .map_err(|_| denied())?
                        .timestamp_millis(),
                )
                .map_err(|_| denied())?,
            )
        } else {
            expiry
        };
        if expiry <= self.now_unix_ms {
            return Err(denied());
        }
        Ok(expiry)
    }

    pub fn label(
        &self,
        call: &ToolCall,
        output: &ToolRunOutput,
        mut envelope: DataEnvelope,
    ) -> Result<DataEnvelope, AgentError> {
        let selected = self.selected(call)?;
        let (_, mut input) = build_read_operation(call)?;
        self.bind(call, &mut input)?;
        let OperationInput::ReadContext(ReadContextInput { kind }) = input else {
            return Err(denied());
        };
        let max_bytes = u64::from(match kind {
            ContextKind::FileMetadataInspect(params) => params.max_bytes,
            ContextKind::FileContentRead(params) => params.max_bytes,
            ContextKind::SpreadsheetFileInspect(params) => params.max_bytes,
            ContextKind::SpreadsheetMergePreview(params) => params.max_bytes,
            ContextKind::TerminalOutputInspect(params) => params.max_bytes,
            ContextKind::SpreadsheetLiveInspect(params)
            | ContextKind::DocumentLiveInspect(params)
            | ContextKind::PresentationLiveInspect(params) => params.max_bytes,
            _ => return Err(denied()),
        });
        if output.image_data_url.is_some() || output.content.len() as u64 > max_bytes {
            return Err(denied());
        }
        let expiry = self.expiry(call)?;
        envelope.provenance.source_object_id = None;
        envelope.provenance.source_envelope_ids = selected
            .iter()
            .map(|object| object.envelope.envelope_id.clone())
            .collect();
        envelope.allowed_destinations = vec![(*self.destination).clone()];
        envelope.retention = envelope.retention.most_restrictive(RetentionBoundary {
            expires_at_unix_ms: Some(expiry),
            delete_with_run: false,
        });
        for object in selected {
            envelope.sensitivity = envelope.sensitivity.max(object.envelope.sensitivity);
            envelope.retention = envelope
                .retention
                .most_restrictive(object.envelope.retention);
        }
        if let ContentRef::EphemeralObservation {
            expires_at_unix_ms, ..
        } = &mut envelope.content
        {
            *expires_at_unix_ms = (*expires_at_unix_ms).min(expiry);
        }
        envelope.validate().map_err(|_| denied())?;
        Ok(envelope)
    }
}

fn denied() -> AgentError {
    AgentError {
        kind: AgentErrorKind::PermissionDenied,
        message: "original object selection is unavailable for this read".into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}
