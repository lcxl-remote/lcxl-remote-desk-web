use super::*;
use crate::{
    goal::GoalUsage,
    subagent::{
        budget::Usage,
        reservation::{CallAdmission, DelegationCallKind, DelegationCallReservation},
    },
};

#[derive(Default)]
pub(super) struct BudgetSessions {
    pub(super) sessions: MemSession,
    deny: Option<DelegationCallKind>,
    pub(super) reservations: RefCell<Vec<DelegationCallReservation>>,
    settlements: RefCell<Vec<(String, Option<GoalUsage>)>>,
}

#[async_trait(?Send)]
impl SessionSeam for BudgetSessions {
    async fn claim_turn(
        &self,
        params: ClaimTurnParams,
    ) -> Result<PersistedAgentSession, ClaimError> {
        let mut session = self.sessions.claim_turn(params).await?;
        session.delegation_group_id = Some("group".into());
        session.input_revision = 1;
        Ok(session)
    }

    async fn save(&self, session: &mut PersistedAgentSession) -> Result<(), AgentError> {
        self.sessions.save(session).await
    }

    async fn reserve_delegation_call(
        &self,
        session: &PersistedAgentSession,
        logical_id: &str,
        kind: DelegationCallKind,
        digest: &str,
        upper: GoalUsage,
        _now: &str,
    ) -> Result<CallAdmission, AgentError> {
        if self.deny == Some(kind) {
            return Ok(CallAdmission::Exhausted);
        }
        if self
            .reservations
            .borrow()
            .iter()
            .any(|receipt| receipt.logical_call_id == logical_id)
        {
            return Err(crate::subagent::invalid(
                "an admitted physical call cannot be replayed",
            ));
        }
        let receipt = DelegationCallReservation {
            reservation_id: format!("receipt-{}", self.reservations.borrow().len()),
            logical_call_id: logical_id.into(),
            root_conversation_id: session.agent_role.binding().map_or_else(
                || session.conversation_id.clone(),
                |binding| binding.root_conversation_id.clone(),
            ),
            conversation_id: session.conversation_id.clone(),
            group_id: "group".into(),
            task_id: session
                .agent_role
                .binding()
                .map(|binding| binding.task_id.clone()),
            kind,
            arguments_sha256: digest.into(),
            source_epoch: 1,
            input_revision: session.input_revision,
            control_revision: session.control_revision,
            planning_lease_token: Some(session.lease_token),
            review_authority: None,
            upper: Usage {
                model_calls: u64::from(upper.model_calls),
                tool_calls: u64::from(upper.tool_calls),
                tokens: upper.total_tokens().unwrap(),
            },
            source_goal_upper: None,
        };
        receipt.validate().unwrap();
        self.reservations.borrow_mut().push(receipt.clone());
        Ok(CallAdmission::Reserved(receipt))
    }

    async fn settle_delegation_call(
        &self,
        receipt: &DelegationCallReservation,
        actual: Option<GoalUsage>,
        _now: &str,
    ) -> Result<(), AgentError> {
        assert!(self.reservations.borrow().contains(receipt));
        self.settlements
            .borrow_mut()
            .push((receipt.reservation_id.clone(), actual));
        Ok(())
    }
}

#[tokio::test]
async fn exhausted_budget_stops_before_any_provider_call() {
    let sessions = BudgetSessions {
        deny: Some(DelegationCallKind::Model),
        ..Default::default()
    };
    let model = ScriptModel {
        turns: RefCell::new([answer("must not be requested")].into()),
        requests: Rc::new(RefCell::new(Vec::new())),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(Vec::new())),
        reply: "unused".into(),
    };
    let clock = || "2026-06-20T00:00:01Z".into();
    let mut deps = deps(&sessions.sessions, &model, &tools, &[], &clock);
    deps.session_seam = &sessions;
    let error = run_agent_turn(
        &deps,
        claim(),
        ChatMessage::text("user", ChatRole::User, "Investigate"),
        &mut NullTurnSink,
    )
    .await
    .unwrap_err();
    assert!(call_budget::is_exhausted(&error));
    assert!(model.requests.borrow().is_empty());
    assert!(sessions.settlements.borrow().is_empty());
}

#[tokio::test]
async fn physical_retry_has_a_new_reservation_and_unknown_usage_remains_unknown() {
    let sessions = BudgetSessions::default();
    let model = ScriptModel {
        turns: RefCell::new([answer(""), answer("Delivered")].into()),
        requests: Rc::new(RefCell::new(Vec::new())),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(Vec::new())),
        reply: "unused".into(),
    };
    let clock = || "2026-06-20T00:00:01Z".into();
    let mut deps = deps(&sessions.sessions, &model, &tools, &[], &clock);
    deps.session_seam = &sessions;
    let result = run_agent_turn(
        &deps,
        claim(),
        ChatMessage::text("user", ChatRole::User, "Investigate"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    assert!(matches!(result, LoopOutcome::Answered(_)));
    let receipts = sessions.reservations.borrow();
    assert_eq!(receipts.len(), 2);
    assert_ne!(receipts[0].logical_call_id, receipts[1].logical_call_id);
    assert_eq!(receipts[0].kind, DelegationCallKind::Model);
    assert_eq!(receipts[1].kind, DelegationCallKind::Model);
    assert_eq!(sessions.settlements.borrow().len(), 2);
    assert!(
        sessions
            .settlements
            .borrow()
            .iter()
            .all(|(_, actual)| actual.is_none())
    );
    assert!(
        model
            .requests
            .borrow()
            .iter()
            .all(|request| request.caller_output_hard_cap == Some(call_budget::OUTPUT_HARD_CAP))
    );
    let requests = model.requests.borrow();
    assert_eq!(requests[0].delegation_call.as_ref(), Some(&receipts[0]));
    assert_eq!(requests[1].delegation_call.as_ref(), Some(&receipts[1]));
}

#[tokio::test]
async fn denied_tool_budget_closes_history_before_any_device_dispatch() {
    let sessions = BudgetSessions {
        deny: Some(DelegationCallKind::Tool),
        ..Default::default()
    };
    let model = ScriptModel {
        turns: RefCell::new([tool_use("read-1", "sysinfo")].into()),
        requests: Rc::new(RefCell::new(Vec::new())),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(Vec::new())),
        reply: "unused".into(),
    };
    let registry = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "2026-06-20T00:00:01Z".into();
    let mut deps = deps(&sessions.sessions, &model, &tools, &registry, &clock);
    deps.session_seam = &sessions;
    let error = run_agent_turn(
        &deps,
        claim(),
        ChatMessage::text("user", ChatRole::User, "Investigate"),
        &mut NullTurnSink,
    )
    .await
    .unwrap_err();
    assert!(call_budget::is_exhausted(&error));
    assert!(tools.calls.borrow().is_empty());
    let saved = sessions.sessions.inner.borrow();
    let session = saved.as_ref().unwrap();
    assert_eq!(
        session
            .conversation
            .iter()
            .filter(|message| message.tool_call_id.as_deref() == Some("read-1"))
            .count(),
        1
    );
    assert!(session.conversation.iter().any(|message| {
        message
            .text
            .starts_with("not executed: call budget admission failed")
    }));
}
