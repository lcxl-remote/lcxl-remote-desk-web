//! Project only server-owned delegation selectors, never task text or reports.

use serde_json::Value;

use crate::chat::{ChatMessage, ChatRole};

use super::tools;

pub(crate) fn is_runtime(message: &ChatMessage) -> bool {
    message.role == ChatRole::SystemEvent
        && message.data_envelope.as_ref().is_some_and(|label| {
            label.provenance.source_provider_id == crate::dynamic_run::RUN_CONTROL_PROVIDER_ID
                && matches!(
                    label.provenance.source_tool_name.as_str(),
                    "delegation_state" | "subagent_notification"
                )
        })
}

pub(crate) fn is_receipt(message: &ChatMessage) -> bool {
    matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput)
        && message.tool_ok != Some(false)
        && message
            .trusted_tool_result()
            .data_envelope
            .as_ref()
            .is_some_and(|label| {
                label.provenance.source_provider_id == crate::dynamic_run::RUN_CONTROL_PROVIDER_ID
                    && tools::effect(&label.provenance.source_tool_name).is_some()
            })
}

pub(crate) fn omit_internal_selectors(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        object.remove("wait_id");
    }
    fn task(value: &mut Value) {
        if value.get("task_id").is_some()
            && value.get("state").is_some()
            && let Some(object) = value.as_object_mut()
        {
            object.remove("child_session_id");
            object.remove("group_id");
        }
    }
    task(value);
    if let Some(summary) = value.get_mut("task") {
        task(summary);
    }
    for field in ["items", "tasks"] {
        if let Some(items) = value.get_mut(field).and_then(Value::as_array_mut) {
            for item in items {
                task(item);
                if let Some(summary) = item.get_mut("task") {
                    task(summary);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
