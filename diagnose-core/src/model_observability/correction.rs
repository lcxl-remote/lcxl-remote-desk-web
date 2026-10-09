//! Content-free correction facts resolved by the isolated observation writer.

use super::{
    Attribution, ObservationAlias, ObservationContext, ObservationEvent, ObservationPhase,
    ObservationRelation,
    tool::{ToolBatch, now_ms},
};
use crate::{
    chat::{ChatMessage, ChatRole},
    session::PersistedAgentSession,
};
use serde::{Deserialize, Serialize};

fn bounded_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 192
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorrectionGoal {
    pub goal_id: String,
    pub source_message_id: String,
    pub segment_seq: u32,
    pub goal_revision: u64,
}

/// Lease ownership is deliberately separate from the input's content identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorrectionFence {
    pub input_revision: u64,
    pub focus_revision: u64,
    pub source_input_id: String,
    pub goal: Option<CorrectionGoal>,
    pub provider_id: String,
    pub model_id: String,
    pub configuration_revision: String,
    pub contract_revision: String,
}

impl CorrectionFence {
    pub fn new(context: &ObservationContext, session: &PersistedAgentSession) -> Option<Self> {
        if session.conversation.len() > 4_096 {
            return None;
        }
        let source_input_id =
            crate::permission_resume::latest_user_requirement(&session.conversation)?
                .message_id
                .clone();
        let fence = Self {
            input_revision: session.input_revision,
            focus_revision: session.focus_epoch.input_revision,
            source_input_id,
            goal: session
                .focus_epoch
                .goal_segment
                .as_ref()
                .map(|goal| CorrectionGoal {
                    goal_id: goal.goal_id.clone(),
                    source_message_id: goal.source_message_id.clone(),
                    segment_seq: goal.segment_seq,
                    goal_revision: goal.goal_revision,
                }),
            provider_id: context.attribution.provider_id.clone(),
            model_id: context.attribution.model_id.clone(),
            configuration_revision: context.attribution.configuration_revision.clone(),
            contract_revision: context.attribution.contract_revision.clone(),
        };
        fence.is_bounded().then_some(fence)
    }
    pub fn is_bounded(&self) -> bool {
        bounded_id(&self.source_input_id)
            && [
                &self.provider_id,
                &self.model_id,
                &self.configuration_revision,
                &self.contract_revision,
            ]
            .into_iter()
            .all(|value| bounded_id(value))
            && self
                .goal
                .as_ref()
                .is_none_or(|goal| bounded_id(&goal.goal_id) && bounded_id(&goal.source_message_id))
    }
    pub fn same_input(&self, other: &Self) -> bool {
        self.input_revision == other.input_revision
            && self.focus_revision == other.focus_revision
            && self.source_input_id == other.source_input_id
            && self.goal == other.goal
            && self.contract_revision == other.contract_revision
    }
    pub fn same_model(&self, other: &Self) -> bool {
        self.provider_id == other.provider_id
            && self.model_id == other.model_id
            && self.configuration_revision == other.configuration_revision
    }
    pub fn matches_attribution(&self, attribution: &Attribution) -> bool {
        self.provider_id == attribution.provider_id
            && self.model_id == attribution.model_id
            && self.configuration_revision == attribution.configuration_revision
            && self.contract_revision == attribution.contract_revision
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorrectionCandidate {
    pub object_id: String,
    pub tool_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "result",
    content = "candidate",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum CorrectionResponse {
    Returned(Option<CorrectionCandidate>),
    NotComparable,
    NoResponse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fact", rename_all = "snake_case", deny_unknown_fields)]
pub enum CorrectionFact {
    Feedback {
        fence: CorrectionFence,
    },
    Projected {
        fence: CorrectionFence,
        next_call_id: String,
    },
    Response {
        fence: CorrectionFence,
        next_call_id: String,
        response: CorrectionResponse,
    },
}

impl CorrectionFact {
    pub fn is_bounded(&self) -> bool {
        match self {
            Self::Feedback { fence } => fence.is_bounded(),
            Self::Projected {
                fence,
                next_call_id,
            } => fence.is_bounded() && bounded_id(next_call_id),
            Self::Response {
                fence,
                next_call_id,
                response,
            } => {
                fence.is_bounded()
                    && bounded_id(next_call_id)
                    && match response {
                        CorrectionResponse::Returned(Some(candidate)) => {
                            candidate.object_id == format!("{next_call_id}.tool.0")
                                && bounded_id(&candidate.object_id)
                                && !candidate.tool_key.is_empty()
                                && candidate.tool_key.len() <= 96
                                && candidate
                                    .tool_key
                                    .bytes()
                                    .all(|b| b.is_ascii_alphanumeric() || b"_.".contains(&b))
                        }
                        _ => true,
                    }
            }
        }
    }
}

/// Select metadata only from the actual outgoing projection. A later owner
/// input, assistant response or summary prevents reuse of an earlier feedback.
pub fn projected_feedback(messages: &[ChatMessage]) -> Option<ObservationAlias> {
    if messages.len() > 4_096 {
        return None;
    }
    let mut results = Vec::new();
    for message in messages.iter().rev() {
        match message.role {
            ChatRole::System | ChatRole::SystemEvent => {}
            ChatRole::Tool => {
                if results.len() >= 256 {
                    return None;
                }
                results.push(message.tool_call_id.as_deref()?);
            }
            ChatRole::User if crate::permission_resume::is_permission_resume_message(message) => {}
            ChatRole::Assistant => {
                let [call] = message.tool_calls.as_slice() else {
                    return None;
                };
                if results.len() != 1 || results[0] != call.id.as_str() {
                    return None;
                }
                return ObservationAlias::source_message(&message.message_id, 0);
            }
            _ => return None,
        }
    }
    None
}

/// The guard owns no durable state and performs no storage read. Its result
/// refers to the first projected request, even if that request is retried later.
pub struct CorrectionProjection {
    context: ObservationContext,
    source: ObservationAlias,
    fence: CorrectionFence,
    responded: bool,
    model_returned: bool,
}

impl CorrectionProjection {
    pub fn new(
        context: Option<ObservationContext>,
        fence: Option<CorrectionFence>,
        messages: &[ChatMessage],
    ) -> Option<Self> {
        let context = context?;
        let fence = fence?;
        let source = projected_feedback(messages)?;
        let value = Self {
            context,
            source,
            fence,
            responded: false,
            model_returned: false,
        };
        value.emit(
            "projected",
            CorrectionFact::Projected {
                fence: value.fence.clone(),
                next_call_id: value.context.call_id.clone(),
            },
        );
        Some(value)
    }
    fn emit(&self, suffix: &str, fact: CorrectionFact) {
        let mut event = ObservationEvent::deferred_tool(
            self.source.clone(),
            ObservationPhase::Stage,
            0,
            now_ms(),
            std::collections::BTreeMap::new(),
            super::PermissionOutcome::NotReached,
            None,
            super::InputIssue::None,
        );
        let id = format!("{}.correction.{suffix}", self.context.call_id);
        event.event_id = id.clone();
        event.object_id = id;
        event.relation = Some(ObservationRelation::Correction {
            source: self.source.clone(),
            fact,
        });
        self.context.emit_related(
            event.object_id,
            event.phase,
            event.sequence,
            event.occurred_at_ms,
            event.payload,
            event.relation,
        );
    }
    pub fn response(&mut self, batch: &ToolBatch, comparable: bool) {
        if self.responded {
            return;
        }
        self.responded = true;
        let response = if comparable {
            CorrectionResponse::Returned(batch.correction_candidate())
        } else {
            CorrectionResponse::NotComparable
        };
        self.emit(
            "response",
            CorrectionFact::Response {
                fence: self.fence.clone(),
                next_call_id: self.context.call_id.clone(),
                response,
            },
        );
    }
    pub fn model_returned(&mut self) {
        self.model_returned = true;
    }
}

impl Drop for CorrectionProjection {
    fn drop(&mut self) {
        if !self.responded {
            let response = if self.model_returned {
                CorrectionResponse::NotComparable
            } else {
                CorrectionResponse::NoResponse
            };
            self.emit(
                "response",
                CorrectionFact::Response {
                    fence: self.fence.clone(),
                    next_call_id: self.context.call_id.clone(),
                    response,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use crate::chat::{ToolCall, ToolCallRef, ToolSpec};
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
    };

    #[derive(Default)]
    struct Recorder(Mutex<Vec<ObservationEvent>>);
    impl ObservabilitySeam for Recorder {
        fn submit(&self, event: ObservationEvent) {
            self.0.lock().unwrap().push(event);
        }
    }
    fn context(recorder: Arc<Recorder>, id: &str) -> ObservationContext {
        ObservationContext::new(
            id.into(),
            1_000,
            Attribution {
                provider_id: "provider".into(),
                model_id: "model".into(),
                model_name: "Model".into(),
                configuration_revision: "1".into(),
                contract_revision: "1".into(),
                surface: Surface::Assistant,
                purpose: Purpose::Agent,
                origin: Origin::User,
                configuration_scope: ConfigurationScope::Local,
                protocol: Protocol::OpenAiChatCompletions,
            },
            recorder,
        )
    }
    fn calls() -> Vec<ToolCall> {
        vec![ToolCall {
            id: "private-provider-call".into(),
            name: "server_tool".into(),
            arguments_json: "{\"private\":\"value\"}".into(),
        }]
    }
    fn definitions() -> BTreeMap<String, ToolSpec> {
        BTreeMap::from([(
            "server_tool".into(),
            ToolSpec {
                name: "server_tool".into(),
                description: "description".into(),
                parameters_schema: serde_json::json!({"type":"object"}),
            },
        )])
    }
    fn messages() -> Vec<ChatMessage> {
        vec![
            ChatMessage::text("owner-input", ChatRole::User, "private owner requirement"),
            ChatMessage::assistant_tool_calls(
                "server-message",
                "private assistant text",
                vec![ToolCallRef {
                    id: "private-provider-call".into(),
                    name: "server_tool".into(),
                    arguments_json: "{\"private\":\"value\"}".into(),
                }],
            ),
            ChatMessage::tool_result(
                "server-feedback",
                "private-provider-call",
                "private rejected input details",
            ),
        ]
    }
    fn session() -> PersistedAgentSession {
        let mut session = PersistedAgentSession::new(
            "conversation",
            "actor",
            "device",
            1,
            desk_agent_protocol::AgentScope {
                granted: vec![],
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-10-08T00:00:00Z",
        );
        session.conversation = messages();
        session.input_revision = 1;
        session.focus_epoch.input_revision = 1;
        session
    }

    #[test]
    fn projection_uses_actual_message_shape_and_does_not_guess_from_text() {
        let expected = ObservationAlias::source_message("server-message", 0).unwrap();
        let base = messages();
        assert_eq!(projected_feedback(&base), Some(expected));
        let mut without_feedback = base.clone();
        without_feedback.pop();
        assert_eq!(projected_feedback(&without_feedback), None);
        let mut duplicate = base.clone();
        duplicate.push(base.last().unwrap().clone());
        assert_eq!(projected_feedback(&duplicate), None);
        let mut wrong = base.clone();
        wrong.last_mut().unwrap().tool_call_id = Some("other-call".into());
        assert_eq!(projected_feedback(&wrong), None);
        for role in [
            ChatRole::User,
            ChatRole::Assistant,
            ChatRole::ContextSummary,
            ChatRole::UntrustedOutput,
        ] {
            let mut next = base.clone();
            next.push(ChatMessage::text("new-message", role, "same private text"));
            assert_eq!(projected_feedback(&next), None);
        }
        let mut many = base;
        let call = many[1].tool_calls[0].clone();
        many[1].tool_calls.push(call);
        assert_eq!(projected_feedback(&many), None);
    }

    #[test]
    fn scope_survives_restore_and_lease_change_but_not_new_input_or_contract() {
        let recorder = Arc::new(Recorder::default());
        let context = context(recorder, "call");
        let mut original = session();
        original.focus_epoch.goal_segment = Some(crate::focus_epoch::GoalSegmentIdentity {
            goal_id: "goal".into(),
            source_message_id: "goal-message".into(),
            segment_seq: 3,
            goal_revision: 8,
            lease_epoch: 4,
        });
        let fence = CorrectionFence::new(&context, &original).unwrap();
        let mut restored: PersistedAgentSession =
            serde_json::from_str(&serde_json::to_string(&original).unwrap()).unwrap();
        restored
            .focus_epoch
            .goal_segment
            .as_mut()
            .unwrap()
            .lease_epoch = 9;
        assert_eq!(
            CorrectionFence::new(&context, &restored),
            Some(fence.clone())
        );
        restored.input_revision += 1;
        assert!(!fence.same_input(&CorrectionFence::new(&context, &restored).unwrap()));
        let mut changed = fence.clone();
        changed.contract_revision = "2".into();
        assert!(!fence.same_input(&changed));
        changed = fence.clone();
        changed.model_id = "another-model".into();
        assert!(fence.same_input(&changed));
        assert!(!fence.same_model(&changed));
        changed = fence;
        changed.source_input_id = "private/raw/input".into();
        assert!(!changed.is_bounded());
    }

    #[test]
    fn feedback_and_response_are_content_free_facts_with_no_process_local_lineage() {
        let recorder = Arc::new(Recorder::default());
        let original_context = context(recorder.clone(), "original");
        let session = session();
        let fence = CorrectionFence::new(&original_context, &session).unwrap();
        let original = ToolBatch::new(Some(original_context), &calls(), &definitions());
        original.bind_message("server-message");
        original.reject(0, Stage::Schema, InputIssue::Type, None);
        original.correction_feedback(Some(fence.clone()));
        drop(original);
        let next_context = context(recorder.clone(), "next");
        let mut projection =
            CorrectionProjection::new(Some(next_context.clone()), Some(fence), &messages())
                .unwrap();
        let next = ToolBatch::new(Some(next_context), &calls(), &definitions());
        projection.model_returned();
        projection.response(&next, true);
        drop(projection);
        let events = recorder.0.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    &event.relation,
                    Some(ObservationRelation::Correction {
                        fact: CorrectionFact::Feedback { .. },
                        ..
                    })
                ))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    &event.relation,
                    Some(ObservationRelation::Correction {
                        fact: CorrectionFact::Response { .. },
                        ..
                    })
                ))
                .count(),
            1
        );
        assert!(events.iter().all(ObservationEvent::is_bounded));
        let encoded = serde_json::to_string(&*events).unwrap();
        for private in [
            "private-provider-call",
            "private owner requirement",
            "private assistant text",
            "private rejected input details",
            "value",
        ] {
            assert!(!encoded.contains(private));
        }
        assert!(
            events
                .iter()
                .filter_map(|event| match &event.payload {
                    ObservationPayload::Tool(tool) => Some(tool),
                    _ => None,
                })
                .all(|tool| tool.correction_of.is_none())
        );
    }

    #[test]
    fn mixed_feedback_and_plain_loop_drop_cannot_invent_a_missing_response() {
        let recorder = Arc::new(Recorder::default());
        let context = context(recorder.clone(), "original");
        let fence = CorrectionFence::new(&context, &session()).unwrap();
        let mut two = calls();
        two.push(two[0].clone());
        let batch = ToolBatch::new(Some(context), &two, &definitions());
        batch.bind_message("server-message");
        batch.reject(0, Stage::Schema, InputIssue::Type, None);
        batch.correction_feedback(Some(fence));
        drop(batch);
        assert!(!recorder.0.lock().unwrap().iter().any(|event| matches!(
            &event.relation,
            Some(ObservationRelation::Correction { .. })
        )));
    }

    #[test]
    fn projection_drop_distinguishes_unreturned_requests_from_rejected_outputs() {
        for returned in [false, true] {
            let recorder = Arc::new(Recorder::default());
            let context = context(recorder.clone(), "next");
            let fence = CorrectionFence::new(&context, &session()).unwrap();
            let mut projection =
                CorrectionProjection::new(Some(context), Some(fence), &messages()).unwrap();
            if returned {
                projection.model_returned();
            }
            drop(projection);
            let events = recorder.0.lock().unwrap();
            let Some(ObservationRelation::Correction {
                fact: CorrectionFact::Response { response, .. },
                ..
            }) = &events.last().unwrap().relation
            else {
                panic!("response fact required");
            };
            assert_eq!(
                *response,
                if returned {
                    CorrectionResponse::NotComparable
                } else {
                    CorrectionResponse::NoResponse
                }
            );
        }
    }

    #[test]
    fn a_panicking_metrics_sink_cannot_interrupt_feedback_or_projection_cleanup() {
        struct Panicking;
        impl ObservabilitySeam for Panicking {
            fn submit(&self, _event: ObservationEvent) {
                panic!("observation writer unavailable");
            }
        }
        let base = context(Arc::new(Recorder::default()), "original");
        let context = ObservationContext::new(
            base.call_id,
            base.started_at_ms,
            base.attribution,
            Arc::new(Panicking),
        );
        let fence = CorrectionFence::new(&context, &session()).unwrap();
        let batch = ToolBatch::new(Some(context.clone()), &calls(), &definitions());
        batch.bind_message("server-message");
        batch.reject(0, Stage::Schema, InputIssue::Type, None);
        batch.correction_feedback(Some(fence.clone()));
        let projection = CorrectionProjection::new(Some(context), Some(fence), &messages());
        assert!(projection.is_some());
        drop(projection);
    }
}
