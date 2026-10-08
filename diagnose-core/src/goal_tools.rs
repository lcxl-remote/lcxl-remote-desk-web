//! Built-in planning control for one claimed AI Assistant goal segment.

use desk_agent_protocol::{AgentError, AgentErrorKind, Capability};
use serde_json::json;

use crate::chat::{ToolCall, ToolSpec};
use crate::goal::{GoalControl, GoalWaitReason};
use crate::registry::{RegisteredTool, ToolEffect};

pub const CONTROL_GOAL_TOOL_NAME: &str = "control_goal";
pub const REQUEST_GOAL_TOOL_NAME: &str = "request_goal";
/// JSON Schema counts Unicode characters; four UTF-8 bytes each still fit the
/// existing 2 KiB persistent control fields.
pub const MAX_CONTROL_MESSAGE_CHARS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalDecision {
    Continue,
    Complete,
    Blocked,
    AskUser,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalControlProposal {
    pub decision: GoalDecision,
    pub message: String,
}

impl GoalControlProposal {
    pub fn into_control(self, evidence_ids: Vec<String>) -> Result<GoalControl, AgentError> {
        let control = match self.decision {
            GoalDecision::Continue => GoalControl::Continue {
                progress: self.message.clone(),
                next_step: self.message,
            },
            GoalDecision::Complete => GoalControl::Complete {
                evidence_ids,
                summary: self.message,
            },
            GoalDecision::Blocked => GoalControl::Blocked {
                reason: self.message,
            },
            GoalDecision::AskUser => GoalControl::Wait {
                reason: GoalWaitReason::User,
                reference_id: self.message,
            },
        };
        control
            .validate()
            .map_err(|_| invalid("invalid goal control binding"))?;
        Ok(control)
    }
}

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
    if input.goal_text.trim().is_empty() || input.goal_text.chars().count() > 16 * 1024 {
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
            description: "End the current long-running goal segment using exactly decision and message. Choose continue and describe factual progress and the next step; complete and explain the verified result; blocked and explain the concrete blocker; or ask_user and write the exact question for the owner. The server binds completion sources and handles pending approvals and work automatically. ask_user stops the goal until the owner replies. This changes planning state and grants no device authority. Call it alone after earlier tool results are saved. Example: {\"decision\":\"complete\",\"message\":\"Read 6+8=14 from the calculator.\"}".into(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "decision": {"type": "string", "enum": ["continue", "complete", "blocked", "ask_user"]},
                    "message": {"type": "string", "minLength": 1, "maxLength": MAX_CONTROL_MESSAGE_CHARS}
                },
                "required": ["decision", "message"],
                "additionalProperties": false
            }),
        },
        required_capability: Capability::SystemInfo,
        effect: ToolEffect::GoalControl,
    }]
}

pub fn parse(call: &ToolCall) -> Result<GoalControlProposal, AgentError> {
    if call.name != CONTROL_GOAL_TOOL_NAME {
        return Err(invalid("unexpected tool name"));
    }
    let control: GoalControlProposal = serde_json::from_str(&call.arguments_json)
        .map_err(|error| invalid(format!("invalid JSON: {error}")))?;
    if control.message.trim().is_empty()
        || control.message.chars().count() > MAX_CONTROL_MESSAGE_CHARS
    {
        return Err(invalid(
            "message must contain 1 to 512 non-blank characters",
        ));
    }
    Ok(control)
}

/// Only persisted observations may use the historical contract. Never use this
/// compatibility reader to validate a new model invocation.
pub fn recorded_decision(call: &ToolCall) -> Result<GoalDecision, AgentError> {
    if let Ok(proposal) = parse(call) {
        return Ok(proposal.decision);
    }
    if call.name != CONTROL_GOAL_TOOL_NAME {
        return Err(invalid("unexpected tool name"));
    }
    let control: GoalControl = serde_json::from_str(&call.arguments_json)
        .map_err(|_| invalid("invalid recorded goal decision"))?;
    control
        .validate()
        .map_err(|_| invalid("invalid recorded goal control"))?;
    Ok(match control {
        GoalControl::Continue { .. } => GoalDecision::Continue,
        GoalControl::Complete { .. } => GoalDecision::Complete,
        GoalControl::Blocked { .. } => GoalDecision::Blocked,
        GoalControl::Wait { .. } => GoalDecision::AskUser,
    })
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

    fn call(value: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call".into(),
            name: CONTROL_GOAL_TOOL_NAME.into(),
            arguments_json: value.to_string(),
        }
    }

    #[test]
    fn all_decisions_share_exactly_two_required_fields() {
        let spec = &registry()[0].spec.parameters_schema;
        assert_eq!(spec["required"], json!(["decision", "message"]));
        assert_eq!(spec["properties"].as_object().unwrap().len(), 2);
        for decision in ["continue", "complete", "blocked", "ask_user"] {
            let proposal = parse(&call(
                json!({"decision": decision, "message": "下一步：验证结果"}),
            ))
            .unwrap();
            proposal
                .into_control(vec!["server-response".into()])
                .unwrap()
                .validate()
                .unwrap();
            assert!(parse(&call(json!({"decision": decision}))).is_err());
            assert!(parse(&call(json!({"decision": decision, "message": "  "}))).is_err());
            assert!(
                parse(&call(
                    json!({"decision": decision, "message": "Done", "reason": "extra"})
                ))
                .is_err()
            );
        }
        assert!(parse(&call(json!({"decision": "wait", "message": "why"}))).is_err());
    }

    #[test]
    fn unicode_limit_matches_schema_and_persistent_utf8_bounds() {
        for scalar in ["中", "😀"] {
            let proposal = parse(&call(json!({"decision": "ask_user", "message": scalar.repeat(MAX_CONTROL_MESSAGE_CHARS)}))).unwrap();
            proposal.into_control(vec![]).unwrap().validate().unwrap();
            assert!(parse(&call(json!({"decision": "ask_user", "message": scalar.repeat(MAX_CONTROL_MESSAGE_CHARS + 1)}))).is_err());
        }
        assert_eq!(
            registry()[0].spec.parameters_schema["properties"]["message"]["maxLength"],
            json!(MAX_CONTROL_MESSAGE_CHARS)
        );
    }

    #[test]
    fn old_records_are_read_without_accepting_old_model_input() {
        let old = call(
            json!({"decision": "complete", "summary": "Done", "evidence_ids": ["original-source"]}),
        );
        assert!(parse(&old).is_err());
        assert_eq!(recorded_decision(&old).unwrap(), GoalDecision::Complete);
        assert!(
            parse(&call(json!({"decision": "complete", "message": "Done"})))
                .unwrap()
                .into_control(vec![])
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
