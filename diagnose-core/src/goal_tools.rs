//! Built-in planning control for one claimed AI Assistant goal segment.

use desk_agent_protocol::{AgentError, AgentErrorKind, Capability};
use serde_json::json;

use crate::chat::{ToolCall, ToolSpec};
use crate::goal::GoalControl;
use crate::registry::{RegisteredTool, ToolEffect};

pub const CONTROL_GOAL_TOOL_NAME: &str = "control_goal";
pub const REQUEST_GOAL_TOOL_NAME: &str = "request_goal";

pub fn open_registry() -> Vec<RegisteredTool> {
    vec![RegisteredTool {
        spec: ToolSpec {
            name: REQUEST_GOAL_TOOL_NAME.into(),
            description: "Ask the device owner to approve exact long-running goal text. With no active goal, this requests a new goal. If the owner says a completed goal is unfinished, pass its goal ID as previous_completed_goal_id; the server verifies its completed result and links the new goal to it. When an existing goal is stopped waiting for clarification, and the owner has replied after that stop, this instead requests a revision of that same goal. The request never starts work, increases budgets, or approves a device action. Provide the complete proposed goal text and call this tool alone; the owner must decide before goal work resumes.".into(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "goal_text": {"type": "string", "minLength": 1, "maxLength": 16384},
                    "previous_completed_goal_id": {"type": "string", "minLength": 1, "maxLength": 256}
                },
                "required": ["goal_text"],
                "additionalProperties": false
            }),
        },
        required_capability: Capability::SystemInfo,
        effect: ToolEffect::GoalOpenPlanning,
    }]
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalOpenProposal {
    pub goal_text: String,
    pub previous_completed_goal_id: Option<String>,
}

pub fn parse_open(call: &ToolCall) -> Result<GoalOpenProposal, AgentError> {
    if call.name != REQUEST_GOAL_TOOL_NAME {
        return Err(invalid("unexpected tool name"));
    }
    let input: GoalOpenProposal = serde_json::from_str(&call.arguments_json)
        .map_err(|error| invalid(format!("invalid JSON: {error}")))?;
    if input.goal_text.trim().is_empty() || input.goal_text.len() > 16 * 1024 {
        return Err(invalid("goal_text is empty or too long"));
    }
    if input
        .previous_completed_goal_id
        .as_deref()
        .is_some_and(|id| id.trim().is_empty() || id.len() > 256)
    {
        return Err(invalid("previous_completed_goal_id is invalid"));
    }
    Ok(input)
}

pub fn registry() -> Vec<RegisteredTool> {
    vec![RegisteredTool {
        spec: ToolSpec {
            name: CONTROL_GOAL_TOOL_NAME.into(),
            description: "End the current long-running goal segment. Choose continue with a factual progress note and next step, wait for a real pending approval/work item or missing user input, complete with verifiable evidence references, or blocked with a concrete reason. For wait reason user, put the exact question for the owner in reference_id; the goal stops until the owner replies. This only changes planning state; it grants no device authority. Call it alone, after all earlier tool results for this segment are saved.".into(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "decision": {"type": "string", "enum": ["continue", "wait", "complete", "blocked"]},
                    "progress": {"type": "string", "maxLength": 2048},
                    "next_step": {"type": "string", "maxLength": 2048},
                    "reason": {"type": "string", "maxLength": 2048},
                    "reference_id": {"type": "string", "maxLength": 2048},
                    "evidence_ids": {"type": "array", "maxItems": 32, "items": {"type": "string", "maxLength": 2048}},
                    "summary": {"type": "string", "maxLength": 2048}
                },
                "required": ["decision"],
                "additionalProperties": false
            }),
        },
        required_capability: Capability::SystemInfo,
        effect: ToolEffect::GoalControl,
    }]
}

pub fn parse(call: &ToolCall) -> Result<GoalControl, AgentError> {
    if call.name != CONTROL_GOAL_TOOL_NAME {
        return Err(invalid("unexpected tool name"));
    }
    let control: GoalControl = serde_json::from_str(&call.arguments_json)
        .map_err(|error| invalid(format!("invalid JSON: {error}")))?;
    control
        .validate()
        .map_err(|_| invalid("invalid goal decision or arguments"))?;
    Ok(control)
}

pub(crate) fn invalid(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

pub fn unavailable() -> AgentError {
    AgentError {
        kind: AgentErrorKind::UnsupportedCapability,
        message: "Goal opening is unavailable on this surface".into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_one_closed_goal_decision() {
        let call = |arguments_json: &str| ToolCall {
            id: "call".into(),
            name: CONTROL_GOAL_TOOL_NAME.into(),
            arguments_json: arguments_json.into(),
        };
        assert!(
            parse(&call(
                r#"{"decision":"continue","progress":"Saved the file","next_step":"Inspect it"}"#
            ))
            .is_ok()
        );
        assert!(parse(&call(r#"{"decision":"continue","progress":"Saved the file","next_step":"Inspect it","grant_id":"x"}"#)).is_err());
        assert!(
            parse(&call(
                r#"{"decision":"complete","summary":"Done","evidence_ids":[]}"#
            ))
            .is_err()
        );
    }

    #[test]
    fn goal_open_proposal_accepts_a_bounded_previous_completed_id() {
        let call = |arguments_json: &str| ToolCall {
            id: "call".into(),
            name: REQUEST_GOAL_TOOL_NAME.into(),
            arguments_json: arguments_json.into(),
        };
        let proposal = parse_open(&call(
            r#"{"goal_text":"Finish the report","previous_completed_goal_id":"goal-1"}"#,
        ))
        .unwrap();
        assert_eq!(
            proposal.previous_completed_goal_id.as_deref(),
            Some("goal-1")
        );
        assert!(
            parse_open(&call(
                r#"{"goal_text":"Finish the report","previous_completed_goal_id":" "}"#
            ))
            .is_err()
        );
        assert!(parse_open(&call(r#"{"goal_text":"Finish the report","unknown":1}"#)).is_err());
    }
}
