//! Exact text mutation input compilation. Hosts supply authenticated device
//! evidence from their durable receipt store, never model-supplied file refs.
use super::*;
use desk_agent_protocol::computer_use::{
    CreatedFileArtifactOutput, FileContentReadOutput, FilePatchAction, TextFileChange,
};

pub const UPDATE_TEXT_TOOL: &str = "update_text_file";
pub const DELETE_TEXT_TOOL: &str = "delete_text_file";

/// Owner-only recovery projection. This is evidence, not a terminal result or
/// model input, and must never release the unknown-outcome execution barrier.
pub fn unknown_text_recovery_receipt(json: &str) -> Option<String> {
    use desk_agent_protocol::computer_use::{
        ComputerActionCompleted, ComputerActionOutput, ComputerActionResultClass,
    };
    if json.len() > 16_384 {
        return None;
    }
    let completed: ComputerActionCompleted = serde_json::from_str(json).ok()?;
    let Some(ComputerActionOutput::TextFileMutation(output)) = &completed.output else {
        return None;
    };
    if completed.result != ComputerActionResultClass::OutcomeUnknown
        || output.verified
        || output.updated_file.is_some()
        || output.original_file_name.is_empty()
        || output.original_file_name.len() > 512
        || output.original_size_bytes > 65_536
        || output.recovery_path.is_empty()
        || output.recovery_path.len() > 4096
        || output.recovery_path.chars().any(char::is_control)
        || output.original_sha256.len() != 64
        || !output
            .original_sha256
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return None;
    }
    serde_json::to_string(&completed).ok()
}

fn unavailable() -> AgentError {
    error(
        AgentErrorKind::PermissionDenied,
        "text file evidence, exact input, or current conversation directory is unavailable",
        false,
        true,
    )
}

pub fn registered_tools() -> Vec<crate::registry::RegisteredTool> {
    use crate::chat::ToolSpec;
    use crate::registry::{RegisteredTool, ToolEffect};
    [false, true].into_iter().map(|delete| {
        let mut properties = serde_json::json!({
            "directory_request_id": {"type":"string", "minLength":1, "maxLength":256},
            "file_result_call_id": {"type":"string", "minLength":1, "maxLength":256, "description":"Exact tool call id of a verified text-file creation, read, or update result in this conversation."},
            "expected_sha256": {"type":"string", "pattern":"^[0-9a-f]{64}$", "description":"Complete file SHA-256 copied from that device result; never a digest of truncated text."}
        });
        let mut required = vec!["file_result_call_id", "expected_sha256"];
        if !delete {
            properties["change"] = serde_json::json!({"oneOf":[
                {"type":"object","properties":{"kind":{"const":"replace_all"},"content_utf8":{"type":"string","maxLength":65536}},"required":["kind","content_utf8"],"additionalProperties":false},
                {"type":"object","properties":{"kind":{"const":"replace_once"},"before":{"type":"string","minLength":1,"maxLength":65536},"after":{"type":"string","maxLength":65536}},"required":["kind","before","after"],"additionalProperties":false}
            ]});
            required.push("change");
        }
        RegisteredTool {
            spec: ToolSpec {
                name: if delete { DELETE_TEXT_TOOL } else { UPDATE_TEXT_TOOL }.into(),
                description: if delete {
                    "Move exactly one verified text file to a private recovery directory on macOS. Requires a currently approved conversation directory and one exact user confirmation. Never recursive, never permanent deletion. Preserve the recovery receipt; an unknown result must not be retried."
                } else {
                    "Update exactly one verified UTF-8 text file on macOS using full replacement or exactly one unambiguous text match, within 64 KiB. Requires a currently approved conversation directory and one exact user confirmation. The device checks the original identity and full SHA-256, retains recovery material and verifies the new bytes. Conflict means read again and request new approval, never retry automatically."
                }.into(),
                parameters_schema: serde_json::json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
            },
            required_capability: if delete { Capability::FileDeleteConfirmed } else { Capability::FilePatchConfirmed },
            effect: ToolEffect::Mutating,
        }
    }).collect()
}

pub fn is_text_mutation(action: &ComputerActionKind) -> bool {
    matches!(
        action,
        ComputerActionKind::File(
            FilePatchAction::UpdateText { .. } | FilePatchAction::DeleteText { .. }
        )
    )
}

/// Frozen wire consistency only. Hosts must separately re-resolve receipt and
/// directory consent from authoritative session storage before dispatch.
pub fn frozen_resources(
    call: &ToolCall,
    target: &ObjectRef,
    action: &ComputerActionKind,
) -> Result<Vec<String>, AgentError> {
    let ComputerActionKind::File(action) = action else {
        return Err(unavailable());
    };
    let (id, directory) = match (call.name.as_str(), action) {
        (
            UPDATE_TEXT_TOOL,
            FilePatchAction::UpdateText {
                directory,
                expected_sha256,
                change,
            },
        ) => {
            let args: UpdateArgs =
                serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
            if args.expected_sha256 != *expected_sha256 || args.change != *change {
                return Err(unavailable());
            }
            (args.file_result_call_id, directory)
        }
        (
            DELETE_TEXT_TOOL,
            FilePatchAction::DeleteText {
                directory,
                expected_sha256,
            },
        ) => {
            let args: DeleteArgs =
                serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
            if args.expected_sha256 != *expected_sha256 {
                return Err(unavailable());
            }
            (args.file_result_call_id, directory)
        }
        _ => return Err(unavailable()),
    };
    if id.is_empty() || id.len() > 256 || target.object_kind != ObjectKind::File {
        return Err(unavailable());
    }
    action.validate_text_mutation().map_err(|_| unavailable())?;
    Ok(fresh_object_resource_scope(&[
        directory.clone(),
        target.clone(),
    ]))
}

/// Identical receipt interpretation for OSS and Manager. Unknown commits retain
/// recovery metadata but never become a reusable updated-file reference.
pub fn validate_completion(
    target: &ObjectRef,
    action: &ComputerActionKind,
    completed: &desk_agent_protocol::computer_use::ComputerActionCompleted,
) -> Result<(), AgentError> {
    use desk_agent_protocol::computer_use::{
        ComputerActionOutput, ComputerActionResultClass as Class,
    };
    let ComputerActionKind::File(action) = action else {
        return Err(unavailable());
    };
    if !matches!(
        action,
        FilePatchAction::UpdateText { .. } | FilePatchAction::DeleteText { .. }
    ) {
        return Err(unavailable());
    }
    let Some(ComputerActionOutput::TextFileMutation(output)) = &completed.output else {
        return if completed.output.is_none()
            && matches!(
                completed.result,
                Class::DefinitelyNotStarted | Class::Failed | Class::OutcomeUnknown
            )
            && completed
                .facts
                .iter()
                .all(|fact| !fact.changed && !fact.verified)
        {
            Ok(())
        } else {
            Err(unavailable())
        };
    };
    output
        .validate_for(target, action)
        .map_err(|_| unavailable())?;
    if completed.result
        != if output.verified {
            Class::Verified
        } else {
            Class::OutcomeUnknown
        }
        || completed.facts.len() != 1
        || completed.facts[0].index != 0
        || !completed.facts[0].changed
        || completed.facts[0].verified != output.verified
    {
        return Err(unavailable());
    }
    if let (
        FilePatchAction::UpdateText {
            change: TextFileChange::ReplaceAll { content_utf8 },
            ..
        },
        Some(file),
    ) = (action, &output.updated_file)
    {
        if file.size_bytes != content_utf8.len() as u64
            || file.digest_sha256 != format!("{:x}", Sha256::digest(content_utf8.as_bytes()))
        {
            return Err(unavailable());
        }
    }
    Ok(())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateArgs {
    #[serde(default)]
    directory_request_id: Option<String>,
    file_result_call_id: String,
    expected_sha256: String,
    change: TextFileChange,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteArgs {
    #[serde(default)]
    directory_request_id: Option<String>,
    file_result_call_id: String,
    expected_sha256: String,
}

/// Complete, bounded version evidence. Construction validates shape and digest,
/// not provenance: only runtime-authenticated same-conversation receipts may be
/// supplied. This type never issues a capability grant or authorizes read egress.
pub struct VerifiedTextFile {
    result_call_id: String,
    source_envelope_id: Option<String>,
    reference: ObjectRef,
    sha256: String,
}

/// Resolve only device results stored in this authoritative conversation. User
/// prose, checkpoints and model-invented paths/refs cannot become file evidence.
pub fn resolve_file_result(
    session: &crate::session::PersistedAgentSession,
    result_call_id: &str,
    now_unix_ms: u64,
) -> Result<VerifiedTextFile, AgentError> {
    use crate::chat::ChatRole;
    use desk_agent_protocol::computer_use::{
        ComputerActionCompleted, ComputerActionOutput, ComputerActionResultClass,
    };
    let registry = crate::device_assistant::device_assistant_provider_registry();
    let mut source_calls = session
        .conversation
        .iter()
        .filter(|message| message.role == ChatRole::Assistant)
        .flat_map(|message| &message.tool_calls)
        .filter(|call| call.id == result_call_id);
    let source_call = source_calls.next().ok_or_else(unavailable)?;
    if source_calls.next().is_some() {
        return Err(unavailable());
    }
    let supported = source_call.name == "read_selected_text_file"
        || matches!(
            source_call.name.as_str(),
            "create_text_artifact_in_selected_directory"
                | "create_local_communication_draft"
                | UPDATE_TEXT_TOOL
        );
    if !supported {
        return Err(unavailable());
    }
    let descriptor = registry
        .capability_for_tool(&source_call.name)
        .ok_or_else(unavailable)?;
    let provider = registry
        .provider_for_capability(&descriptor.wire.capability_id)
        .ok_or_else(unavailable)?;
    let mut selected: Option<VerifiedTextFile> = None;
    for message in &session.conversation {
        if !matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput)
            || message.tool_call_id.as_deref() != Some(result_call_id)
        {
            continue;
        }
        let Some(envelope) = &message.data_envelope else {
            continue;
        };
        if envelope.validate().is_err()
            || crate::model_egress::envelope_expires_by(envelope, now_unix_ms)
            || envelope.provenance.source_tool_name != source_call.name
            || envelope.provenance.source_provider_id != provider.wire.provider_id
            || envelope.digest_sha256 != format!("{:x}", Sha256::digest(message.text.as_bytes()))
        {
            continue;
        }
        let mut evidence = if source_call.name == "read_selected_text_file" {
            let Ok(desk_agent_protocol::OperationOutput::ReadContext(
                desk_agent_protocol::ReadContextOutput::FileContentRead(output),
            )) = serde_json::from_str(&message.text)
            else {
                continue;
            };
            VerifiedTextFile::from_read(result_call_id, &output)?
        } else {
            let Ok(completion) = serde_json::from_str::<ComputerActionCompleted>(&message.text)
            else {
                continue;
            };
            if completion.result != ComputerActionResultClass::Verified {
                continue;
            }
            match completion.output {
                Some(ComputerActionOutput::FileArtifact(output))
                    if source_call.name != UPDATE_TEXT_TOOL =>
                {
                    VerifiedTextFile::from_artifact(result_call_id, &output)?
                }
                Some(ComputerActionOutput::TextFileMutation(output))
                    if source_call.name == UPDATE_TEXT_TOOL && output.verified =>
                {
                    VerifiedTextFile::from_artifact(
                        result_call_id,
                        output.updated_file.as_ref().ok_or_else(unavailable)?,
                    )?
                }
                _ => continue,
            }
        };
        if selected.as_ref().is_some_and(|existing| {
            existing.reference != evidence.reference || existing.sha256 != evidence.sha256
        }) {
            return Err(unavailable());
        }
        evidence.source_envelope_id = Some(envelope.envelope_id.clone());
        selected = Some(evidence);
    }
    selected.ok_or_else(unavailable)
}

impl VerifiedTextFile {
    pub fn reference(&self) -> &ObjectRef {
        &self.reference
    }
    pub fn source_envelope_id(&self) -> Option<&str> {
        self.source_envelope_id.as_deref()
    }
    pub fn from_read(
        result_call_id: &str,
        output: &FileContentReadOutput,
    ) -> Result<Self, AgentError> {
        if output.byte_len != output.content_utf8.len() as u64
            || output.byte_len > 65_536
            || output.content_utf8.contains('\0')
            || format!("{:x}", Sha256::digest(output.content_utf8.as_bytes())) != output.sha256
        {
            return Err(unavailable());
        }
        Self::new(result_call_id, &output.file, &output.sha256)
    }

    pub fn from_artifact(
        result_call_id: &str,
        output: &CreatedFileArtifactOutput,
    ) -> Result<Self, AgentError> {
        output.validate().map_err(|_| unavailable())?;
        if output.media_type != TEXT_ARTIFACT_MEDIA_TYPE || output.size_bytes > 65_536 {
            return Err(unavailable());
        }
        Self::new(result_call_id, &output.file, &output.digest_sha256)
    }

    fn new(result_call_id: &str, reference: &ObjectRef, sha256: &str) -> Result<Self, AgentError> {
        if result_call_id.is_empty()
            || result_call_id.len() > 256
            || result_call_id.chars().any(char::is_control)
            || reference.object_kind != ObjectKind::File
            || reference.token.is_empty()
            || reference.snapshot_id.is_empty()
        {
            return Err(unavailable());
        }
        Ok(Self {
            result_call_id: result_call_id.into(),
            reference: reference.clone(),
            source_envelope_id: None,
            sha256: sha256.into(),
        })
    }
}

/// Optional selector for a separately authorized read. It is an immutable
/// receipt identity, never a model-supplied path or opaque device token.
pub fn read_result_id(call: &ToolCall) -> Result<Option<String>, AgentError> {
    if call.name != "read_selected_text_file" {
        return Ok(None);
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Args {
        #[serde(default)]
        file_result_call_id: Option<String>,
        #[serde(default)]
        entry_name: Option<String>,
    }
    let args: Args = serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
    if args
        .file_result_call_id
        .as_ref()
        .is_some_and(|id| id.is_empty() || id.len() > 256 || id.chars().any(char::is_control))
    {
        return Err(unavailable());
    }
    if args.entry_name.as_ref().is_some_and(|name| {
        args.file_result_call_id.is_none()
            || name.is_empty()
            || name.len() > 512
            || name
                .chars()
                .any(|c| c.is_control() || c == '/' || c == '\\')
            || matches!(name.as_str(), "." | "..")
    }) {
        return Err(unavailable());
    }
    Ok(args.file_result_call_id)
}

pub fn uses_session_file_read(call: &ToolCall) -> Result<bool, AgentError> {
    if read_result_id(call)?.is_some() {
        return Ok(true);
    }
    if call.name != "inspect_selected_file_metadata" {
        return Ok(false);
    }
    let value: serde_json::Value =
        serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
    Ok(value.get("directory_request_id").is_some())
}

/// Reject unusable file selectors before presenting an owner permission card.
/// This resolves evidence only; grant issuance and dispatch revalidate it again.
pub fn validate_read_permission_input(
    session: &crate::session::PersistedAgentSession,
    tool_name: &str,
    exact_input: Option<&str>,
    now: u64,
) -> Result<(), AgentError> {
    if !matches!(
        tool_name,
        "inspect_selected_file_metadata" | "read_selected_text_file"
    ) {
        return Ok(());
    }
    let invalid = || {
        error(
            AgentErrorKind::InvalidInput,
            "File permission requires a current selected attachment or exact_input naming an approved directory_request_id (metadata), or a real tool call id in file_result_call_id (not snapshot_id) and entry_name for a directory child. Obtain fresh metadata if the source expired.",
            false,
            true,
        )
    };
    let call = ToolCall {
        id: "file-permission-preflight".into(),
        name: tool_name.into(),
        arguments_json: exact_input.unwrap_or("{}").into(),
    };
    if uses_session_file_read(&call).map_err(|_| invalid())? {
        let destinations = crate::permission_resume::latest_user_requirement(&session.conversation)
            .and_then(|m| m.data_envelope.as_ref())
            .map(|envelope| envelope.allowed_destinations.as_slice())
            .ok_or_else(invalid)?;
        let [destination] = destinations else {
            return Err(invalid());
        };
        ResultFileRead::build(session, &call, destination, now).map_err(|_| invalid())?;
    } else if session.context_attachments.is_empty() {
        return Err(invalid());
    }
    Ok(())
}

/// Metadata references permit a separately approved read, never a mutation.
fn read_evidence(
    session: &crate::session::PersistedAgentSession,
    call: &ToolCall,
    now: u64,
) -> Result<VerifiedTextFile, AgentError> {
    let id = read_result_id(call)?.ok_or_else(unavailable)?;
    let args: serde_json::Value =
        serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
    let Some(name) = args.get("entry_name").and_then(|v| v.as_str()) else {
        return resolve_file_result(session, &id, now);
    };
    let mut calls = session
        .conversation
        .iter()
        .filter(|m| m.role == crate::chat::ChatRole::Assistant)
        .flat_map(|m| &m.tool_calls)
        .filter(|c| c.id == id);
    let source_call = calls.next().ok_or_else(unavailable)?;
    if calls.next().is_some() || source_call.name != "inspect_selected_file_metadata" {
        return Err(unavailable());
    }
    let source_call = ToolCall {
        id: source_call.id.clone(),
        name: source_call.name.clone(),
        arguments_json: source_call.arguments_json.clone(),
    };
    let directory = crate::file_scope::select_output_directory(session, &source_call, now)
        .map_err(|_| unavailable())?;
    let mut found = None;
    for message in &session.conversation {
        if !matches!(
            message.role,
            crate::chat::ChatRole::Tool | crate::chat::ChatRole::UntrustedOutput
        ) || message.tool_call_id.as_deref() != Some(&id)
        {
            continue;
        }
        let Some(envelope) = &message.data_envelope else {
            continue;
        };
        if envelope.validate().is_err()
            || crate::model_egress::envelope_expires_by(envelope, now)
            || envelope.provenance.source_tool_name != source_call.name
            || envelope.provenance.source_provider_id
                != crate::device_assistant::FILE_WORKSPACE_PROVIDER_ID
            || envelope.digest_sha256 != format!("{:x}", Sha256::digest(message.text.as_bytes()))
        {
            continue;
        }
        let Ok(desk_agent_protocol::OperationOutput::ReadContext(
            desk_agent_protocol::ReadContextOutput::FileMetadataInspect(output),
        )) = serde_json::from_str(&message.text)
        else {
            continue;
        };
        if output.entries.len() != 1 || output.entries[0].object_ref != directory {
            return Err(unavailable());
        }
        let mut entries = output.directory_entries.iter().filter(|entry| {
            entry.display_name == name
                && !entry.is_directory
                && entry.parent_snapshot_id == directory.snapshot_id
        });
        let entry = entries.next().ok_or_else(unavailable)?;
        if entries.next().is_some() {
            return Err(unavailable());
        }
        let reference = entry.object_ref.as_ref().ok_or_else(unavailable)?;
        let mut evidence = VerifiedTextFile::new(&id, reference, "")?;
        evidence.source_envelope_id = Some(envelope.envelope_id.clone());
        if found
            .as_ref()
            .is_some_and(|previous: &VerifiedTextFile| previous.reference != evidence.reference)
        {
            return Err(unavailable());
        }
        found = Some(evidence);
    }
    found.ok_or_else(unavailable)
}

pub(crate) fn read_source_envelope_id(
    session: &crate::session::PersistedAgentSession,
    call: &ToolCall,
) -> Result<String, AgentError> {
    read_evidence(session, call, 1)?
        .source_envelope_id
        .ok_or_else(unavailable)
}

pub struct ResultFileRead {
    file: ObjectRef,
    pub source_envelope_id: String,
    pub valid_until_unix_ms: u64,
}

impl ResultFileRead {
    pub fn build(
        session: &crate::session::PersistedAgentSession,
        call: &ToolCall,
        destination: &desk_agent_protocol::data_lineage::DestinationIdentity,
        now: u64,
    ) -> Result<Self, AgentError> {
        if call.name == "inspect_selected_file_metadata" {
            let owner = crate::permission_resume::latest_user_requirement(&session.conversation)
                .and_then(|m| m.data_envelope.as_ref())
                .ok_or_else(unavailable)?;
            if !matches!(
                destination,
                desk_agent_protocol::data_lineage::DestinationIdentity::Model { .. }
            ) || owner.allowed_destinations.as_slice() != [destination.clone()]
            {
                return Err(unavailable());
            }
            let file = crate::file_scope::select_output_directory(session, call, now)
                .map_err(|_| unavailable())?;
            let expiry = chrono::DateTime::parse_from_rfc3339(&file.expires_at)
                .ok()
                .and_then(|t| u64::try_from(t.timestamp_millis()).ok())
                .ok_or_else(unavailable)?;
            return Ok(Self {
                file,
                source_envelope_id: owner.envelope_id.clone(),
                valid_until_unix_ms: expiry.min(now.saturating_add(120_000)),
            });
        }
        let evidence = read_evidence(session, call, now)?;
        let source_id = evidence.source_envelope_id().ok_or_else(unavailable)?;
        let source = session
            .conversation
            .iter()
            .filter_map(|m| m.data_envelope.as_ref())
            .find(|e| e.envelope_id == source_id)
            .ok_or_else(unavailable)?;
        let owner = crate::permission_resume::latest_user_requirement(&session.conversation)
            .and_then(|m| m.data_envelope.as_ref())
            .ok_or_else(unavailable)?;
        // Native receipts intentionally have no pre-approved model sink. A
        // fresh owner-bound read grant authorizes re-reading the referenced
        // object; it does not replay or relabel the old receipt's contents.
        if !matches!(
            destination,
            desk_agent_protocol::data_lineage::DestinationIdentity::Model { .. }
        ) || (!source.allowed_destinations.is_empty()
            && !source.allowed_destinations.contains(destination))
            || owner.allowed_destinations.as_slice() != [destination.clone()]
        {
            return Err(unavailable());
        }
        let expiry = chrono::DateTime::parse_from_rfc3339(&evidence.reference.expires_at)
            .ok()
            .and_then(|t| u64::try_from(t.timestamp_millis()).ok())
            .ok_or_else(unavailable)?;
        // This operation obtains new bytes under a new read grant. The old
        // receipt must be valid when resolving its reference (above), but its
        // remaining model-retention window is not the new observation's TTL.
        // Hosts additionally cap this by the current read grant's expiry.
        let valid_until_unix_ms = expiry.min(now.saturating_add(120_000));
        if now == 0 || now >= valid_until_unix_ms {
            return Err(unavailable());
        }
        Ok(Self {
            file: evidence.reference.clone(),
            source_envelope_id: source_id.into(),
            valid_until_unix_ms,
        })
    }
    pub fn resource_scope(&self) -> Vec<String> {
        fresh_object_resource_scope(std::slice::from_ref(&self.file))
    }
    pub fn reference(&self) -> &ObjectRef {
        &self.file
    }
    pub fn bind(&self, input: &mut OperationInput) -> Result<(), AgentError> {
        if let OperationInput::ReadContext(ReadContextInput {
            kind: ContextKind::FileMetadataInspect(params),
        }) = input
        {
            if self.file.object_kind != ObjectKind::Directory {
                return Err(unavailable());
            }
            params.roots = vec![self.file.clone()];
            params.enumerate_directories = true;
            params.max_bytes = params.max_bytes.min(65_536);
            params.max_entries = params.max_entries.min(256);
            return Ok(());
        }
        let OperationInput::ReadContext(ReadContextInput {
            kind: ContextKind::FileContentRead(params),
        }) = input
        else {
            return Err(unavailable());
        };
        params.file = self.file.clone();
        params.max_bytes = params.max_bytes.min(65_536);
        Ok(())
    }
    pub fn validate_output(&self, output: &crate::seam::ToolRunOutput) -> Result<(), AgentError> {
        if output.image_data_url.is_some() {
            return Err(unavailable());
        }
        if self.file.object_kind == ObjectKind::Directory {
            let desk_agent_protocol::OperationOutput::ReadContext(
                desk_agent_protocol::ReadContextOutput::FileMetadataInspect(metadata),
            ) = serde_json::from_str(&output.content).map_err(|_| unavailable())?
            else {
                return Err(unavailable());
            };
            if metadata.entries.len() != 1
                || metadata.entries[0].object_ref != self.file
                || metadata.directory_entries.len() > 256
                || metadata.directory_entries.iter().any(|entry| {
                    entry.parent_snapshot_id != self.file.snapshot_id
                        || entry.object_ref.as_ref().is_some_and(|r| {
                            r.object_kind != ObjectKind::File || entry.is_directory
                        })
                })
            {
                return Err(unavailable());
            }
            return Ok(());
        }
        let desk_agent_protocol::OperationOutput::ReadContext(
            desk_agent_protocol::ReadContextOutput::FileContentRead(read),
        ) = serde_json::from_str(&output.content).map_err(|_| unavailable())?
        else {
            return Err(unavailable());
        };
        if read.file != self.file
            || read.content_utf8.len() > 65_536
            || read.byte_len < read.content_utf8.len() as u64
        {
            return Err(unavailable());
        }
        Ok(())
    }
}

pub struct TextMutationPreflight {
    capability: CapabilityDescriptor,
    provider_id: String,
    surface: ProductSurface,
    operation_scope: Vec<String>,
    target: ObjectRef,
    action: FilePatchAction,
    canonical_input_json: String,
    canonical_input_digest_sha256: String,
    resource_scope: Vec<String>,
    valid_until_unix_ms: u64,
}

impl TextMutationPreflight {
    pub fn from_session(
        session: &crate::session::PersistedAgentSession,
        surface: ProductSurface,
        call: &ToolCall,
        now_unix_ms: u64,
    ) -> Result<Self, AgentError> {
        let args: serde_json::Value =
            serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
        let id = args
            .get("file_result_call_id")
            .and_then(|v| v.as_str())
            .ok_or_else(unavailable)?;
        let evidence = resolve_file_result(session, id, now_unix_ms)?;
        let mut result = Self::build(session, call, &evidence, now_unix_ms)?;
        if !matches!(
            surface,
            ProductSurface::OssPersonalOwner | ProductSurface::ManagerPersonalOwner
        ) || !result.capability.wire.surfaces.contains(&surface)
        {
            return Err(unavailable());
        }
        result.surface = surface;
        Ok(result)
    }

    pub fn required_capability(&self) -> Capability {
        self.capability.required_capability
    }

    pub fn grant_call<'a>(
        &'a self,
        subject: &'a ProviderCallSubject<'_>,
    ) -> Result<CapabilityGrantCall<'a>, AgentError> {
        crate::assistant_policy::require_current_policy(subject.policy_revision)?;
        if subject.readiness_revision == 0
            || subject.now_unix_ms == 0
            || subject.now_unix_ms >= self.valid_until_unix_ms
            || [subject.actor_id, subject.run_id, subject.target_device_id]
                .iter()
                .any(|id| id.trim().is_empty())
        {
            return Err(unavailable());
        }
        Ok(CapabilityGrantCall {
            actor_id: subject.actor_id,
            run_id: subject.run_id,
            input_revision: subject.input_revision,
            surface: self.surface,
            target_device_id: subject.target_device_id,
            target_session_id: None,
            provider_id: &self.provider_id,
            capability_id: &self.capability.wire.capability_id,
            tool_name: &self.capability.wire.tool_name,
            tool_schema_version: self.capability.wire.input_schema_version,
            effect: self.capability.wire.effect,
            risk_tier: CapabilityRiskTier::R3,
            resource_scope: &self.resource_scope,
            operation_scope: &self.operation_scope,
            export_destinations: &[],
            envelope_ids: &[],
            content_digests_sha256: &[],
            canonical_input_digest_sha256: &self.canonical_input_digest_sha256,
            byte_count: self.canonical_input_json.len() as u64,
            item_count: 1,
            policy_revision: subject.policy_revision,
            readiness_revision: subject.readiness_revision,
            now_unix_ms: subject.now_unix_ms,
        })
    }
    pub fn target(&self) -> &ObjectRef {
        &self.target
    }
    pub fn action(&self) -> &FilePatchAction {
        &self.action
    }
    pub fn canonical_input_json(&self) -> &str {
        &self.canonical_input_json
    }
    pub fn canonical_input_digest_sha256(&self) -> &str {
        &self.canonical_input_digest_sha256
    }
    pub fn resource_scope(&self) -> &[String] {
        &self.resource_scope
    }
    pub fn valid_until_unix_ms(&self) -> u64 {
        self.valid_until_unix_ms
    }
    pub fn supports(tool: &str) -> bool {
        matches!(tool, UPDATE_TEXT_TOOL | DELETE_TEXT_TOOL)
    }

    pub fn build(
        session: &crate::session::PersistedAgentSession,
        call: &ToolCall,
        evidence: &VerifiedTextFile,
        now_unix_ms: u64,
    ) -> Result<Self, AgentError> {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let capability = registry
            .capability_for_tool(&call.name)
            .ok_or_else(unavailable)?;
        let provider = registry
            .provider_for_capability(&capability.wire.capability_id)
            .ok_or_else(unavailable)?;
        if call.arguments_json.len() > capability.wire.limits.max_input_bytes as usize
            || now_unix_ms == 0
        {
            return Err(unavailable());
        }
        let directory = crate::file_scope::select_output_directory(session, call, now_unix_ms)
            .map_err(|_| unavailable())?;
        let (result_id, expected_sha256, selector, action) = match call.name.as_str() {
            UPDATE_TEXT_TOOL => {
                let args: UpdateArgs =
                    serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
                let action = FilePatchAction::UpdateText {
                    directory: directory.clone(),
                    expected_sha256: args.expected_sha256.clone(),
                    change: args.change,
                };
                (
                    args.file_result_call_id,
                    args.expected_sha256,
                    args.directory_request_id,
                    action,
                )
            }
            DELETE_TEXT_TOOL => {
                let args: DeleteArgs =
                    serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
                let action = FilePatchAction::DeleteText {
                    directory: directory.clone(),
                    expected_sha256: args.expected_sha256.clone(),
                };
                (
                    args.file_result_call_id,
                    args.expected_sha256,
                    args.directory_request_id,
                    action,
                )
            }
            _ => return Err(unavailable()),
        };
        if result_id != evidence.result_call_id
            || expected_sha256 != evidence.sha256
            || selector
                .as_ref()
                .is_some_and(|id| id.is_empty() || id.len() > 256)
        {
            return Err(unavailable());
        }
        action.validate_text_mutation().map_err(|_| unavailable())?;
        let expiry = |reference: &ObjectRef| {
            chrono::DateTime::parse_from_rfc3339(&reference.expires_at)
                .ok()
                .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
                .filter(|time| *time > now_unix_ms)
                .ok_or_else(unavailable)
        };
        let valid_until_unix_ms = expiry(&directory)?.min(expiry(&evidence.reference)?);
        let canonical_input_json = canonical_tool_permission_input_json(
            &call.name,
            serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?,
        )
        .map_err(|_| unavailable())?;
        Ok(Self {
            capability: capability.clone(),
            provider_id: provider.wire.provider_id.clone(),
            surface: ProductSurface::OssPersonalOwner,
            operation_scope: canonical_compiled_scope(
                &capability.wire.authorization_hint.resources,
                capability.wire.effect,
            )
            .ok_or_else(unavailable)?
            .operations,
            target: evidence.reference.clone(),
            action,
            canonical_input_digest_sha256: format!(
                "{:x}",
                Sha256::digest(canonical_input_json.as_bytes())
            ),
            canonical_input_json,
            resource_scope: fresh_object_resource_scope(&[directory, evidence.reference.clone()]),
            valid_until_unix_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_scope::{DirectoryConsentSource, DirectoryProposal};
    use crate::session::{AgentSessionSurface, PersistedAgentSession};
    use desk_agent_protocol::{AgentScope, ExecutionMode};

    fn fixture() -> (PersistedAgentSession, VerifiedTextFile, ToolCall) {
        let mut session = PersistedAgentSession::new(
            "conversation",
            "owner",
            "device",
            1,
            AgentScope {
                granted: vec![],
                expires_at: None,
                mode: ExecutionMode::ReadOnly,
                policy_name: None,
            },
            "2026-09-05T00:00:00Z",
        );
        session.adopt_client_metadata(Some("client"), AgentSessionSurface::DeviceAssistant);
        session.input_revision = 1;
        let subject = session
            .file_scope_subject("owner", "device", "conversation")
            .unwrap();
        session
            .file_scope
            .propose(
                &subject,
                0,
                DirectoryProposal {
                    request_id: "directory".into(),
                    requested_path: "/private/tmp/text-test".into(),
                    canonical_path: "/private/tmp/text-test".into(),
                    purpose: "requested text changes".into(),
                    source: DirectoryConsentSource::ModelProposal,
                    directory: ObjectRef {
                        token: "root".into(),
                        snapshot_id: "root-snapshot".into(),
                        object_kind: ObjectKind::Directory,
                        expires_at: "2099-01-01T00:00:00Z".into(),
                    },
                },
                1,
            )
            .unwrap();
        session
            .file_scope
            .decide(&subject, 1, "directory", true, 1)
            .unwrap();
        let evidence = VerifiedTextFile::from_read(
            "read-call",
            &FileContentReadOutput {
                file: ObjectRef {
                    token: "file".into(),
                    snapshot_id: "file-snapshot".into(),
                    object_kind: ObjectKind::File,
                    expires_at: "2099-01-01T00:00:00Z".into(),
                },
                display_name: "notes.txt".into(),
                content_utf8: "old".into(),
                byte_len: 3,
                sha256: format!("{:x}", Sha256::digest(b"old")),
            },
        )
        .unwrap();
        let call = ToolCall { id: "update-call".into(), name: UPDATE_TEXT_TOOL.into(), arguments_json: serde_json::json!({
            "directory_request_id": "directory", "file_result_call_id": "read-call", "expected_sha256": evidence.sha256,
            "change": {"kind": "replace_all", "content_utf8": "new"}
        }).to_string() };
        (session, evidence, call)
    }

    #[test]
    fn mutation_binding_includes_directory_file_version_and_exact_content() {
        let (mut session, evidence, mut call) = fixture();
        let plan = TextMutationPreflight::build(&session, &call, &evidence, 1).unwrap();
        assert_eq!(plan.resource_scope().len(), 2);
        assert_eq!(
            ComputerActionKind::File(plan.action().clone()).required_capability(),
            Capability::FilePatchConfirmed
        );
        assert!(session.scope_snapshot.granted.is_empty());
        let original_digest = plan.canonical_input_digest_sha256().to_string();
        let mut input: serde_json::Value = serde_json::from_str(&call.arguments_json).unwrap();
        input["change"]["content_utf8"] = "different".into();
        call.arguments_json = input.to_string();
        assert_ne!(
            TextMutationPreflight::build(&session, &call, &evidence, 1)
                .unwrap()
                .canonical_input_digest_sha256(),
            original_digest
        );
        input["file_result_call_id"] = "other-conversation-call".into();
        call.arguments_json = input.to_string();
        assert!(TextMutationPreflight::build(&session, &call, &evidence, 1).is_err());
        input["file_result_call_id"] = "read-call".into();
        call.arguments_json = input.to_string();
        let subject = session
            .file_scope_subject("owner", "device", "conversation")
            .unwrap();
        session.file_scope.revoke(&subject, 2, "directory").unwrap();
        assert!(TextMutationPreflight::build(&session, &call, &evidence, 1).is_err());
    }

    #[test]
    fn complete_read_evidence_rejects_truncated_or_substituted_content() {
        let mut output = FileContentReadOutput {
            file: ObjectRef {
                token: "file".into(),
                snapshot_id: "snapshot".into(),
                expires_at: "2099-01-01T00:00:00Z".into(),
                object_kind: ObjectKind::File,
            },
            display_name: "notes.txt".into(),
            content_utf8: "完整".into(),
            byte_len: 6,
            sha256: format!("{:x}", Sha256::digest("完整".as_bytes())),
        };
        assert!(VerifiedTextFile::from_read("read-call", &output).is_ok());
        output.byte_len = 100;
        assert!(VerifiedTextFile::from_read("read-call", &output).is_err());
        output.byte_len = 6;
        output.sha256 = "a".repeat(64);
        assert!(VerifiedTextFile::from_read("read-call", &output).is_err());
    }

    #[test]
    fn unknown_receipt_preserves_recovery_without_claiming_success_or_new_file() {
        use desk_agent_protocol::computer_use::{
            ComputerActionCompleted, ComputerActionOutput, ComputerActionResultClass as Class,
            ComputerActionStepFact, TextFileMutationOutput,
        };
        let (session, evidence, call) = fixture();
        let plan = TextMutationPreflight::build(&session, &call, &evidence, 1).unwrap();
        let action = ComputerActionKind::File(plan.action().clone());
        let mut completed = ComputerActionCompleted {
            work_id: "work".into(),
            action_request_id: "action".into(),
            execution_generation: "generation".into(),
            result: Class::OutcomeUnknown,
            facts: vec![ComputerActionStepFact {
                index: 0,
                changed: true,
                verified: false,
                summary: "retained recovery".into(),
            }],
            message: Some("do not retry".into()),
            output: Some(ComputerActionOutput::TextFileMutation(
                TextFileMutationOutput {
                    operation: desk_agent_protocol::computer_use::TextFileMutationOperation::Update,
                    original: evidence.reference.clone(),
                    original_file_name: "notes.txt".into(),
                    original_size_bytes: 3,
                    original_sha256: evidence.sha256.clone(),
                    recovery_path: "/private/tmp/text-test/.assistant-recovery-test".into(),
                    verified: false,
                    updated_file: None,
                },
            )),
        };
        assert!(validate_completion(plan.target(), &action, &completed).is_ok());
        assert!(
            unknown_text_recovery_receipt(&serde_json::to_string(&completed).unwrap()).is_some()
        );
        completed.result = Class::Verified;
        assert!(
            unknown_text_recovery_receipt(&serde_json::to_string(&completed).unwrap()).is_none()
        );
        assert!(validate_completion(plan.target(), &action, &completed).is_err());
        completed.result = Class::OutcomeUnknown;
        let Some(ComputerActionOutput::TextFileMutation(receipt)) = &mut completed.output else {
            panic!()
        };
        receipt.original.token = "different-target".into();
        assert!(validate_completion(plan.target(), &action, &completed).is_err());
    }

    #[test]
    fn history_resolution_accepts_only_same_conversation_device_evidence() {
        use crate::chat::{ChatMessage, ChatRole, ToolCallRef};
        use desk_agent_protocol::data_lineage::{
            ContentRef, DataEnvelope, DataProvenance, RetentionBoundary, Sensitivity,
        };
        let (mut session, evidence, _) = fixture();
        let output = FileContentReadOutput {
            file: evidence.reference,
            display_name: "notes.txt".into(),
            content_utf8: "old".into(),
            byte_len: 3,
            sha256: evidence.sha256,
        };
        let text = serde_json::to_string(&desk_agent_protocol::OperationOutput::ReadContext(
            desk_agent_protocol::ReadContextOutput::FileContentRead(output),
        ))
        .unwrap();
        let hash = format!("{:x}", Sha256::digest(text.as_bytes()));
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let cap = registry
            .capability_for_tool("read_selected_text_file")
            .unwrap();
        let provider = registry
            .provider_for_capability(&cap.wire.capability_id)
            .unwrap();
        let mut parent = ChatMessage::text("proposal", ChatRole::Assistant, "");
        parent.tool_calls.push(ToolCallRef {
            id: "read-call".into(),
            name: "read_selected_text_file".into(),
            arguments_json: "{}".into(),
        });
        let mut result = ChatMessage::tool_result("receipt", "read-call", text.clone());
        result.data_envelope = Some(DataEnvelope {
            schema_version: desk_agent_protocol::data_lineage::DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: "device-result".into(),
            content: ContentRef::ImmutableBlob {
                blob_id: "receipt-blob".into(),
                sha256: hash.clone(),
                size_bytes: text.len() as u64,
                media_type: "application/json".into(),
            },
            provenance: DataProvenance {
                source_provider_id: provider.wire.provider_id.clone(),
                source_tool_name: "read_selected_text_file".into(),
                source_object_id: Some("device:read-call".into()),
                source_envelope_ids: vec![],
            },
            digest_sha256: hash,
            sensitivity: Sensitivity::Sensitive,
            allowed_destinations: vec![],
            retention: RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: true,
            },
        });
        session.conversation = vec![parent, result];
        assert!(resolve_file_result(&session, "read-call", 1).is_ok());
        let destination = desk_agent_protocol::data_lineage::DestinationIdentity::Model {
            connection_id: "test".into(),
            connection_revision: 1,
            model_id: "test".into(),
            profile_revision: 1,
        };
        let call = ToolCall {
            id: "next-read".into(),
            name: "read_selected_text_file".into(),
            arguments_json: serde_json::json!({"file_result_call_id":"read-call"}).to_string(),
        };
        assert!(ResultFileRead::build(&session, &call, &destination, 1).is_err());
        session.conversation[1]
            .data_envelope
            .as_mut()
            .unwrap()
            .allowed_destinations = vec![destination.clone()];
        let mut owner = ChatMessage::text("owner", ChatRole::User, "Read the resulting file");
        owner.data_envelope = session.conversation[1].data_envelope.clone();
        session.conversation.push(owner);
        let read = ResultFileRead::build(&session, &call, &destination, 1).unwrap();
        let (_, mut input) = crate::read_tools::build_read_operation(&call).unwrap();
        read.bind(&mut input).unwrap();
        let OperationInput::ReadContext(ReadContextInput {
            kind: ContextKind::FileContentRead(params),
        }) = input
        else {
            panic!()
        };
        assert_eq!(
            params.file,
            resolve_file_result(&session, "read-call", 1)
                .unwrap()
                .reference
        );
        assert!(session.scope_snapshot.granted.is_empty());
        assert_eq!(read.resource_scope().len(), 1);
        assert_exact_mutation_grants(&session);
        assert_new_mutation_result_window(&session);
        session.conversation.pop();
        session.conversation[1].role = ChatRole::User;
        assert!(resolve_file_result(&session, "read-call", 1).is_err());
        session.conversation[1].role = ChatRole::Tool;
        session.conversation[1].text.push(' ');
        assert!(resolve_file_result(&session, "read-call", 1).is_err());
        session.conversation[1].text = text;
        session.conversation[1]
            .data_envelope
            .as_mut()
            .unwrap()
            .provenance
            .source_provider_id = "web-result".into();
        assert!(resolve_file_result(&session, "read-call", 1).is_err());
        session.conversation.clear();
        assert!(resolve_file_result(&session, "read-call", 1).is_err());
    }

    #[test]
    fn directory_child_read_requires_current_root_exact_receipt_and_separate_egress() {
        use crate::chat::{ChatMessage, ToolCallRef};
        use desk_agent_protocol::computer_use::{
            DirectoryEntryProjection, FileMetadataInspectOutput, FileMetadataProjection,
        };
        use desk_agent_protocol::data_lineage::DestinationIdentity;
        let (mut session, evidence, _) = fixture();
        let destination = DestinationIdentity::Model {
            connection_id: "model".into(),
            connection_revision: 1,
            model_id: "test".into(),
            profile_revision: 1,
        };
        let owner = crate::model_message_labels::model_bound_user_message(
            "owner-input".into(),
            "Read the selected child".into(),
            destination.clone(),
        )
        .unwrap();
        let source = ToolCall {
            id: "metadata".into(),
            name: "inspect_selected_file_metadata".into(),
            arguments_json: serde_json::json!({"directory_request_id":"directory"}).to_string(),
        };
        let root = crate::file_scope::select_output_directory(&session, &source, 1000).unwrap();
        let output = FileMetadataInspectOutput {
            snapshot_id: "metadata-snapshot".into(),
            entries: vec![FileMetadataProjection {
                object_ref: root.clone(),
                display_name: "text-test".into(),
                is_directory: true,
                byte_len: None,
                modified_at: None,
            }],
            directory_entries: vec![DirectoryEntryProjection {
                object_ref: Some(evidence.reference.clone()),
                parent_snapshot_id: root.snapshot_id.clone(),
                display_name: "notes.txt".into(),
                is_directory: false,
                byte_len: Some(3),
                modified_at: None,
            }],
            truncated: false,
        };
        let text = serde_json::to_string(&desk_agent_protocol::OperationOutput::ReadContext(
            desk_agent_protocol::ReadContextOutput::FileMetadataInspect(output),
        ))
        .unwrap();
        let proposal = ChatMessage::assistant_tool_calls(
            "proposal",
            "",
            vec![ToolCallRef {
                id: source.id.clone(),
                name: source.name.clone(),
                arguments_json: source.arguments_json.clone(),
            }],
        );
        let mut receipt = ChatMessage::tool_result("receipt", &source.id, text.clone());
        receipt.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
            owner.data_envelope.as_ref(),
            &source.id,
            &text,
            &source.name,
        )
        .unwrap();
        receipt
            .data_envelope
            .as_mut()
            .unwrap()
            .provenance
            .source_provider_id = crate::device_assistant::FILE_WORKSPACE_PROVIDER_ID.into();
        session.conversation = vec![owner, proposal, receipt];
        let framed = crate::chat::frame_file_tool_result(&session.conversation[2]);
        assert!(framed.starts_with("file_result_call_id: \"metadata\"\n"));
        assert!(framed.ends_with(&text));
        assert_eq!(session.conversation[2].text, text);
        let mut call = ToolCall {
            id: "read-child".into(),
            name: "read_selected_text_file".into(),
            arguments_json:
                serde_json::json!({"file_result_call_id":"metadata","entry_name":"notes.txt"})
                    .to_string(),
        };
        let resolved = ResultFileRead::build(&session, &call, &destination, 1000).unwrap();
        assert_eq!(resolved.reference(), &evidence.reference);
        assert!(
            validate_read_permission_input(&session, &call.name, Some(&call.arguments_json), 1000)
                .is_ok()
        );
        assert!(validate_read_permission_input(&session, &call.name, None, 1000).is_err());
        assert!(
            validate_read_permission_input(
                &session,
                &call.name,
                Some(r#"{"file_result_call_id":"metadata-snapshot","entry_name":"notes.txt"}"#),
                1000
            )
            .is_err()
        );
        assert!(session.scope_snapshot.granted.is_empty());
        // A metadata reference is never a complete version for a destructive operation.
        assert!(resolve_file_result(&session, "metadata", 1000).is_err());
        let original = call.arguments_json.clone();
        for name in ["../notes.txt", "missing.txt", "/notes.txt", "sub/notes.txt"] {
            call.arguments_json =
                serde_json::json!({"file_result_call_id":"metadata","entry_name":name}).to_string();
            assert!(ResultFileRead::build(&session, &call, &destination, 1000).is_err());
        }
        call.arguments_json = original;
        let mut changed = session.clone();
        changed.conversation[2].text.push(' ');
        assert!(ResultFileRead::build(&changed, &call, &destination, 1000).is_err());
        changed = session.clone();
        changed.conversation[2]
            .data_envelope
            .as_mut()
            .unwrap()
            .allowed_destinations
            .clear();
        assert!(ResultFileRead::build(&changed, &call, &destination, 1000).is_ok());
        assert!(changed.scope_snapshot.granted.is_empty());
        changed.conversation[2]
            .data_envelope
            .as_mut()
            .unwrap()
            .retention
            .expires_at_unix_ms = Some(2000);
        let fresh = ResultFileRead::build(&changed, &call, &destination, 1000).unwrap();
        assert_eq!(fresh.valid_until_unix_ms, 121_000);
        // Expired evidence still cannot be used to start another read.
        assert!(ResultFileRead::build(&changed, &call, &destination, 2000).is_err());
        changed.conversation[2]
            .data_envelope
            .as_mut()
            .unwrap()
            .allowed_destinations = vec![DestinationIdentity::Model {
            connection_id: "another-model".into(),
            connection_revision: 1,
            model_id: "other".into(),
            profile_revision: 1,
        }];
        assert!(ResultFileRead::build(&changed, &call, &destination, 1000).is_err());
        let subject = session
            .file_scope_subject("owner", "device", "conversation")
            .unwrap();
        session.file_scope.revoke(&subject, 2, "directory").unwrap();
        assert!(ResultFileRead::build(&session, &call, &destination, 1000).is_err());
    }

    fn assert_new_mutation_result_window(original: &PersistedAgentSession) {
        use crate::chat::{ChatMessage, ToolCallRef};
        let mut session = original.clone();
        session.latest_input_seq = 1;
        session
            .begin_turn(
                "mutation-turn",
                None,
                None,
                1,
                session.scope_snapshot.clone(),
                "now",
            )
            .unwrap();
        session.conversation[1]
            .data_envelope
            .as_mut()
            .unwrap()
            .retention
            .expires_at_unix_ms = Some(2000);
        let call = fixture().2;
        let mut proposal = ChatMessage::assistant_tool_calls(
            "mutation-proposal",
            "Modify the file",
            vec![ToolCallRef {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments_json: call.arguments_json.clone(),
            }],
        );
        proposal.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
            session.conversation.last().unwrap().data_envelope.as_ref(),
            &call.id,
            &proposal.text,
            "test_model_output",
        )
        .unwrap();
        proposal
            .data_envelope
            .as_mut()
            .unwrap()
            .retention
            .expires_at_unix_ms = Some(7000);
        proposal.turn_id = Some("mutation-turn".into());
        session.conversation.push(proposal);
        let origin = crate::action_result::ActionResultOrigin::capture(
            &crate::device_assistant::device_assistant_provider_registry(),
            &session,
            &call,
        )
        .unwrap();
        assert_eq!(origin.retention.expires_at_unix_ms, Some(7000));
        assert_eq!(
            session.conversation[1]
                .data_envelope
                .as_ref()
                .unwrap()
                .retention
                .expires_at_unix_ms,
            Some(2000)
        );
        assert!(
            origin
                .source_envelope_ids
                .iter()
                .any(|id| id == "device-result")
        );
        let action = crate::session::ActionIdentity::new(
            1,
            "action",
            "generation",
            crate::session::WorkKind::CapabilityProvider,
        );
        let output = crate::seam::ToolRunOutput {
            content: "New device completion metadata".into(),
            image_data_url: None,
        };
        let receipt = origin.receipt(action.clone(), 1, 3000, &output).unwrap();
        receipt
            .validate_for(&origin, action.clone(), 1, &output)
            .unwrap();
        assert!(receipt.envelope.allowed_destinations.is_empty());
        assert_eq!(receipt.envelope.retention.expires_at_unix_ms, Some(7000));
        assert_eq!(
            origin
                .receipt(action, 1, 6000, &output)
                .unwrap()
                .envelope
                .retention
                .expires_at_unix_ms,
            Some(7000)
        );
    }

    fn assert_exact_mutation_grants(session: &crate::session::PersistedAgentSession) {
        use crate::capability_availability::CapabilityAvailability;
        use crate::dynamic_run::{PermissionDecisionItem, PermissionItemDecision};
        use crate::permission_grant::{PermissionGrantIssuanceContext, build_permission_grants};
        let registry = crate::device_assistant::device_assistant_provider_registry();
        for tool in [UPDATE_TEXT_TOOL, DELETE_TEXT_TOOL] {
            let mut call = fixture().2;
            call.name = tool.into();
            if tool == DELETE_TEXT_TOOL {
                let mut value: serde_json::Value =
                    serde_json::from_str(&call.arguments_json).unwrap();
                value.as_object_mut().unwrap().remove("change");
                call.arguments_json = value.to_string();
            }
            let capability = registry.capability_for_tool(tool).unwrap();
            let request_call = ToolCall { id: "permission".into(), name: crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(), arguments_json: serde_json::json!({"items":[{
                "item_id":"mutation", "provider_id":crate::device_assistant::TEXT_FILE_PROVIDER_ID,
                "tool_name":tool,"expected_effect":"write_artifact", "exact_input":serde_json::from_str::<serde_json::Value>(&call.arguments_json).unwrap(),
                "suggested_ttl_seconds":60,"suggested_max_uses":50,"reason":"Perform the requested exact text change"
            }]}).to_string() };
            let request = crate::permission_tools::build_permission_request(
                &request_call,
                &registry,
                "permission".into(),
                session.input_revision,
                "2026-09-05T00:00:00Z".into(),
            )
            .unwrap();
            assert_eq!(request.items[0].suggested_max_uses, 1);
            let decisions = vec![PermissionDecisionItem {
                item_id: "mutation".into(),
                decision: PermissionItemDecision::Approve {
                    resource_scope: request.items[0].resource_scope.clone(),
                    operation_scope: request.items[0].operation_scope.clone(),
                    export_destinations: vec![],
                    ttl_seconds: 60,
                    max_uses: 1,
                },
            }];
            let inventory = vec![CapabilityAvailability {
                provider_id: crate::device_assistant::TEXT_FILE_PROVIDER_ID.into(),
                capability_id: capability.wire.capability_id.clone(),
                tool_name: tool.into(),
                compiled: true,
                enabled: true,
                connected: true,
                ready: true,
                reason: None,
            }];
            for surface in [
                ProductSurface::OssPersonalOwner,
                ProductSurface::ManagerPersonalOwner,
            ] {
                let context = PermissionGrantIssuanceContext {
                    surface,
                    registry: &registry,
                    inventory: &inventory,
                    readiness_revision: 1,
                    now_unix_ms: 1000,
                    implicit_fresh_object_refs: &[],
                };
                let grants =
                    build_permission_grants(session, &request, &decisions, &context, None).unwrap();
                let input =
                    TextMutationPreflight::from_session(session, surface, &call, 1000).unwrap();
                let subject = ProviderCallSubject {
                    actor_id: &session.actor_id,
                    run_id: &session.conversation_id,
                    input_revision: session.input_revision,
                    target_device_id: &session.device_id,
                    policy_revision: session.policy_revision,
                    readiness_revision: 1,
                    now_unix_ms: 1000,
                };
                let authority = input.grant_call(&subject).unwrap();
                assert_eq!(grants[0].risk_tier, CapabilityRiskTier::R3);
                assert_eq!(
                    grants[0].use_policy,
                    desk_agent_protocol::capability_grant::CapabilityGrantUsePolicy::OneShotExact
                );
                assert_eq!(grants[0].resource_scope.len(), 2);
                crate::capability_grant::match_capability_grant(&grants[0], &authority).unwrap();
                let mut wrong = grants[0].clone();
                wrong.tool_name = "create_text_artifact_in_selected_directory".into();
                assert!(
                    crate::capability_grant::match_capability_grant(&wrong, &authority).is_err()
                );
                let mut revoked = session.clone();
                let subject = revoked
                    .file_scope_subject(
                        &revoked.actor_id,
                        &revoked.device_id,
                        &revoked.conversation_id,
                    )
                    .unwrap();
                revoked
                    .file_scope
                    .revoke(&subject, revoked.file_scope.revision(), "directory")
                    .unwrap();
                assert!(
                    build_permission_grants(&revoked, &request, &decisions, &context, None)
                        .is_err()
                );
            }
        }
    }
}
