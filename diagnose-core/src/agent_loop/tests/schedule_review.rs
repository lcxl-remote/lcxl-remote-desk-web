use super::*;
use std::cell::Cell;

struct ReviewStore<'a> {
    mem: &'a MemSession,
    requests: Rc<RefCell<Vec<ModelRequest>>>,
    polls: Cell<usize>,
    decision: &'static str,
    cancelled: &'a Cell<bool>,
}
#[async_trait(?Send)]
impl SessionSeam for ReviewStore<'_> {
    async fn claim_turn(
        &self,
        params: ClaimTurnParams,
    ) -> Result<PersistedAgentSession, ClaimError> {
        let mut session = self.mem.claim_turn(params).await?;
        session.surface = AgentSessionSurface::DeviceAssistant;
        Ok(session)
    }
    async fn save(&self, session: &mut PersistedAgentSession) -> Result<(), AgentError> {
        self.mem.save(session).await
    }
    async fn latest_input_revision(&self, id: &str) -> Result<Option<u64>, AgentError> {
        self.mem.latest_input_revision(id).await
    }
    async fn manage_schedule_tool(
        &self,
        session: &mut PersistedAgentSession,
        call: &ToolCall,
    ) -> Result<String, AgentError> {
        let parent = crate::model_message_labels::model_bound_user_message(
            "model-input".into(),
            "timer".into(),
            desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: "gateway".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
        )
        .unwrap()
        .data_envelope;
        append_internal_tool_result(
            session,
            parent.as_ref(),
            "draft".into(),
            &call.id,
            r#"{"schedule_id":"timer","state":"pending_review","kind":"conversation_resume","awaiting_confirmation":true}"#.into(),
            "schedule_proposal",
        )?;
        session.pending_schedule_review = Some("timer".into());
        self.mem.save(session).await?;
        Ok("draft".into())
    }
    async fn poll_schedule_review(
        &self,
        session: &mut PersistedAgentSession,
        id: &str,
    ) -> Result<bool, AgentError> {
        assert_eq!(id, "timer");
        assert_eq!(
            self.requests.borrow().len(),
            1,
            "no model call while awaiting the owner"
        );
        assert!(session.turn_state.is_active());
        let count = self.polls.get() + 1;
        self.polls.set(count);
        if self.decision == "stop" {
            self.cancelled.set(true);
            return Ok(false);
        }
        if count < 3 {
            return Ok(false);
        }
        let text = serde_json::json!({"event": self.decision, "schedule_id": id, "state": if self.decision == "scheduled_task_activated" { "active" } else { "deleted" }, "owner_decision": if self.decision == "scheduled_task_activated" { "approved" } else { "rejected" }}).to_string();
        let mut message = ChatMessage::system_event("decision", &text);
        message.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
            session.conversation.last().unwrap().data_envelope.as_ref(),
            "decision",
            &text,
            "schedule_activation",
        )?;
        session.conversation.push(message);
        self.mem.save(session).await?;
        Ok(true)
    }
}
struct ReviewHeartbeat<'a>(&'a Cell<bool>);
struct ReviewGuard;
impl crate::seam::HeartbeatGuard for ReviewGuard {}
impl crate::seam::LeaseHeartbeat for ReviewHeartbeat<'_> {
    fn is_healthy(&self) -> bool {
        !self.0.get()
    }
    fn start(&self, _: String, _: u64) -> Box<dyn crate::seam::HeartbeatGuard> {
        Box::new(ReviewGuard)
    }
}

#[tokio::test]
async fn model_waits_for_explicit_schedule_decision_and_stop_interrupts_wait() {
    for decision in [
        "scheduled_task_activated",
        "scheduled_task_rejected",
        "stop",
    ] {
        let mem = MemSession::default();
        let requests = Rc::new(RefCell::new(vec![]));
        let cancelled = Cell::new(false);
        let store = ReviewStore {
            mem: &mem,
            requests: requests.clone(),
            polls: Cell::new(0),
            decision,
            cancelled: &cancelled,
        };
        let model = ScriptModel {
            turns: RefCell::new(
                [
                    tool_use("create", crate::schedule::proposal::REQUEST_SCHEDULE),
                    answer("decision received"),
                ]
                .into(),
            ),
            requests: requests.clone(),
        };
        let tools = RecordingTools {
            calls: Rc::new(RefCell::new(vec![])),
            reply: "unused".into(),
        };
        let registry = crate::schedule::proposal::registry();
        let clock = || "2026-09-09T00:00:00Z".to_string();
        let heartbeat = ReviewHeartbeat(&cancelled);
        let mut deps = deps(&mem, &model, &tools, &registry, &clock);
        deps.session_seam = &store;
        deps.heartbeat = Some(&heartbeat);
        let result = run_agent_turn(
            &deps,
            claim(),
            ChatMessage::text("user", ChatRole::User, "remind me in one minute"),
            &mut NullTurnSink,
        )
        .await;
        if decision == "stop" {
            assert!(result.is_err());
            assert_eq!(requests.borrow().len(), 1);
            assert_eq!(store.polls.get(), 1);
        } else {
            assert!(matches!(result.unwrap(), LoopOutcome::Answered(_)));
            assert_eq!(store.polls.get(), 3);
            assert_eq!(requests.borrow().len(), 2);
            let projected_requests = requests.borrow();
            let result = projected_requests[1]
                .messages
                .iter()
                .find(|m| m.tool_call_id.as_deref() == Some("create"))
                .unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&result.text).unwrap()["review_complete"],
                true
            );
            assert!(
                mem.inner
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .conversation
                    .iter()
                    .any(|m| m.role == ChatRole::Tool && m.text.contains("pending_review"))
            );
            drop(projected_requests);
            assert!(
                requests.borrow()[1]
                    .messages
                    .iter()
                    .any(|m| m.text.contains(decision))
            );
        }
        assert!(
            mem.inner
                .borrow()
                .as_ref()
                .unwrap()
                .pending_schedule_review
                .is_none()
        );
        assert!(tools.calls.borrow().is_empty());
    }
}
