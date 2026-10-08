//! Stable model references to authoritative results. Hashes remain native evidence.

use crate::chat::{ChatMessage, ChatRole, ToolCallRef, ToolSpec};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use desk_agent_protocol::{
    AgentError, AgentErrorKind,
    communication::CommunicationDraftHandoff,
    computer_use::{
        ComputerActionCompleted, ComputerActionOutput, ComputerActionResultClass,
        CreatedFileArtifactOutput,
    },
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn invalid(detail: &str) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: format!(
            "Invalid result reference: {detail}. Read a current verified result and use its returned ID; no action was executed."
        ),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

/// This alias locates evidence; it is neither a digest check nor an authorization.
pub(crate) fn alias(kind: &str, identity: &str) -> String {
    let digest = Sha256::digest(format!("model-result:v1:{kind}:{identity}"));
    format!("{kind}_{}", URL_SAFE_NO_PAD.encode(&digest[..9]))
}

fn artifact_id(artifact: &CreatedFileArtifactOutput) -> String {
    alias(
        "artifact",
        &serde_json::to_string(&(&artifact.file, &artifact.digest_sha256))
            .expect("artifact identity serialization"),
    )
}

fn artifact(value: &Value) -> Option<CreatedFileArtifactOutput> {
    let completed: ComputerActionCompleted = serde_json::from_value(value.clone()).ok()?;
    if completed.result != ComputerActionResultClass::Verified {
        return None;
    }
    let artifact = match completed.output? {
        ComputerActionOutput::FileArtifact(artifact) => artifact,
        ComputerActionOutput::DocumentArtifact(output) => output.artifact,
        ComputerActionOutput::TextFileMutation(output) if output.verified => output.updated_file?,
        _ => return None,
    };
    artifact.validate().ok()?;
    Some(artifact)
}

fn handoff(value: &Value) -> Option<CommunicationDraftHandoff> {
    let direct = serde_json::from_value::<CommunicationDraftHandoff>(value.clone()).ok();
    let handoff = direct.or_else(|| {
        let completed: ComputerActionCompleted = serde_json::from_value(value.clone()).ok()?;
        if completed.result != ComputerActionResultClass::Verified {
            return None;
        }
        match completed.output? {
            ComputerActionOutput::CommunicationHandoff(output) => Some(output),
            _ => None,
        }
    })?;
    handoff.validate().ok()?;
    Some(handoff)
}

pub(crate) fn verified_results(history: &[ChatMessage], now: u64) -> Vec<(ToolCallRef, Value)> {
    let registry = crate::ai_assistant::ai_assistant_provider_registry();
    history
        .iter()
        .filter(|message| matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput))
        .filter_map(|message| {
            let id = message.tool_call_id.as_deref()?;
            let mut calls = history
                .iter()
                .filter(|message| message.role == ChatRole::Assistant)
                .flat_map(|message| &message.tool_calls)
                .filter(|call| call.id == id);
            let call = calls.next()?;
            if calls.next().is_some() {
                return None;
            }
            let result = message.trusted_tool_result();
            let envelope = result.data_envelope.as_ref()?;
            let capability = registry.capability_for_tool(&call.name)?;
            let provider = registry.provider_for_capability(&capability.wire.capability_id)?;
            if envelope.validate().is_err()
                || crate::model_egress::envelope_expires_by(envelope, now)
                || envelope.provenance.source_tool_name != call.name
                || envelope.provenance.source_provider_id != provider.wire.provider_id
                || envelope.digest_sha256 != format!("{:x}", Sha256::digest(result.text.as_bytes()))
            {
                return None;
            }
            Some((call.clone(), serde_json::from_str(&result.text).ok()?))
        })
        .collect()
}

fn unique_result(
    results: &[(ToolCallRef, Value)],
    predicate: impl Fn(&ToolCallRef, &Value) -> bool,
) -> Result<(ToolCallRef, Value), AgentError> {
    let mut selected = None;
    for (call, value) in results
        .iter()
        .filter(|(call, value)| predicate(call, value))
    {
        if selected
            .as_ref()
            .is_some_and(|prior: &(ToolCallRef, Value)| prior.0.id != call.id || prior.1 != *value)
        {
            return Err(invalid("ID is ambiguous or collides with another result"));
        }
        selected = Some((call.clone(), value.clone()));
    }
    selected.ok_or_else(|| {
        invalid("ID is unknown, expired, the wrong type, or its original result is unavailable")
    })
}

pub(crate) fn supports(tool: &str) -> bool {
    matches!(
        tool,
        "update_text_file"
            | "delete_text_file"
            | "prepare_gmail_draft"
            | "send_gmail_message"
            | "send_slack_message"
            | "create_word_report"
    ) || crate::provider_preflight::text_file::selection::supports(tool)
        || tool == "read_text_file"
}

/// Freeze the exact source envelopes in the existing action-result lineage.
pub(crate) fn input_envelope_ids(
    tool: &str,
    value: &Value,
    history: &[ChatMessage],
) -> Result<Vec<String>, AgentError> {
    let selection = match tool {
        "prepare_gmail_draft" => value
            .pointer("/attachment/artifact")
            .cloned()
            .map(|artifact| ("artifact", artifact)),
        "send_gmail_message" | "send_slack_message" => value
            .get("handoff")
            .cloned()
            .map(|handoff| ("handoff", handoff)),
        _ => None,
    };
    let Some((kind, expected)) = selection else {
        return Ok(Vec::new());
    };
    let results = verified_results(history, 1);
    let (source, selected) = unique_result(&results, |call, value| {
        if kind == "artifact" {
            artifact(value).is_some_and(|artifact| json!(artifact) == expected)
        } else {
            matches!(
                call.name.as_str(),
                "prepare_gmail_draft" | "prepare_slack_message"
            ) && handoff(value).is_some_and(|handoff| json!(handoff) == expected)
        }
    })?;
    let mut ids = history
        .iter()
        .filter(|message| message.tool_call_id.as_deref() == Some(&source.id))
        .filter_map(|message| {
            let result = message.trusted_tool_result();
            if serde_json::from_str::<Value>(&result.text).ok().as_ref() != Some(&selected) {
                return None;
            }
            let envelope = result.data_envelope.as_ref()?;
            if envelope.validate().is_err()
                || envelope.provenance.source_tool_name != source.name
                || envelope.digest_sha256 != format!("{:x}", Sha256::digest(result.text.as_bytes()))
            {
                return None;
            }
            Some(envelope.envelope_id.clone())
        })
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Err(invalid("source lineage is unavailable"));
    }
    Ok(ids)
}

fn expand_file_selectors(
    value: &mut Value,
    results: &[(ToolCallRef, Value)],
) -> Result<(), AgentError> {
    match value {
        Value::Object(object) => {
            if let Some(id) = object.get("file_result_call_id").and_then(Value::as_str)
                && id.starts_with("result_")
            {
                let (source, _) =
                    unique_result(results, |call, _| alias("result", &call.id) == id)?;
                object.insert("file_result_call_id".into(), json!(source.id));
            }
            for child in object.values_mut() {
                expand_file_selectors(child, results)?;
            }
        }
        Value::Array(array) => {
            for child in array {
                expand_file_selectors(child, results)?;
            }
        }
        _ => {}
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
    let results = verified_results(history, now);
    expand_file_selectors(value, &results)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid("arguments must be an object"))?;
    if tool == "create_word_report"
        && let Some(id) = object.remove("web_search_result_id")
    {
        if object.contains_key("web_search_call_id") || object.contains_key("web_sources") {
            return Err(invalid("use only the search and source IDs"));
        }
        let id = id
            .as_str()
            .ok_or_else(|| invalid("web_search_result_id must be a string"))?;
        let (source, result) = unique_result(&results, |call, _| {
            call.name == "search_public_web" && alias("search", &call.id) == id
        })?;
        let requested = object
            .remove("web_source_ids")
            .ok_or_else(|| invalid("web_source_ids is required with web_search_result_id"))?;
        let requested = requested
            .as_array()
            .filter(|ids| (1..=8).contains(&ids.len()))
            .ok_or_else(|| invalid("select 1 to 8 Web source IDs"))?;
        let observed = result["results"]
            .as_array()
            .ok_or_else(|| invalid("search result list is unavailable"))?;
        let mut restored = Vec::new();
        for id in requested {
            let id = id
                .as_str()
                .ok_or_else(|| invalid("Web source ID must be a string"))?;
            let matches = observed
                .iter()
                .filter_map(|entry| web_source(entry).filter(|source| web_source_id(source) == id))
                .collect::<Vec<_>>();
            let source = matches
                .first()
                .ok_or_else(|| invalid("Web source ID is not in this search"))?;
            if matches.iter().any(|other| other != source) || restored.contains(source) {
                return Err(invalid("Web source is ambiguous or repeated"));
            }
            restored.push(source.clone());
        }
        object.insert("web_search_call_id".into(), json!(source.id));
        object.insert("web_sources".into(), json!(restored));
    }
    if matches!(tool, "update_text_file" | "delete_text_file") {
        if let Some(id) = object.remove("file_version_id") {
            if object.contains_key("file_result_call_id") || object.contains_key("expected_sha256")
            {
                return Err(invalid("use only file_version_id for the file version"));
            }
            let id = id
                .as_str()
                .ok_or_else(|| invalid("file_version_id must be a string"))?;
            let (source, _) = unique_result(&results, |call, _| alias("file", &call.id) == id)?;
            object.insert("file_result_call_id".into(), json!(source.id));
        }
        let id = object
            .get("file_result_call_id")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("file_version_id is required"))?;
        let evidence = crate::provider_preflight::text_file::resolve_file_result_from_history(
            history, id, now,
        )?;
        if object
            .get("expected_sha256")
            .is_some_and(|hash| hash != evidence.sha256())
        {
            return Err(invalid("provided hash does not match the fixed result"));
        }
        object.insert("expected_sha256".into(), json!(evidence.sha256()));
    }
    if tool == "prepare_gmail_draft" {
        if let Some(attachment) = object.get_mut("attachment").and_then(Value::as_object_mut)
            && let Some(id) = attachment.remove("artifact_result_id")
        {
            if attachment.contains_key("artifact") {
                return Err(invalid("use only artifact_result_id"));
            }
            let id = id
                .as_str()
                .ok_or_else(|| invalid("artifact_result_id must be a string"))?;
            let (_, result) = unique_result(&results, |call, value| {
                matches!(
                    call.name.as_str(),
                    "create_text_file"
                        | "create_local_message_draft"
                        | "create_workbook"
                        | "create_formula_workbook"
                        | "create_word_report"
                        | "convert_document"
                        | "update_text_file"
                ) && artifact(value).is_some_and(|artifact| artifact_id(&artifact) == id)
            })?;
            let artifact = artifact(&result).expect("selected typed artifact");
            let expiry = chrono::DateTime::parse_from_rfc3339(&artifact.file.expires_at)
                .map_err(|_| invalid("artifact expiry is invalid"))?;
            if expiry.timestamp_millis() <= now as i64 {
                return Err(invalid("artifact object expired"));
            }
            attachment.insert("artifact".into(), json!(artifact));
        }
        let labels = object
            .get("attachment")
            .and_then(|attachment| attachment.pointer("/artifact/file_name"))
            .map_or_else(|| json!([]), |name| json!([name]));
        if let Some(draft) = object.get_mut("draft").and_then(Value::as_object_mut) {
            draft.entry("attachment_labels").or_insert(labels);
        }
    }
    if matches!(tool, "send_gmail_message" | "send_slack_message")
        && let Some(id) = object.remove("handoff_result_id")
    {
        if object.contains_key("handoff")
            || object.contains_key("draft")
            || object.contains_key("body_plain_text")
        {
            return Err(invalid(
                "choose handoff_result_id without repeating handoff or message content",
            ));
        }
        let id = id
            .as_str()
            .ok_or_else(|| invalid("handoff_result_id must be a string"))?;
        let prepare = if tool == "send_gmail_message" {
            "prepare_gmail_draft"
        } else {
            "prepare_slack_message"
        };
        let (source, result) = unique_result(&results, |call, value| {
            call.name == prepare
                && handoff(value).is_some_and(|handoff| alias("draft", &handoff.handoff_id) == id)
        })?;
        let handoff = handoff(&result).expect("selected typed handoff");
        let mut original: Value = serde_json::from_str(&source.arguments_json)
            .map_err(|_| invalid("original preparation input is unavailable"))?;
        crate::model_input::fill_versions(prepare, &mut original);
        if tool == "send_gmail_message" {
            let mut draft = original
                .get("draft")
                .cloned()
                .ok_or_else(|| invalid("original draft is unavailable"))?;
            let snapshot = handoff
                .send_payload_snapshot
                .as_ref()
                .ok_or_else(|| invalid("handoff only permits manual sending"))?;
            draft["attachment_labels"] = json!(
                snapshot
                    .payload
                    .attachments
                    .iter()
                    .map(|artifact| artifact.file_name.clone())
                    .collect::<Vec<_>>()
            );
            object.insert("draft".into(), draft);
        } else {
            object.insert(
                "body_plain_text".into(),
                original
                    .get("body_plain_text")
                    .cloned()
                    .ok_or_else(|| invalid("original message is unavailable"))?,
            );
        }
        object.insert("handoff".into(), json!(handoff));
    }
    Ok(())
}

pub(crate) fn project_arguments(tool: &str, value: &mut Value) {
    if !supports(tool) {
        return;
    }
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if tool == "create_word_report"
        && let Some(id) = object
            .remove("web_search_call_id")
            .and_then(|id| id.as_str().map(str::to_owned))
    {
        object.insert("web_search_result_id".into(), json!(alias("search", &id)));
        if let Some(sources) = object
            .remove("web_sources")
            .and_then(|sources| sources.as_array().cloned())
        {
            object.insert(
                "web_source_ids".into(),
                json!(sources.iter().map(web_source_id).collect::<Vec<_>>()),
            );
        }
    }
    if matches!(tool, "update_text_file" | "delete_text_file")
        && let Some(id) = object
            .remove("file_result_call_id")
            .and_then(|id| id.as_str().map(str::to_owned))
    {
        object.insert("file_version_id".into(), json!(alias("file", &id)));
        object.remove("expected_sha256");
    }
    if tool == "prepare_gmail_draft"
        && let Some(attachment) = object.get_mut("attachment").and_then(Value::as_object_mut)
        && let Some(value) = attachment.remove("artifact")
        && let Ok(artifact) = serde_json::from_value::<CreatedFileArtifactOutput>(value)
    {
        attachment.insert("artifact_result_id".into(), json!(artifact_id(&artifact)));
    }
    if matches!(tool, "send_gmail_message" | "send_slack_message")
        && let Some(value) = object.remove("handoff")
        && let Ok(handoff) = serde_json::from_value::<CommunicationDraftHandoff>(value)
    {
        object.insert(
            "handoff_result_id".into(),
            json!(alias("draft", &handoff.handoff_id)),
        );
        object.remove("draft");
        object.remove("body_plain_text");
    }
    project_file_selectors(value);
}

fn project_file_selectors(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if let Some(id) = object.get("file_result_call_id").and_then(Value::as_str)
                && !id.starts_with("result_")
            {
                object.insert("file_result_call_id".into(), json!(alias("result", id)));
            }
            for child in object.values_mut() {
                project_file_selectors(child);
            }
        }
        Value::Array(array) => {
            for child in array {
                project_file_selectors(child);
            }
        }
        _ => {}
    }
}

pub(crate) fn preserves_explicit_evidence(tool: &str, original: &Value, resolved: &Value) -> bool {
    if tool == "request_permissions" {
        let (Some(left), Some(right)) =
            (original["items"].as_array(), resolved["items"].as_array())
        else {
            return false;
        };
        return left.len() == right.len()
            && left.iter().zip(right).all(|(left, right)| {
                left["tool_name"] == right["tool_name"]
                    && left["tool_name"].as_str().is_some_and(|name| {
                        preserves_explicit_evidence(
                            name,
                            &left["exact_input"],
                            &right["exact_input"],
                        )
                    })
            });
    }
    let fields: &[&str] = match tool {
        "update_text_file" | "delete_text_file" => &["expected_sha256"],
        "send_gmail_message" => &["handoff", "draft"],
        "send_slack_message" => &["handoff", "body_plain_text"],
        "create_word_report" => &["web_sources"],
        _ => &[],
    };
    fields.iter().all(|field| {
        original
            .get(*field)
            .is_none_or(|value| resolved.get(*field) == Some(value))
    }) && (tool != "prepare_gmail_draft"
        || original
            .pointer("/attachment/artifact")
            .is_none_or(|value| resolved.pointer("/attachment/artifact") == Some(value)))
}

fn replace_fields(schema: &mut Value, removed: &[&str], field: &str) {
    let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) else {
        return;
    };
    for name in removed {
        properties.remove(*name);
    }
    properties.insert(field.into(), json!({"type":"string","minLength":1,"maxLength":32,"description":"Copy the typed short ID from a verified result in this conversation. The server restores the exact original evidence; never invent an ID."}));
    if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
        required.retain(|value| !removed.iter().any(|name| value == name));
        if !required.contains(&json!(field)) {
            required.push(json!(field));
        }
    }
}

pub(crate) fn project_tool(tool: &mut ToolSpec) {
    match tool.name.as_str() {
        "create_word_report" => {
            let schema = &mut tool.parameters_schema;
            if let Some(properties) = schema["properties"].as_object_mut() {
                properties.remove("web_search_call_id");
                properties.remove("web_sources");
                properties.insert("web_search_result_id".into(), json!({"type":"string","minLength":1,"maxLength":32,"description":"Optional search_result_id copied from an earlier verified search_public_web result."}));
                properties.insert("web_source_ids".into(), json!({"type":"array","minItems":1,"maxItems":8,"uniqueItems":true,"items":{"type":"string","minLength":1,"maxLength":32},"description":"Select source_id values from that search; the server restores the original title and HTTPS URL."}));
                schema["dependentRequired"] = json!({"web_search_result_id":["web_source_ids"],"web_source_ids":["web_search_result_id"]});
            }
            tool.description = tool.description.replace("one prior search_public_web call id from this same run plus 1-8 title/URL pairs copied exactly from that result", "web_search_result_id from one verified prior search_public_web result and 1-8 web_source_ids from that result");
        }
        "update_text_file" | "delete_text_file" => {
            replace_fields(
                &mut tool.parameters_schema,
                &["file_result_call_id", "expected_sha256"],
                "file_version_id",
            );
            tool.description = tool
                .description
                .replace(
                    "copy its file_result_call_id and full SHA-256",
                    "use its file_version_id; the server restores the original full SHA-256",
                )
                .replace("and its full SHA-256", "using its file_version_id");
        }
        "prepare_gmail_draft" => {
            if let Some(attachment) = tool.parameters_schema.pointer_mut("/properties/attachment") {
                replace_fields(attachment, &["artifact"], "artifact_result_id");
            }
        }
        "send_gmail_message" | "send_slack_message" => {
            replace_fields(
                &mut tool.parameters_schema,
                &["handoff", "draft", "body_plain_text"],
                "handoff_result_id",
            );
            tool.description = tool.description.replace("copy the complete handoff output and the owner's original draft verbatim", "use handoff_result_id from the verified preparation result; the server restores the exact original draft").replace("copy the complete handoff and the original body_plain_text verbatim", "use handoff_result_id from the verified preparation result; the server restores the exact original message");
            tool.description = tool.description.replace("copy the complete handoff output and owner-provided body verbatim", "use handoff_result_id from the verified preparation result; the server restores the exact original message");
        }
        _ => {}
    }
}

fn hide_internal(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if object.contains_key("token")
                && object.contains_key("snapshot_id")
                && object.contains_key("object_kind")
                && object.contains_key("expires_at")
            {
                *value = json!({"id":object["token"],"kind":object["object_kind"]});
                return;
            }
            object.retain(|key, _| !key.ends_with("sha256") && key != "schema_version");
            for child in object.values_mut() {
                hide_internal(child);
            }
        }
        Value::Array(array) => {
            for child in array {
                hide_internal(child);
            }
        }
        _ => {}
    }
}

pub(crate) fn project_result_message(message: &mut ChatMessage) {
    if !matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput) {
        return;
    }
    let Some(envelope) = &message.data_envelope else {
        return;
    };
    let source = &envelope.provenance.source_tool_name;
    let Ok(mut value) = serde_json::from_str::<Value>(&message.text) else {
        return;
    };
    if !value.is_object() {
        return;
    }
    let Some(call_id) = &message.tool_call_id else {
        return;
    };
    let original = serde_json::from_str::<Value>(&message.trusted_tool_result().text)
        .unwrap_or_else(|_| value.clone());
    if source == "search_public_web" {
        value["search_result_id"] = json!(alias("search", call_id));
        if let Some(entries) = value["results"].as_array_mut() {
            for entry in entries {
                if let Some(source) = web_source(entry) {
                    entry["source_id"] = json!(web_source_id(&source));
                }
            }
        }
        message.text = value.to_string();
        return;
    }
    let created = artifact(&original);
    if let Some(artifact) = &created {
        value["artifact_result_id"] = json!(artifact_id(artifact));
    }
    if matches!(
        source.as_str(),
        "read_text_file" | "create_text_file" | "create_local_message_draft" | "update_text_file"
    ) {
        let verified = created.as_ref().is_some_and(|artifact| {
            artifact.media_type == crate::provider_preflight::TEXT_ARTIFACT_MEDIA_TYPE
        }) || original
            .pointer("/ReadContext/FileContentRead")
            .and_then(|read| serde_json::from_value(read.clone()).ok())
            .is_some_and(|read| {
                crate::provider_preflight::text_file::VerifiedTextFile::from_read(call_id, &read)
                    .is_ok()
            });
        if verified {
            value["file_version_id"] = json!(alias("file", call_id));
        }
    }
    if source == "inspect_files" || source == "read_text_file" || created.is_some() {
        value["file_result_call_id"] = json!(alias("result", call_id));
    }
    if let Some(handoff) = handoff(&original) {
        value = json!({"handoff_result_id":alias("draft", &handoff.handoff_id),"verification":handoff.verification,"send_authority":handoff.send_authority,"surface":handoff.surface,"compose_id":handoff.compose_id,"business_effect":"draft_prepared","message_sent":false});
    }
    if supports(source)
        || created.is_some()
        || source == "prepare_outlook_draft"
        || source == "prepare_slack_message"
        || source == "inspect_files"
        || source.starts_with("inspect_live_")
        || source.starts_with("inspect_office_")
    {
        hide_internal(&mut value);
        message.text = value.to_string();
    }
}

fn web_source(entry: &Value) -> Option<Value> {
    let title = entry["title"].as_str()?;
    let url = entry["url"].as_str()?;
    if title.is_empty()
        || title.chars().count() > 240
        || !url.starts_with("https://")
        || url.len() > 2048
    {
        return None;
    }
    Some(json!({"title":title,"url":url}))
}

fn web_source_id(source: &Value) -> String {
    alias(
        "source",
        &serde_json::to_string(&(source.get("title"), source.get("url")))
            .expect("Web source identity serialization"),
    )
}

#[cfg(test)]
pub(crate) mod tests;
