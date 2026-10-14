//! Frozen server observations for model controls; never model-authored versions.
use crate::{
    chat::{ChatMessage, ChatRole, ToolCall},
    model_egress::ModelInputLineage,
    session::PersistedAgentSession,
};
use desk_agent_protocol::AgentError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlObservation {
    pub id: String,
    pub input_revision: u64,
    pub control_revision: u64,
    pub schedule_revision: i64,
    pub envelope_id: String,
    pub digest_sha256: String,
    pub provider: String,
    pub tool: String,
    pub priority: u8,
    pub message_id: String,
    pub terminal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenModelControls {
    pub conversation_id: String,
    pub actor_id: String,
    pub device_id: String,
    pub input_revision: u64,
    pub control_revision: u64,
    pub lease_token: u64,
    pub turn_id: Option<String>,
    pub observations: Vec<ControlObservation>,
}

fn unavailable() -> AgentError {
    crate::subagent::invalid(
        "The control has no unambiguous current observation in this model request. Query the task again before controlling it; no action was applied.",
    )
}

impl FrozenModelControls {
    pub fn capture(session: &PersistedAgentSession, messages: &[ChatMessage]) -> Self {
        let mut result = Self {
            conversation_id: session.conversation_id.clone(),
            actor_id: session.actor_id.clone(),
            device_id: session.device_id.clone(),
            input_revision: session.input_revision,
            control_revision: session.control_revision,
            lease_token: session.lease_token,
            turn_id: session.current_turn_id.clone(),
            observations: vec![],
        };
        for message in messages {
            let Some(label) = &message.data_envelope else {
                continue;
            };
            if label.provenance.source_provider_id != crate::dynamic_run::RUN_CONTROL_PROVIDER_ID
                || label.validate().is_err()
                || crate::model_egress::message_content_bytes(message)
                    .ok()
                    .is_none_or(|bytes| {
                        format!("{:x}", Sha256::digest(bytes)) != label.digest_sha256
                    })
            {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&message.text) else {
                continue;
            };
            let source = label.provenance.source_tool_name.as_str();
            let (tasks, priority, schedule) = match source {
                "delegation_state" if message.role == ChatRole::SystemEvent => (
                    value
                        .pointer("/runtime_delegation_state/tasks")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .map(|item| item.get("task").unwrap_or(item))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default(),
                    2,
                    false,
                ),
                crate::subagent::tools::SPAWN | crate::subagent::tools::STATUS
                    if message.role == ChatRole::Tool =>
                {
                    (vec![&value], 1, false)
                }
                crate::subagent::tools::LIST if message.role == ChatRole::Tool => (
                    value["items"]
                        .as_array()
                        .map(|items| items.iter().collect())
                        .unwrap_or_default(),
                    1,
                    false,
                ),
                "schedule_query" if message.role == ChatRole::Tool => (
                    value["tasks"]
                        .as_array()
                        .map(|items| items.iter().collect())
                        .unwrap_or_default(),
                    1,
                    true,
                ),
                _ => continue,
            };
            for task in tasks {
                let Some(id) = task[if schedule { "schedule_id" } else { "task_id" }].as_str()
                else {
                    continue;
                };
                let (input, control, revision) = if schedule {
                    let Some(revision) = task["revision"].as_i64().filter(|n| *n > 0) else {
                        continue;
                    };
                    (0, 0, revision)
                } else {
                    let (Some(input), Some(control)) = (
                        task["input_revision"].as_u64().filter(|n| *n > 0),
                        task["control_revision"].as_u64().filter(|n| *n > 0),
                    ) else {
                        continue;
                    };
                    (input, control, 0)
                };
                result.observations.push(ControlObservation {
                    id: id.into(),
                    input_revision: input,
                    control_revision: control,
                    schedule_revision: revision,
                    envelope_id: label.envelope_id.clone(),
                    digest_sha256: label.digest_sha256.clone(),
                    provider: label.provenance.source_provider_id.clone(),
                    tool: source.into(),
                    priority,
                    message_id: message.message_id.clone(),
                    terminal: matches!(
                        task["state"].as_str(),
                        Some("completed" | "failed" | "cancelled" | "deleted" | "expired")
                    ),
                });
            }
        }
        result
    }

    pub fn retain_exported(
        &mut self,
        output: Option<&desk_agent_protocol::data_lineage::DataEnvelope>,
        authorization_id: Option<&str>,
    ) {
        self.observations.retain(|observation| {
            output.is_some_and(|output| {
                let direct = output
                    .provenance
                    .source_envelope_ids
                    .contains(&observation.envelope_id);
                let exported = authorization_id.is_some_and(|id| {
                    output.provenance.source_envelope_ids.contains(
                        &crate::model_egress::model_export_envelope_id(
                            id,
                            &observation.envelope_id,
                            &observation.message_id,
                        ),
                    )
                });
                direct || exported
            })
        });
    }

    pub fn retain_called_targets(&mut self, calls: &[ToolCall]) {
        let targets = calls
            .iter()
            .filter(|call| is_control(&call.name))
            .filter_map(|call| {
                let value: Value = serde_json::from_str(&call.arguments_json).ok()?;
                let schedule = call.name == crate::schedule::management_tools::CANCEL;
                value[if schedule { "schedule_id" } else { "task_id" }]
                    .as_str()
                    .map(|id| (id.to_owned(), schedule))
            })
            .collect::<Vec<_>>();
        self.observations.retain(|observation| {
            targets.iter().any(|(id, schedule)| {
                id == &observation.id && *schedule == (observation.schedule_revision > 0)
            })
        });
    }

    fn selected(&self, id: &str, schedule: bool) -> Result<&ControlObservation, AgentError> {
        let matching = self
            .observations
            .iter()
            .filter(|o| o.id == id && (o.schedule_revision > 0) == schedule)
            .collect::<Vec<_>>();
        let priority = matching
            .iter()
            .map(|o| o.priority)
            .max()
            .ok_or_else(unavailable)?;
        let mut selected = matching.into_iter().filter(|o| o.priority == priority);
        let first = selected.next().ok_or_else(unavailable)?;
        if selected.any(|o| {
            (o.input_revision, o.control_revision, o.schedule_revision)
                != (
                    first.input_revision,
                    first.control_revision,
                    first.schedule_revision,
                )
        }) {
            return Err(unavailable());
        }
        Ok(first)
    }

    pub fn validate_audit(
        &self,
        call: &ToolCall,
        inputs: &[ModelInputLineage],
    ) -> Result<(), AgentError> {
        let value: Value = serde_json::from_str(&call.arguments_json).map_err(|_| unavailable())?;
        let schedule = call.name == crate::schedule::management_tools::CANCEL;
        let id = value[if schedule { "schedule_id" } else { "task_id" }]
            .as_str()
            .ok_or_else(unavailable)?;
        let selected = self.selected(id, schedule)?;
        // An immediate export edge is acceptable. An ancestor of a summary is not.
        if !inputs.iter().any(|input| {
            input.digest_sha256 == selected.digest_sha256
                && input.source_provider_id == selected.provider
                && input.source_tool_name == selected.tool
                && (input.envelope_id == selected.envelope_id
                    || input.source_envelope_ids == [selected.envelope_id.clone()])
        }) {
            return Err(unavailable());
        }
        Ok(())
    }
}

pub fn is_control(name: &str) -> bool {
    matches!(
        name,
        crate::subagent::tools::CANCEL
            | crate::subagent::tools::MESSAGE
            | crate::schedule::management_tools::CANCEL
    )
}

pub fn caller<'a>(
    session: &'a PersistedAgentSession,
    call: &ToolCall,
) -> Result<&'a ChatMessage, AgentError> {
    let mut matching = session.conversation.iter().filter(|message| {
        message.role == ChatRole::Assistant
            && message.turn_id == session.current_turn_id
            && message.tool_calls.iter().any(|raw| {
                raw.id == call.id
                    && raw.name == call.name
                    && crate::subagent::tools::same_arguments(
                        &call.name,
                        &raw.arguments_json,
                        &call.arguments_json,
                    )
            })
    });
    let first = matching.next().ok_or_else(unavailable)?;
    if matching.next().is_some() {
        return Err(unavailable());
    }
    Ok(first)
}

pub fn observation<'a>(
    session: &'a PersistedAgentSession,
    call: &ToolCall,
    id: &str,
    schedule: bool,
) -> Result<&'a ControlObservation, AgentError> {
    let caller = caller(session, call)?;
    let binding = caller.model_controls.as_ref().ok_or_else(unavailable)?;
    if binding.conversation_id != session.conversation_id
        || binding.actor_id != session.actor_id
        || binding.device_id != session.device_id
        || binding.input_revision != session.input_revision
        || binding.control_revision != session.control_revision
        || binding.lease_token != session.lease_token
        || binding.turn_id != session.current_turn_id
    {
        return Err(unavailable());
    }
    let selected = binding.selected(id, schedule)?;
    if selected.terminal {
        return Err(crate::subagent::invalid(
            "This observed task is terminal and cannot be controlled. Read its result or create a new task for subsequent work; do not retry this control.",
        ));
    }
    Ok(selected)
}

/// Hide only the declared control metadata, leaving business task data intact.
pub(crate) fn project_message(message: &mut ChatMessage) {
    let Some(label) = &message.data_envelope else {
        return;
    };
    if label.provenance.source_provider_id != crate::dynamic_run::RUN_CONTROL_PROVIDER_ID {
        return;
    }
    let Ok(mut value) = serde_json::from_str::<Value>(&message.text) else {
        return;
    };
    fn task(value: &mut Value) {
        if let Some(object) = value.as_object_mut() {
            for key in ["input_revision", "control_revision", "state_revision"] {
                object.remove(key);
            }
        }
    }
    match label.provenance.source_tool_name.as_str() {
        "delegation_state" => {
            if let Some(items) = value
                .pointer_mut("/runtime_delegation_state/tasks")
                .and_then(Value::as_array_mut)
            {
                for item in items {
                    if item.get("task").is_some() {
                        task(&mut item["task"]);
                    } else {
                        task(item);
                    }
                }
            }
        }
        crate::subagent::tools::SPAWN
        | crate::subagent::tools::STATUS
        | crate::subagent::tools::RESULT
        | crate::subagent::tools::CANCEL
        | crate::subagent::tools::MESSAGE => task(&mut value),
        crate::subagent::tools::LIST => {
            if let Some(items) = value["items"].as_array_mut() {
                for item in items {
                    task(item)
                }
            }
        }
        crate::subagent::tools::WAIT => {
            if let Some(items) = value.get_mut("tasks").and_then(Value::as_array_mut) {
                for item in items {
                    task(item)
                }
            }
        }
        "schedule_query" => {
            if let Some(items) = value["tasks"].as_array_mut() {
                for item in items {
                    if let Some(object) = item.as_object_mut() {
                        object.remove("revision");
                    }
                }
            }
        }
        _ => return,
    }
    message.text = value.to_string();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent::tools;
    use desk_agent_protocol::{AgentScope, ExecutionMode};
    fn session() -> PersistedAgentSession {
        let scope = AgentScope {
            granted: vec![],
            mode: ExecutionMode::SuggestOnly,
            expires_at: None,
            policy_name: None,
        };
        let mut session = PersistedAgentSession::new(
            "chat",
            "owner",
            "device",
            1,
            scope.clone(),
            "2026-10-13T00:00:00Z",
        );
        session.surface = crate::session::AgentSessionSurface::AiAssistant;
        session.input_revision = 1;
        session
            .begin_turn("turn", None, None, 1, scope, "2026-10-13T00:00:00Z")
            .unwrap();
        session
    }
    fn input(revision: u64) -> ChatMessage {
        let owner = crate::model_message_labels::model_bound_user_message(
            "owner".into(),
            "Adjust this task".into(),
            desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: "gateway".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
        )
        .unwrap();
        crate::subagent::projection::runtime_message(&format!("observation-{revision}"), &serde_json::json!({"tasks":[{"task":{"task_id":"task","input_revision":revision,"control_revision":2,"state_revision":9,"state":"running"}}]}), &[owner.data_envelope.unwrap()]).unwrap()
    }
    fn commit(session: &mut PersistedAgentSession, input: &[ChatMessage], call: &ToolCall) {
        let mut caller = ChatMessage::assistant_tool_calls("reply", "", vec![call.to_ref()])
            .with_turn_id("turn");
        caller.model_controls = Some(Box::new(FrozenModelControls::capture(session, input)));
        session.conversation.push(caller);
    }
    #[test]
    fn controls_restore_only_this_request_and_reject_model_versions_and_conflicts() {
        let mut session = session();
        let call = ToolCall {
            id: "cancel".into(),
            name: tools::CANCEL.into(),
            arguments_json: r#"{"task_id":"task"}"#.into(),
        };
        assert!(tools::parse(&session, &call).is_err());
        let input = input(3);
        commit(&mut session, std::slice::from_ref(&input), &call);
        assert_eq!(
            tools::parse(&session, &call).unwrap(),
            tools::Operation::Cancel {
                task_id: "task".into(),
                input_revision: 3,
                control_revision: 2
            }
        );
        let raw = session.conversation[0].tool_calls[0].arguments_json.clone();
        let supplied = ToolCall {
            arguments_json:
                r#"{"task_id":"task","expected_input_revision":99,"expected_control_revision":2}"#
                    .into(),
            ..call.clone()
        };
        assert!(tools::parse(&session, &supplied).is_err());
        session.control_revision += 1;
        assert!(tools::parse(&session, &call).is_err());
        assert_eq!(session.conversation[0].tool_calls[0].arguments_json, raw);
        let conflict = FrozenModelControls::capture(&session, &[input, self::input(4)]);
        assert!(conflict.selected("task", false).is_err());
    }
    #[test]
    fn direct_audited_input_required_and_summary_ancestor_is_not_an_observation() {
        let session = session();
        let input = input(3);
        let binding = FrozenModelControls::capture(&session, std::slice::from_ref(&input));
        let call = ToolCall {
            id: "cancel".into(),
            name: tools::CANCEL.into(),
            arguments_json: r#"{"task_id":"task"}"#.into(),
        };
        let label = input.data_envelope.unwrap();
        let mut node = ModelInputLineage {
            envelope_id: label.envelope_id.clone(),
            digest_sha256: label.digest_sha256,
            source_provider_id: label.provenance.source_provider_id,
            source_tool_name: label.provenance.source_tool_name,
            source_envelope_ids: vec![],
            public_system_prompt: false,
        };
        binding.validate_audit(&call, &[node.clone()]).unwrap();
        assert!(binding.validate_audit(&call, &[]).is_err());
        node.envelope_id = "export".into();
        node.source_envelope_ids = vec![label.envelope_id];
        binding.validate_audit(&call, &[node.clone()]).unwrap();
        node.source_tool_name = "summary".into();
        assert!(binding.validate_audit(&call, &[node]).is_err());
        let spoof = ChatMessage::context_summary("summary", input.text);
        assert!(
            FrozenModelControls::capture(&session, &[spoof])
                .observations
                .is_empty()
        );
    }
    #[test]
    fn current_runtime_wins_over_old_list_and_output_versions_are_model_only() {
        let session = session();
        let runtime = input(3);
        let mut prior = input(1);
        prior.role = ChatRole::Tool;
        prior.text = serde_json::json!({"task_id":"task","input_revision":1,"control_revision":1,"state":"running"}).to_string();
        prior.data_envelope = Some(
            crate::subagent::projection::envelope(
                "prior",
                &prior.text,
                tools::STATUS,
                &[runtime.data_envelope.clone().unwrap()],
            )
            .unwrap(),
        );
        let binding = FrozenModelControls::capture(&session, &[runtime.clone(), prior]);
        assert_eq!(binding.selected("task", false).unwrap().input_revision, 3);
        let mut model = runtime.clone();
        project_message(&mut model);
        assert!(!model.text.contains("input_revision"));
        assert!(runtime.text.contains("input_revision"));
    }
}
