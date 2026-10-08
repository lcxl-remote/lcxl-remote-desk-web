//! Model-only references to exact document observations and approved directories.

use crate::{
    chat::{ChatMessage, ToolCall, ToolSpec},
    session::PersistedAgentSession,
};
use desk_agent_protocol::{AgentError, AgentErrorKind, computer_use::ObjectRef};
use serde_json::{Value, json};

fn invalid(detail: &str) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: format!(
            "Invalid document reference: {detail}. Obtain the ID from the matching read result; no action was executed."
        ),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

fn field(tool: &str) -> Option<(&str, &str)> {
    match tool {
        "inspect_live_spreadsheet"
        | "inspect_live_document"
        | "inspect_live_presentation"
        | "patch_live_spreadsheet_cell"
        | "replace_live_document_body"
        | "patch_live_presentation_slide"
        | "patch_numbers_copy"
        | "replace_pages_copy_body"
        | "patch_keynote_copy"
        | "patch_excel_copy"
        | "replace_word_copy_body"
        | "patch_powerpoint_copy" => Some(("target", "target_id")),
        "inspect_office_selection" => Some(("document", "document_id")),
        _ => None,
    }
}

pub(crate) fn supports(tool: &str) -> bool {
    field(tool).is_some() || tool == "preview_computer_action"
}

pub(crate) fn copy_tool(tool: &str) -> bool {
    matches!(
        tool,
        "patch_numbers_copy"
            | "replace_pages_copy_body"
            | "patch_keynote_copy"
            | "patch_excel_copy"
            | "replace_word_copy_body"
            | "patch_powerpoint_copy"
    )
}

fn object_id(reference: &ObjectRef) -> String {
    crate::result_model_ids::alias(
        "object",
        &serde_json::to_string(reference).expect("object identity serialization"),
    )
}

fn references(value: &Value, found: &mut Vec<ObjectRef>) {
    if let Ok(reference) = serde_json::from_value::<ObjectRef>(value.clone()) {
        found.push(reference);
    } else {
        match value {
            Value::Object(object) => {
                for child in object.values() {
                    references(child, found);
                }
            }
            Value::Array(array) => {
                for child in array {
                    references(child, found);
                }
            }
            _ => {}
        }
    }
}

fn resolve_id(id: &str, history: &[ChatMessage], now: u64) -> Result<ObjectRef, AgentError> {
    let mut selected = None;
    for (call, mut value) in crate::result_model_ids::verified_results(history, now) {
        if !(call.name.starts_with("inspect_") || call.name == "read_current_screen") {
            continue;
        }
        crate::ui_model_output::expand_value(&mut value);
        let mut observed = Vec::new();
        references(&value, &mut observed);
        for reference in observed.into_iter().filter(|reference| {
            object_id(reference) == id
                || (matches!(
                    call.name.as_str(),
                    "inspect_desktop_ui" | "inspect_desktop_session"
                ) && reference.token == id)
        }) {
            if selected.as_ref().is_some_and(|prior| prior != &reference) {
                return Err(invalid("object ID is ambiguous"));
            }
            if !reference.object_kind.is_lifecycle_bound() {
                let expiry = chrono::DateTime::parse_from_rfc3339(&reference.expires_at)
                    .map_err(|_| invalid("object expiry is invalid"))?;
                if expiry.timestamp_millis() <= now as i64 {
                    return Err(invalid("object observation expired"));
                }
            }
            selected = Some(reference);
        }
    }
    selected
        .ok_or_else(|| invalid("object ID is unavailable or was not observed in this conversation"))
}

fn restore_field(
    object: &mut serde_json::Map<String, Value>,
    native: &str,
    model: &str,
    history: &[ChatMessage],
    now: u64,
) -> Result<(), AgentError> {
    if object.contains_key(native) {
        return Err(invalid(
            "use an observed ID rather than a complete object reference",
        ));
    }
    if let Some(id) = object.remove(model) {
        if id.is_null() {
            object.insert(native.into(), Value::Null);
        } else {
            let reference = resolve_id(
                id.as_str()
                    .ok_or_else(|| invalid("object ID must be a string"))?,
                history,
                now,
            )?;
            object.insert(native.into(), json!(reference));
        }
    }
    Ok(())
}

pub(crate) fn resolve(
    tool: &str,
    value: &mut Value,
    history: &[ChatMessage],
    now: u64,
) -> Result<(), AgentError> {
    if !supports(tool) {
        return Ok(());
    }
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid("arguments must be an object"))?;
    if let Some((native, model)) = field(tool) {
        restore_field(object, native, model, history, now)?;
        if let Some(reference) = object.get(native).filter(|value| !value.is_null()) {
            let registry = crate::ai_assistant::ai_assistant_provider_registry();
            let schema = &registry
                .capability_for_tool(tool)
                .ok_or_else(|| invalid("tool is not registered"))?
                .tool_spec
                .parameters_schema["properties"][native];
            crate::model_input::validate_format_with_schema(tool, schema, reference)
                .map_err(|_| invalid("object ID identifies the wrong kind or invalid reference"))?;
        }
    }
    if tool == "preview_computer_action" {
        for action in object
            .get_mut("actions")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| invalid("actions must be an array"))?
        {
            restore_field(
                action
                    .as_object_mut()
                    .ok_or_else(|| invalid("each action must be an object"))?,
                "target",
                "target_id",
                history,
                now,
            )?;
        }
    }
    Ok(())
}

/// Receives authoritative consent, never model-selected filesystem metadata.
pub(crate) fn resolve_directories(
    tool: &str,
    value: &mut Value,
    session: &PersistedAgentSession,
    now: u64,
) -> Result<(), AgentError> {
    if tool == "request_permissions" {
        if let Some(items) = value.get_mut("items").and_then(Value::as_array_mut) {
            for item in items {
                if let Some(tool) = item["tool_name"].as_str().map(str::to_owned)
                    && let Some(exact) = item.get_mut("exact_input")
                {
                    resolve_directories(&tool, exact, session, now)?;
                }
            }
        }
    } else if copy_tool(tool) {
        let output = value
            .get_mut("output")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| invalid("output is required"))?;
        let id = output
            .get("directory_request_id")
            .cloned()
            .unwrap_or(Value::Null);
        let selector = if id.is_null() {
            json!({})
        } else {
            json!({"directory_request_id":id})
        };
        let directory = crate::file_scope::select_output_directory(
            session,
            &ToolCall {
                id: "resolve-output-directory".into(),
                name: tool.into(),
                arguments_json: selector.to_string(),
            },
            now,
        )
        .map_err(|_| {
            invalid("output directory is not currently approved or the selection is ambiguous")
        })?;
        if output
            .get("destination_parent")
            .is_some_and(|reference| reference != &json!(directory))
        {
            return Err(invalid(
                "output directory reference does not match the selected consent",
            ));
        }
        output.insert("destination_parent".into(), json!(directory));
        output.insert("directory_request_id".into(), id);
    }
    Ok(())
}

fn project_field(object: &mut serde_json::Map<String, Value>, native: &str, model: &str) {
    if let Some(reference) = object.remove(native) {
        let id = serde_json::from_value::<ObjectRef>(reference.clone())
            .map(|reference| json!(object_id(&reference)))
            .unwrap_or(Value::Null);
        object.insert(model.into(), id);
    }
}

pub(crate) fn project_arguments(tool: &str, value: &mut Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if let Some((native, model)) = field(tool) {
        project_field(object, native, model);
    }
    if copy_tool(tool)
        && let Some(output) = object.get_mut("output").and_then(Value::as_object_mut)
    {
        output.remove("destination_parent");
    }
    if tool == "preview_computer_action"
        && let Some(actions) = object.get_mut("actions").and_then(Value::as_array_mut)
    {
        for action in actions {
            if let Some(action) = action.as_object_mut() {
                project_field(action, "target", "target_id");
            }
        }
    }
}

fn project_schema_field(schema: &mut Value, native: &str, model: &str) {
    let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) else {
        return;
    };
    let Some(original) = properties.remove(native) else {
        return;
    };
    let nullable = ["anyOf", "oneOf"].iter().any(|key| {
        original[*key]
            .as_array()
            .is_some_and(|variants| variants.iter().any(|variant| variant["type"] == "null"))
    });
    properties.insert(model.into(), json!({"type":if nullable {json!(["string","null"])} else {json!("string")},"minLength":1,"maxLength":512,"description":"Copy the object ID from the matching verified document or preview observation. The server restores the exact reference."}));
    if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
        for key in required {
            if key == native {
                *key = json!(model);
            }
        }
    }
}

pub(crate) fn project_tool(tool: &mut ToolSpec) {
    if let Some((native, model)) = field(&tool.name) {
        project_schema_field(&mut tool.parameters_schema, native, model);
    }
    if copy_tool(&tool.name)
        && let Some(output) = tool.parameters_schema.pointer_mut("/properties/output")
    {
        output["properties"]
            .as_object_mut()
            .unwrap()
            .remove("destination_parent");
        output["required"] = json!(["native_file_name"]);
    }
    if tool.name == "preview_computer_action"
        && let Some(action) = tool
            .parameters_schema
            .pointer_mut("/properties/actions/items")
    {
        project_schema_field(action, "target", "target_id");
    }
}

fn project_refs(value: &mut Value) {
    if let Ok(reference) = serde_json::from_value::<ObjectRef>(value.clone()) {
        *value = json!({"id":object_id(&reference),"kind":reference.object_kind});
    } else {
        match value {
            Value::Object(object) => {
                for child in object.values_mut() {
                    project_refs(child);
                }
            }
            Value::Array(array) => {
                for child in array {
                    project_refs(child);
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn project_result_message(message: &mut ChatMessage) {
    let Some(source) = message
        .data_envelope
        .as_ref()
        .map(|envelope| envelope.provenance.source_tool_name.as_str())
    else {
        return;
    };
    if source.starts_with("inspect_")
        && !matches!(
            source,
            "inspect_desktop_ui"
                | "inspect_desktop_session"
                | "inspect_files"
                | "inspect_spreadsheets"
        )
        && let Ok(mut value) = serde_json::from_str::<Value>(&message.text)
    {
        project_refs(&mut value);
        message.text = value.to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_scope::{DirectoryConsentSource, DirectoryProposal};
    use desk_agent_protocol::{AgentScope, ExecutionMode, computer_use::ObjectKind};

    fn fixture() -> (PersistedAgentSession, ObjectRef) {
        let reference = ObjectRef {
            token: "document".into(),
            snapshot_id: "snapshot".into(),
            object_kind: ObjectKind::Document,
            expires_at: "2099-01-01T00:00:00Z".into(),
        };
        let mut session = PersistedAgentSession::new(
            "run",
            "owner",
            "device",
            1,
            AgentScope {
                granted: vec![],
                expires_at: None,
                mode: ExecutionMode::ReadOnly,
                policy_name: None,
            },
            "2026-01-01T00:00:00Z",
        );
        session.adopt_client_metadata(
            Some("client"),
            crate::session::AgentSessionSurface::AiAssistant,
        );
        session.conversation = crate::result_model_ids::tests::evidence(
            "inspect_live_document",
            "read",
            json!({}),
            json!({"document":reference}),
        );
        (session, reference)
    }

    #[test]
    fn document_id_restores_exact_observation_and_rejects_wrong_kind_expiry_and_other_sessions() {
        let (session, reference) = fixture();
        let input = json!({"target_id":object_id(&reference),"text":"requested"});
        let call = ToolCall {
            id: "replace".into(),
            name: "replace_live_document_body".into(),
            arguments_json: input.to_string(),
        };
        let resolved = crate::ui_model_ids::resolve_session_call(&call, &session, 1).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&resolved.arguments_json).unwrap()["target"],
            json!(reference)
        );
        assert!(crate::ui_model_ids::same_call_input(
            &call.name,
            &call.arguments_json,
            &resolved.arguments_json
        ));
        assert!(resolve_id(&object_id(&reference), &[], 1).is_err());
        assert!(
            resolve_id(
                &object_id(&reference),
                &session.conversation,
                4_100_000_000_000
            )
            .is_err()
        );
        let mut wrong = input.clone();
        assert!(
            resolve(
                "patch_live_spreadsheet_cell",
                &mut wrong,
                &session.conversation,
                1
            )
            .is_err()
        );
        let mut projected = session.conversation.last().unwrap().clone();
        project_result_message(&mut projected);
        assert!(!projected.text.contains("snapshot_id"));
        assert!(projected.text.contains(&object_id(&reference)));
        assert!(
            session
                .conversation
                .last()
                .unwrap()
                .text
                .contains("snapshot_id")
        );
    }

    #[test]
    fn different_snapshots_never_silently_resolve_to_the_latest() {
        let (mut session, original) = fixture();
        let mut next = original.clone();
        next.snapshot_id = "next".into();
        session
            .conversation
            .extend(crate::result_model_ids::tests::evidence(
                "inspect_live_document",
                "next-read",
                json!({}),
                json!({"document":next}),
            ));
        assert_ne!(object_id(&original), object_id(&next));
        assert_eq!(
            resolve_id(&object_id(&original), &session.conversation, 1).unwrap(),
            original
        );
    }

    #[test]
    fn copy_directory_is_restored_from_owner_consent_before_freezing() {
        let (mut session, reference) = fixture();
        let subject = session
            .file_scope_subject("owner", "device", "run")
            .unwrap();
        let directory = ObjectRef {
            token: "directory".into(),
            snapshot_id: "directory-snapshot".into(),
            object_kind: ObjectKind::Directory,
            expires_at: String::new(),
        };
        session
            .file_scope
            .propose(
                &subject,
                0,
                DirectoryProposal {
                    request_id: "approved-directory".into(),
                    requested_path: "/tmp/output".into(),
                    canonical_path: "/tmp/output".into(),
                    purpose: "save requested copy".into(),
                    source: DirectoryConsentSource::ModelProposal,
                    directory: directory.clone(),
                },
                1,
            )
            .unwrap();
        session
            .file_scope
            .decide(&subject, 1, "approved-directory", true, 1)
            .unwrap();
        let call=ToolCall{id:"copy".into(),name:"replace_pages_copy_body".into(),arguments_json:json!({"target_id":object_id(&reference),"output":{"native_file_name":"report.pages","directory_request_id":"approved-directory"},"text":"requested"}).to_string()};
        let resolved = crate::ui_model_ids::resolve_session_call(&call, &session, 1).unwrap();
        let canonical: Value = serde_json::from_str(&resolved.arguments_json).unwrap();
        assert_eq!(canonical["output"]["destination_parent"], json!(directory));
        assert!(crate::ui_model_ids::same_call_input(
            &call.name,
            &call.arguments_json,
            &resolved.arguments_json
        ));
        let mut changed = canonical.clone();
        changed["output"]["directory_request_id"] = json!("other");
        assert!(!crate::ui_model_ids::same_call_input(
            &call.name,
            &call.arguments_json,
            &changed.to_string()
        ));
        let mut bad: Value = serde_json::from_str(&call.arguments_json).unwrap();
        bad["output"]["directory_request_id"] = json!("unapproved");
        assert!(
            crate::ui_model_ids::resolve_session_call(
                &ToolCall {
                    arguments_json: bad.to_string(),
                    ..call
                },
                &session,
                1
            )
            .is_err()
        );
    }
}
