//! Bind completion to the current persisted response and its accepted inputs.

use super::*;
use crate::goal::GoalRun;

pub(super) fn completion_sources(
    session: &PersistedAgentSession,
    goal: &GoalRun,
    request_message_ids: &HashSet<String>,
    response_message_id: &str,
    registry: &[RegisteredTool],
) -> Result<Vec<String>, AgentError> {
    let segment = session
        .focus_epoch
        .goal_segment
        .as_ref()
        .ok_or_else(invalid_original_result)?;
    if goal.conversation_id != session.conversation_id
        || goal.owner_id != session.actor_id
        || goal.device_id != session.device_id
        || goal.input_revision != session.input_revision
        || goal.state != crate::goal::GoalState::Running
        || segment.goal_id != goal.goal_id
        || segment.source_message_id != goal.source_message_id
        || segment.goal_revision != goal.goal_revision
        || segment.segment_seq != goal.slice_seq
        || segment.lease_epoch != goal.lease_epoch
    {
        return Err(invalid_original_result());
    }
    let response = session
        .conversation
        .iter()
        .find(|message| {
            message.message_id == response_message_id
                && message.role == ChatRole::Assistant
                && message.turn_id == session.current_turn_id
        })
        .ok_or_else(invalid_original_result)?;
    let label = response
        .data_envelope
        .as_ref()
        .ok_or_else(invalid_original_result)?;
    label.validate().map_err(|_| invalid_original_result())?;
    // This persisted model output retains the validated transitive lineage of
    // ALL projected inputs, including compressed history and child reports.
    // A reference records provenance; it is not an assertion that a dispatch
    // or an approval proved the requested outcome.
    let mut ids = vec![response.message_id.clone()];
    for message in session.conversation.iter().rev() {
        if ids.len() == 32 {
            break;
        }
        if !request_message_ids.contains(&message.message_id) {
            continue;
        }
        let Some(input) = &message.data_envelope else {
            continue;
        };
        if !label
            .provenance
            .source_envelope_ids
            .contains(&input.envelope_id)
        {
            continue;
        }
        let original_input =
            message.role == ChatRole::User && message.message_id == goal.source_message_id;
        let read_result = message.role == ChatRole::Tool
            && message.turn_id == session.current_turn_id
            && message.tool_ok == Some(true)
            && message.raw_result.as_ref().is_none_or(|raw| raw.restorable)
            && message.tool_call_id.as_ref().is_some_and(|id| {
                session
                    .conversation
                    .iter()
                    .flat_map(|entry| &entry.tool_calls)
                    .any(|call| {
                        call.id == *id
                            && registry.iter().any(|tool| {
                                tool.name() == call.name && tool.effect == ToolEffect::ReadOnly
                            })
                    })
            });
        if original_input || read_result {
            ids.push(message.message_id.clone());
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::{GoalLimits, GoalModelBinding, GoalOpening};

    fn read_tool(name: &str, cap: desk_agent_protocol::Capability) -> RegisteredTool {
        RegisteredTool {
            spec: crate::chat::ToolSpec {
                name: name.into(),
                description: "read".into(),
                parameters_schema: serde_json::json!({"type":"object"}),
            },
            required_capability: cap,
            effect: ToolEffect::ReadOnly,
        }
    }
    #[test]
    fn sources_bind_current_goal_and_response_without_dispatch_or_approval_evidence() {
        let mut session = PersistedAgentSession::new(
            "run",
            "owner",
            "device",
            1,
            desk_agent_protocol::AgentScope {
                granted: vec![desk_agent_protocol::Capability::SystemInfo],
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-10-07T00:00:00Z",
        );
        session.surface = AgentSessionSurface::AiAssistant;
        session.input_revision = 1;
        session.focus_epoch.reset(1, []).unwrap();
        session.current_turn_id = Some("turn".into());
        let mut goal = GoalRun::new(
            "goal".into(),
            "run".into(),
            "owner".into(),
            "device".into(),
            "Calculate 6+8".into(),
            "input".into(),
            GoalOpening::OwnerRequest,
            GoalModelBinding {
                connection_id: "gateway".into(),
                connection_revision: 1,
                profile_revision: 1,
                model_id: "model".into(),
            },
            1,
            1000,
            GoalLimits::default(),
        )
        .unwrap();
        goal.claim_slice(1001).unwrap();
        session
            .begin_goal_segment(
                &goal.goal_id,
                &goal.source_message_id,
                goal.slice_seq,
                goal.goal_revision,
                goal.lease_epoch,
            )
            .unwrap();
        let destination = DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        };
        let input = crate::model_message_labels::model_bound_user_message(
            "input".into(),
            "Calculate 6+8".into(),
            destination.clone(),
        )
        .unwrap();
        session.conversation.push(input);
        let mut response = crate::model_message_labels::model_bound_user_message(
            "response".into(),
            "Done".into(),
            destination.clone(),
        )
        .unwrap();
        response.role = ChatRole::Assistant;
        response.turn_id = Some("turn".into());
        let mut sources = vec!["user-message-input".into()];
        for (id, name, turn) in [
            ("read", "inspect", "turn"),
            ("dispatch", "launch", "turn"),
            ("approval", "request_permission", "turn"),
            ("old", "inspect", "old-input-turn"),
        ] {
            session.conversation.push(ChatMessage::assistant_tool_calls(
                format!("call-{id}"),
                "",
                vec![crate::chat::ToolCallRef {
                    id: id.into(),
                    name: name.into(),
                    arguments_json: "{}".into(),
                }],
            ));
            let mut result = crate::model_message_labels::model_bound_user_message(
                format!("result-{id}"),
                "14".into(),
                destination.clone(),
            )
            .unwrap();
            result.role = ChatRole::Tool;
            result.turn_id = Some(turn.into());
            result.tool_call_id = Some(id.into());
            result.tool_ok = Some(true);
            sources.push(result.data_envelope.as_ref().unwrap().envelope_id.clone());
            session.conversation.push(result);
        }
        response
            .data_envelope
            .as_mut()
            .unwrap()
            .provenance
            .source_envelope_ids = sources;
        session.conversation.push(response);
        let mut registry = vec![read_tool(
            "inspect",
            desk_agent_protocol::Capability::SystemInfo,
        )];
        let mut launch = read_tool("launch", desk_agent_protocol::Capability::SystemInfo);
        launch.effect = ToolEffect::Mutating;
        registry.push(launch);
        let inputs = session
            .conversation
            .iter()
            .filter(|m| m.message_id != "response")
            .map(|m| m.message_id.clone())
            .collect();
        let bound = completion_sources(&session, &goal, &inputs, "response", &registry).unwrap();
        assert_eq!(bound, vec!["response", "result-read", "input"]);
        assert_eq!(
            completion_sources(&session, &goal, &HashSet::new(), "response", &registry).unwrap(),
            vec!["response"]
        );
        // Compressed history remains linked through the response envelope; an
        // omitted raw result is never invented as a visible input.
        goal.input_revision += 1;
        assert!(completion_sources(&session, &goal, &inputs, "response", &registry).is_err());
        goal.input_revision -= 1;
        goal.goal_id = "different-goal".into();
        assert!(completion_sources(&session, &goal, &inputs, "response", &registry).is_err());
    }
}
