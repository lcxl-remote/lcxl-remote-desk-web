//! Model-only assistant data views. Native snapshots retain their full evidence.

use crate::chat::{ChatMessage, ChatRole};
use serde_json::Value;

pub(crate) fn tool(message: &ChatMessage) -> Option<&str> {
    if !matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput) {
        return None;
    }
    let tool = &message.data_envelope.as_ref()?.provenance.source_tool_name;
    crate::ai_assistant::data_tools::is_data_tool(tool).then_some(tool)
}

/// Known protocol locations only; document text and arbitrary JSON are opaque.
pub(crate) fn conversations(value: &Value, visitor: &mut impl FnMut(&str)) {
    fn locator(value: &Value, visitor: &mut impl FnMut(&str)) {
        for field in ["conversation_id", "root_conversation_id"] {
            if let Some(id) = value.get(field).and_then(Value::as_str) {
                visitor(id);
            }
        }
    }
    locator(value, visitor);
    if let Some(value) = value.get("locator") {
        locator(value, visitor);
    }
    if let Some(hits) = value.get("hits").and_then(Value::as_array) {
        for hit in hits {
            if let Some(value) = hit.get("locator") {
                locator(value, visitor);
            }
        }
    }
}

fn remove(value: &mut Value, fields: &[&str]) {
    if let Some(object) = value.as_object_mut() {
        for field in fields {
            object.remove(*field);
        }
    }
}

fn omit_nulls(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        object.retain(|_, value| !value.is_null());
    }
}

fn device(value: &mut Value) {
    if let Some(device) = value.get_mut("source_device") {
        remove(device, &["historical"]);
        omit_nulls(device);
    }
}

fn source(value: &mut Value) {
    remove(value, &["revision", "message_id"]);
    if let Some(locator) = value.get("locator").cloned() {
        for field in ["conversation_id", "root_conversation_id"] {
            if field == "root_conversation_id"
                && locator.get(field) == locator.get("conversation_id")
            {
                continue;
            }
            if let Some(id) = locator.get(field) {
                value[field] = id.clone();
            }
        }
    }
    remove(value, &["locator"]);
    device(value);
    omit_nulls(value);
}

fn native(value: &mut Value) {
    remove(value, &["revision"]);
    device(value);
    omit_nulls(value);
}

fn memory(value: &mut Value) {
    // Memory revisions are required by update/forget, unlike source revisions.
    device(value);
    omit_nulls(value);
}

pub(crate) fn project_message(message: &mut ChatMessage) {
    let Some(tool) = tool(message).map(str::to_owned) else {
        return;
    };
    let Ok(mut value) = serde_json::from_str::<Value>(&message.text) else {
        return;
    };
    match tool.as_str() {
        "search_conversations" => {
            remove(&mut value, &["retrieval_mode"]);
            if let Some(hits) = value.get_mut("hits").and_then(Value::as_array_mut) {
                for hit in hits {
                    source(hit);
                    if let Some(summary) = hit.get_mut("native_status_summary") {
                        remove(summary, &["source_revision"]);
                        if let Some(items) = summary.get_mut("items").and_then(Value::as_array_mut)
                        {
                            for item in items {
                                native(item);
                            }
                        }
                        if let Some(sources) = summary
                            .get_mut("additional_sources")
                            .and_then(Value::as_array_mut)
                        {
                            for source in sources {
                                remove(source, &["revision"]);
                            }
                        }
                    }
                }
            }
        }
        "read_conversation" => {
            remove(&mut value, &["actual_bytes"]);
            if let Some(messages) = value.get_mut("messages").and_then(Value::as_array_mut) {
                for message in messages {
                    source(message);
                }
            }
            if let Some(items) = value
                .get_mut("related_results")
                .and_then(Value::as_array_mut)
            {
                for item in items {
                    native(item);
                }
            }
        }
        "read_memory_source" => source(&mut value),
        "search_memories" => {
            remove(&mut value, &["memory_state_generation"]);
            if let Some(memories) = value.get_mut("memories").and_then(Value::as_array_mut) {
                for item in memories {
                    memory(item);
                }
            }
        }
        "read_memory_history" => {
            if let Some(versions) = value.get_mut("versions").and_then(Value::as_array_mut) {
                for version in versions {
                    omit_nulls(version);
                }
            }
        }
        "manage_memories" => {
            if let Some(item) = value.get_mut("memory") {
                memory(item);
            }
            if let Some(receipt) = value.get_mut("control_receipt") {
                remove(
                    receipt,
                    &[
                        "operation_id",
                        "control_generation",
                        "memory_state_generation",
                        "cleanup_pending",
                    ],
                );
            }
        }
        _ => return,
    }
    omit_nulls(&mut value);
    message.text = value.to_string();
}
