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

pub(crate) fn visit_message_ids(
    message: &ChatMessage,
    history: &[ChatMessage],
    visitor: &mut impl FnMut(&str),
) {
    let source = message.trusted_tool_result();
    let labelled_receipt = source.data_envelope.as_ref().is_some_and(|label| {
        label.provenance.source_provider_id == crate::dynamic_run::RUN_CONTROL_PROVIDER_ID
            && tools::effect(&label.provenance.source_tool_name).is_some()
    });
    let tool_result = matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput)
        && message.tool_ok != Some(false)
        && (labelled_receipt
            || message.tool_call_id.as_ref().is_some_and(|id| {
                history
                    .iter()
                    .flat_map(|message| &message.tool_calls)
                    .any(|call| call.id == *id && tools::effect(&call.name).is_some())
            }));
    if !tool_result && !is_runtime(message) {
        return;
    }
    let Ok(mut value) = crate::image_input::structured_tool_result(&source.text) else {
        return;
    };
    let value = if is_runtime(message) {
        if value.get("runtime_delegation_state").is_some() {
            value.get_mut("runtime_delegation_state").unwrap()
        } else {
            &mut value
        }
    } else {
        &mut value
    };
    visit_result_ids(value, &mut |id| {
        if let Some(id) = id.as_str().filter(|id| super::valid_id(id)) {
            visitor(id);
        }
    });
}

/// These paths belong to runtime receipts. Identically named fields inside an
/// objective, answer or arbitrary business JSON are not delegation selectors.
pub(crate) fn visit_result_ids(value: &mut Value, visitor: &mut impl FnMut(&mut Value)) {
    fn task(value: &mut Value, visitor: &mut impl FnMut(&mut Value)) {
        if let Some(id) = value.get_mut("task_id") {
            visitor(id);
        }
    }
    task(value, visitor);
    if let Some(summary) = value.get_mut("task") {
        task(summary, visitor);
    }
    for field in ["items", "tasks", "events"] {
        if let Some(items) = value.get_mut(field).and_then(Value::as_array_mut) {
            for item in items {
                task(item, visitor);
                if let Some(summary) = item.get_mut("task") {
                    task(summary, visitor);
                }
            }
        }
    }
}

pub(crate) fn omit_internal_selectors(value: &mut Value) {
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

pub(crate) fn visit_argument_ids(
    tool: &str,
    value: &mut Value,
    visitor: &mut impl FnMut(&mut Value) -> Result<(), desk_agent_protocol::AgentError>,
) -> Result<(), desk_agent_protocol::AgentError> {
    match tool {
        tools::STATUS | tools::RESULT | tools::CANCEL | tools::MESSAGE => {
            if let Some(id) = value.get_mut("task_id") {
                visitor(id)?;
            }
        }
        tools::WAIT => {
            if let Some(ids) = value.get_mut("task_ids").and_then(Value::as_array_mut) {
                for id in ids {
                    visitor(id)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests;
