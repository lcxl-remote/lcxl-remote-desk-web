//! Internal run-control tool for the model-maintained task-status projection.
//!
//! This tool updates only [`PersistedAgentSession`](crate::session::PersistedAgentSession)
//! UX state. It is not a Provider capability, cannot authorize or dispatch work,
//! and is exposed independently of device grants.

use desk_agent_protocol::{AgentError, AgentErrorKind, Capability};
use serde::Deserialize;
use serde_json::json;

use crate::chat::{ToolCall, ToolSpec};
use crate::dynamic_run::{
    MAX_TASK_DESCRIPTION_BYTES, MAX_TASK_NOTE_BYTES, MAX_TASK_STATUS_ITEMS,
    TASK_STATUS_PROJECTION_SCHEMA_VERSION, TaskStatus, TaskStatusItem, TaskStatusProjection,
};
use crate::registry::{RegisteredTool, ToolEffect};

pub const UPDATE_TASK_STATUS_TOOL_NAME: &str = "update_task_status";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskStatusUpdateParams {
    items: Vec<TaskStatusUpdateItem>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskStatusUpdateItem {
    item_id: String,
    description: String,
    status: TaskStatus,
    #[serde(default)]
    note: Option<String>,
}

fn invalid(detail: impl std::fmt::Display) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: format!("invalid update_task_status arguments: {detail}"),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

/// The internal task-projection tool. The placeholder capability is ignored by
/// [`ToolEffect::RunProjection`] exposure and grants no device authority.
pub fn task_status_tool_registry() -> Vec<RegisteredTool> {
    vec![RegisteredTool {
        spec: ToolSpec {
            name: UPDATE_TASK_STATUS_TOOL_NAME.into(),
            description: "Replace the user-visible task status assessment for this run. Use stable item_id values across updates. This is an AI assessment only: it cannot authorize, execute, or mark durable work complete.".into(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "maxItems": MAX_TASK_STATUS_ITEMS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "item_id": {"type": "string", "maxLength": 128},
                                "description": {"type": "string", "maxLength": MAX_TASK_DESCRIPTION_BYTES},
                                "status": {"type": "string", "enum": ["todo", "in_progress", "blocked", "done", "skipped"]},
                                "note": {"type": "string", "maxLength": MAX_TASK_NOTE_BYTES}
                            },
                            "required": ["item_id", "description", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["items"],
                "additionalProperties": false
            }),
        },
        required_capability: Capability::SystemInfo,
        effect: ToolEffect::RunProjection,
    }]
}

/// Parse a model call and bind all authority-controlled projection fields.
/// Revision, timestamp, and step identity never come from model JSON.
pub fn build_task_status_projection(
    call: &ToolCall,
    current_revision: u64,
    updated_at: String,
    step_id: String,
) -> Result<TaskStatusProjection, AgentError> {
    if call.name != UPDATE_TASK_STATUS_TOOL_NAME {
        return Err(invalid(format!("unexpected tool `{}`", call.name)));
    }
    let params: TaskStatusUpdateParams =
        serde_json::from_str(&call.arguments_json).map_err(|error| invalid(error.to_string()))?;
    let revision = current_revision
        .checked_add(1)
        .ok_or_else(|| invalid("projection revision exhausted"))?;
    let projection = TaskStatusProjection {
        schema_version: TASK_STATUS_PROJECTION_SCHEMA_VERSION,
        revision,
        items: params
            .items
            .into_iter()
            .map(|item| TaskStatusItem {
                item_id: item.item_id,
                description: item.description,
                status: item.status,
                note: item.note,
                last_updated_step_id: step_id.clone(),
            })
            .collect(),
        updated_at,
    };
    projection
        .validate()
        .map_err(|error| invalid(error.to_string()))?;
    Ok(projection)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(arguments_json: &str) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: UPDATE_TASK_STATUS_TOOL_NAME.into(),
            arguments_json: arguments_json.into(),
        }
    }

    #[test]
    fn authority_binds_revision_time_and_step() {
        let projection = build_task_status_projection(
            &call(r#"{"items":[{"item_id":"inspect","description":"Inspect workbook","status":"in_progress"}]}"#),
            4,
            "2026-08-26T00:00:00Z".into(),
            "step-a".into(),
        )
        .unwrap();
        assert_eq!(projection.revision, 5);
        assert_eq!(projection.updated_at, "2026-08-26T00:00:00Z");
        assert_eq!(projection.items[0].last_updated_step_id, "step-a");
    }

    #[test]
    fn rejects_unknown_fields_and_duplicate_ids() {
        assert!(
            build_task_status_projection(
                &call("{}"),
                0,
                "2026-08-26T00:00:00Z".into(),
                "step-a".into(),
            )
            .is_err()
        );
        assert!(
            build_task_status_projection(
                &call(r#"{"items":[],"grant_id":"not-authority"}"#),
                0,
                "2026-08-26T00:00:00Z".into(),
                "step-a".into(),
            )
            .is_err()
        );
        assert!(build_task_status_projection(
            &call(r#"{"items":[{"item_id":"x","description":"one","status":"todo"},{"item_id":"x","description":"two","status":"done"}]}"#),
            0,
            "2026-08-26T00:00:00Z".into(),
            "step-a".into(),
        )
        .is_err());
    }
}
