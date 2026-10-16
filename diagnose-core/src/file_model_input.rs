//! Model file selections and deterministic defaults; never an authorization.

use crate::{
    chat::{ToolCall, ToolCallRef, ToolSpec},
    session::PersistedAgentSession,
};
use desk_agent_protocol::{
    AgentError, AgentErrorKind, OperationOutput, ReadContextOutput,
    computer_use::{FileMetadataInspectOutput, ObjectKind, ObjectRef},
};
use serde_json::{Value, json};

pub(crate) fn is_read(tool: &str) -> bool {
    matches!(tool, "inspect_files" | "read_text_file")
        || (crate::provider_preflight::text_file::selection::supports(tool)
            && tool != "convert_document")
}

fn invalid(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

pub(crate) fn prepare_permissions(value: &mut Value) {
    if let Some(items) = value["items"].as_array_mut() {
        for item in items {
            if item["tool_name"].as_str().is_some_and(is_read)
                && let Some(item) = item.as_object_mut()
            {
                item.entry("exact_input").or_insert_with(|| json!({}));
            }
        }
    }
}

fn metadata(value: &Value) -> Option<FileMetadataInspectOutput> {
    match serde_json::from_value::<OperationOutput>(value.clone()).ok()? {
        OperationOutput::ReadContext(ReadContextOutput::FileMetadataInspect(output)) => {
            Some(output)
        }
        _ => None,
    }
}

/// Sources are already authenticated by result_model_ids before reaching here.
pub(crate) fn restore_sources(
    tool: &str,
    value: &mut Value,
    results: &[(ToolCallRef, Value)],
) -> Result<(), AgentError> {
    fn complete(
        source: &mut Value,
        results: &[(ToolCallRef, Value)],
        typed: bool,
    ) -> Result<(), AgentError> {
        if typed && source.get("kind").is_some() {
            return Err(invalid(
                "File source accepts file_result_call_id and optional entry_name only. The server restores source metadata; omit internal fields. No request was created.",
            ));
        }
        let id = source["file_result_call_id"].as_str().ok_or_else(|| {
            invalid(
                "Choose file_result_call_id from a verified file result; no request was created.",
            )
        })?;
        let mut found = results.iter().filter(|(call, _)| call.id == id);
        let (call, result) = found.next().ok_or_else(|| invalid("The selected file result is unavailable or expired. Read a current source and use its returned ID; no request was created."))?;
        if found.any(|(other_call, other_result)| other_call != call || other_result != result) {
            return Err(invalid(
                "The file result is ambiguous. Select an unambiguous current result; no request was created.",
            ));
        }
        if call.name == "inspect_files" {
            let output = metadata(result).ok_or_else(|| {
                invalid("The selected result has no verified directory metadata.")
            })?;
            if source.get("entry_name").is_none() {
                let entries: Vec<_> = output
                    .directory_entries
                    .iter()
                    .filter(|entry| {
                        !entry.is_directory
                            && entry
                                .object_ref
                                .as_ref()
                                .is_some_and(|reference| reference.object_kind == ObjectKind::File)
                            && output.entries.len() == 1
                            && entry.parent_snapshot_id == output.entries[0].object_ref.snapshot_id
                    })
                    .collect();
                if output.truncated
                    || output.pagination_id.is_some()
                    || output.next_cursor.is_some()
                    || entries.len() != 1
                {
                    return Err(invalid(
                        "entry_name is required: select an exact regular-file name from this directory result. Only one readable file in a complete, unpaged result can be selected automatically; no request was created.",
                    ));
                }
                source["entry_name"] = json!(entries[0].display_name);
            }
            if typed {
                source["kind"] = json!("directory_entry");
            }
        } else {
            if source.get("entry_name").is_some() {
                return Err(invalid(
                    "This result identifies one file. Remove entry_name and reuse file_result_call_id; no request was created.",
                ));
            }
            if typed {
                source["kind"] = json!("artifact_result");
            }
        }
        Ok(())
    }
    if tool == "read_text_file" && value.get("file_result_call_id").is_some() {
        complete(value, results, false)?;
    }
    if !crate::provider_preflight::text_file::selection::supports(tool) {
        return Ok(());
    }
    if let Some(source) = value.get_mut("source") {
        complete(source, results, true)?;
    }
    if let Some(sources) = value.get_mut("sources").and_then(Value::as_array_mut) {
        for source in sources {
            complete(source, results, true)?;
        }
    }
    Ok(())
}

pub(crate) fn project_arguments(tool: &str, value: &mut Value) {
    if !crate::provider_preflight::text_file::selection::supports(tool) {
        return;
    }
    if let Some(source) = value.get_mut("source").and_then(Value::as_object_mut) {
        source.remove("kind");
    }
    if let Some(sources) = value.get_mut("sources").and_then(Value::as_array_mut) {
        for source in sources {
            if let Some(source) = source.as_object_mut() {
                source.remove("kind");
            }
        }
    }
}

pub(crate) fn project_tool(tool: &mut ToolSpec) {
    if crate::provider_preflight::text_file::selection::supports(&tool.name) {
        let source = json!({"type":"object","additionalProperties":false,"properties":{
            "file_result_call_id":{"type":"string","minLength":1,"maxLength":32,"description":"Short result ID from a verified file or directory result in this conversation."},
            "entry_name":{"type":"string","minLength":1,"maxLength":512,"description":"Exact regular-file name when selecting among multiple directory children. Omit for a one-file result; a complete unpaged directory result with exactly one readable file needs no name."}
        },"required":["file_result_call_id"]});
        let multiple = crate::provider_preflight::text_file::selection::multiple(&tool.name);
        let field = if multiple { "sources" } else { "source" };
        tool.parameters_schema["properties"][field] = if multiple {
            json!({"type":"array","minItems":1,"maxItems":8,"items":source})
        } else {
            source
        };
        if tool.name != "convert_document"
            && let Some(required) = tool.parameters_schema["required"].as_array_mut()
        {
            required.retain(|key| key != field);
        }
        if let Some(offset) = tool
            .description
            .find(" Select each file from a recorded result")
        {
            tool.description.truncate(offset);
        }
        if !tool
            .description
            .contains("Select recorded files with source")
        {
            tool.description.push_str(" Select recorded files with source (sources for multi-file operations), using file_result_call_id and entry_name only when a directory result needs a child choice. The server restores source metadata. Never provide native paths or device tokens.");
            if tool.name != "convert_document" {
                tool.description.push_str(" Source may be omitted only when the current accepted user input already selects the required file or file set. Available attachments or a recent history result are not implicit selections. Keep any required cell, worksheet or merge rules. Request contextual_exact_input permission; the server completes defaults before approval.");
            }
        }
    }
    if tool.name == "inspect_files" {
        tool.description = tool.description.replace("by supplying directory_request_id for an approved conversation directory and request separate metadata permission with these exact arguments", "from a selected or uniquely approved conversation directory; request contextual_exact_input metadata permission");
        if !tool
            .description
            .contains("directory_request_id may be omitted")
        {
            tool.description.push_str(" directory_request_id may be omitted for exactly one currently approved conversation directory, or when using the immutable object selection of the current user input. Multiple unbound directories require an explicit selection.");
        }
    }
    if tool.name == "read_text_file" {
        tool.description = tool.description.replace("For a child from inspect_files, provide file_result_call_id plus entry_name.", "For a child from inspect_files, provide file_result_call_id and choose entry_name when needed.").replace("Request read_text_file permission with these exact arguments", "Request contextual_exact_input read_text_file permission with the necessary business choices");
        if !tool
            .description
            .contains("A complete unpaged metadata result")
        {
            tool.description.push_str(" A complete unpaged metadata result with exactly one readable file needs no entry_name. file_result_call_id may be omitted only for the single file explicitly selected in the current accepted user input, never for an arbitrary recent result.");
        }
    }
}

pub(crate) fn directory_id(
    session: &PersistedAgentSession,
    reference: &ObjectRef,
    now: u64,
) -> Result<String, AgentError> {
    let subject = session
        .file_scope_subject(
            &session.actor_id,
            &session.device_id,
            &session.conversation_id,
        )
        .map_err(|_| invalid("The conversation directory is unavailable."))?;
    let mut ids = session.file_scope.records().iter().filter_map(|record| {
        session
            .file_scope
            .approved_directory(
                &subject,
                session.file_scope.revision(),
                &record.proposal.request_id,
                now,
            )
            .ok()
            .filter(|root| *root == reference)
            .map(|_| record.proposal.request_id.clone())
    });
    let id = ids.next().ok_or_else(|| invalid("The original conversation directory was revoked, changed or expired. Obtain directory approval again; no request was created."))?;
    Ok(id)
}

fn source_directory(
    session: &PersistedAgentSession,
    value: &Value,
    now: u64,
) -> Result<Option<ObjectRef>, AgentError> {
    let Some(id) = value["file_result_call_id"].as_str().or_else(|| {
        value
            .pointer("/source/file_result_call_id")
            .and_then(Value::as_str)
    }) else {
        return Ok(None);
    };
    let results = crate::result_model_ids::verified_results(&session.conversation, now);
    let mut id = id.to_string();
    let mut visited = std::collections::BTreeSet::new();
    while visited.insert(id.clone()) {
        let (call, result) = results.iter().find(|(call, _)| call.id == id).ok_or_else(|| invalid("The source directory cannot be recovered from current verified evidence. Select directory_request_id explicitly; no request was created."))?;
        if call.name == "inspect_files" {
            let output = metadata(result)
                .ok_or_else(|| invalid("The source has no verified directory metadata."))?;
            let [root] = output.entries.as_slice() else {
                return Err(invalid(
                    "The source directory is ambiguous. Select directory_request_id explicitly.",
                ));
            };
            return (root.is_directory && root.object_ref.object_kind == ObjectKind::Directory)
                .then(|| Some(root.object_ref.clone()))
                .ok_or_else(|| invalid("The source root is not a directory."));
        }
        let source = ToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments_json: call.arguments_json.clone(),
        };
        let value: Value = serde_json::from_str(&source.arguments_json)
            .map_err(|_| invalid("The source input is unavailable."))?;
        if let Some(id) = value.get("directory_request_id").and_then(Value::as_str) {
            let root = session.file_scope.records().iter().find(|record| record.proposal.request_id == id).ok_or_else(|| invalid("The source directory record is unavailable. Select a currently approved directory explicitly."))?;
            return Ok(Some(root.proposal.directory.clone()));
        }
        let resolved = crate::ui_model_ids::resolve_call(&source, &session.conversation, now)?;
        let value: Value = serde_json::from_str(&resolved.arguments_json)
            .map_err(|_| invalid("The source input is unavailable."))?;
        let Some(next) = value["file_result_call_id"].as_str().or_else(|| {
            value
                .pointer("/source/file_result_call_id")
                .and_then(Value::as_str)
        }) else {
            return Ok(None);
        };
        id = next.to_string();
    }
    Err(invalid(
        "The file source chain is cyclic; choose a current verified source.",
    ))
}

/// Freeze directory choices before either human or AI approval sees the input.
pub(crate) fn complete_session_input(
    tool: &str,
    value: &mut Value,
    session: &PersistedAgentSession,
    original: Option<&crate::input_read_context::ReadContextSelection>,
    now: u64,
) -> Result<(), AgentError> {
    if session.surface != crate::session::AgentSessionSurface::AiAssistant {
        return Ok(());
    }
    if tool == "request_permissions" {
        if let Some(items) = value["items"].as_array_mut() {
            for item in items {
                let Some(tool) = item["tool_name"].as_str().map(str::to_owned) else {
                    continue;
                };
                if tool == "request_permissions" {
                    return Err(invalid(
                        "Permission items must identify a Provider tool, not request_permissions; no request was created.",
                    ));
                }
                if let Some(exact) = item.get_mut("exact_input") {
                    complete_session_input(&tool, exact, session, original, now)?;
                }
            }
        }
        return Ok(());
    }
    if is_read(tool)
        || crate::provider_preflight::ArtifactCallPreflight::supports(tool)
        || crate::provider_preflight::text_file::TextMutationPreflight::supports(tool)
    {
        crate::model_input::validate_format(tool, value).map_err(invalid)?;
    }
    let directory_tool = tool == "inspect_files"
        || crate::provider_preflight::ArtifactCallPreflight::supports(tool)
        || crate::provider_preflight::text_file::TextMutationPreflight::supports(tool);
    if !directory_tool {
        return Ok(());
    }
    if tool == "inspect_files"
        && value.get("directory_request_id").is_none()
        && original.is_some_and(|selection| {
            selection.tool_names.iter().any(|name| name == tool)
                && selection.object_attachments.iter().any(|object| {
                    matches!(
                        object.kind,
                        crate::context_attachment::ContextAttachmentKind::File
                            | crate::context_attachment::ContextAttachmentKind::DirectorySelection
                    )
                })
        })
    {
        return Ok(());
    }
    let call = ToolCall {
        id: "complete-file-directory".into(),
        name: tool.into(),
        arguments_json: value.to_string(),
    };
    let root = if value.get("directory_request_id").is_none() {
        source_directory(session, value, now)?.map(Ok).unwrap_or_else(|| crate::file_scope::select_output_directory(session, &call, now).map_err(|_| invalid("directory_request_id is required when no single approved conversation directory is bound. Select an approved directory; no request was created.")))?
    } else {
        crate::file_scope::select_output_directory(session, &call, now).map_err(|_| invalid("The explicit directory_request_id is unavailable, expired or revoked. Obtain a current directory approval; no request was created."))?
    };
    if value.get("directory_request_id").is_none() {
        value["directory_request_id"] = json!(directory_id(session, &root, now)?);
    }
    Ok(())
}

/// Filled fields are compared after model projection; explicit choices stay exact.
pub(crate) fn complete_comparison(tool: &str, original: &mut Value, resolved: &Value) {
    if tool == "request_permissions" {
        if let (Some(left), Some(right)) = (
            original["items"].as_array_mut(),
            resolved["items"].as_array(),
        ) {
            for (left, right) in left.iter_mut().zip(right) {
                if let Some(tool) = left["tool_name"].as_str().map(str::to_owned) {
                    if is_read(&tool) && left.get("exact_input").is_none() {
                        left["exact_input"] = json!({});
                        crate::model_input::fill_versions(&tool, &mut left["exact_input"]);
                    }
                    if let Some(exact) = left.get_mut("exact_input") {
                        complete_comparison(&tool, exact, &right["exact_input"]);
                    }
                }
            }
        }
        return;
    }
    let directory_tool = tool == "inspect_files"
        || crate::provider_preflight::ArtifactCallPreflight::supports(tool)
        || crate::provider_preflight::text_file::TextMutationPreflight::supports(tool);
    if directory_tool
        && original.get("directory_request_id").is_none()
        && let Some(id) = resolved.get("directory_request_id")
    {
        original["directory_request_id"] = id.clone();
    }
    if tool == "read_text_file"
        && original.get("file_result_call_id").is_some()
        && original.get("entry_name").is_none()
        && let Some(name) = resolved.get("entry_name")
    {
        original["entry_name"] = name.clone();
    }
    for field in ["source", "sources"] {
        if !crate::provider_preflight::text_file::selection::supports(tool) {
            break;
        }
        fn entry(left: &mut Value, right: &Value) {
            if left.is_object()
                && left.get("entry_name").is_none()
                && let Some(name) = right.get("entry_name")
            {
                left["entry_name"] = name.clone();
            }
        }
        if field == "source" {
            if let Some(left) = original.get_mut(field) {
                entry(left, &resolved[field]);
            }
        } else if let (Some(left), Some(right)) = (
            original.get_mut(field).and_then(Value::as_array_mut),
            resolved.get(field).and_then(Value::as_array),
        ) {
            for (left, right) in left.iter_mut().zip(right) {
                entry(left, right);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
