use super::*;
use crate::{action_version::ActionVersion, seam::ExecCompletion, session::AgentSessionSurface};
use std::cell::Cell;

struct CasStore(RefCell<PersistedAgentSession>);

fn stale() -> AgentError {
    AgentError {
        kind: AgentErrorKind::SessionUnavailable,
        message: "stale version".into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    }
}

#[async_trait(?Send)]
impl SessionSeam for CasStore {
    async fn claim_turn(&self, _: ClaimTurnParams) -> Result<PersistedAgentSession, ClaimError> {
        unreachable!()
    }
    async fn save(&self, held: &mut PersistedAgentSession) -> Result<(), AgentError> {
        let mut stored = self.0.borrow_mut();
        if stored.version != held.version || stored.lease_token != held.lease_token {
            return Err(stale());
        }
        held.version += 1;
        *stored = held.clone();
        Ok(())
    }
}

struct TransactionalTools<'a> {
    store: &'a CasStore,
    mode: &'static str,
    observed_version: Cell<i64>,
    acknowledged: Cell<bool>,
}

#[async_trait(?Send)]
impl ToolSeam for TransactionalTools<'_> {
    async fn run_read(&self, _: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        unreachable!()
    }

    async fn confirm_and_exec_versioned(
        &self,
        call: &ToolCall,
        ctx: &ExecContext,
        version: Option<&ActionVersion>,
    ) -> ExecCompletion {
        let version = version.unwrap();
        let mut stored = self.store.0.borrow_mut();
        assert_eq!(stored.turn_state, TurnState::AwaitingApproval);
        assert_eq!(stored.version, version.version);
        assert_eq!(ctx.assistant_turn_fence.as_ref(), Some(&version.turn_fence));
        assert_eq!(call.id, version.tool_call_id);
        self.observed_version.set(version.version);
        stored.version += 1;
        let mut receipt_source = version.clone();
        if self.mode == "wrong-call" {
            receipt_source.tool_call_id = "other".into();
        }
        let receipt = receipt_source.committed(stored.version).unwrap();
        // These are independent writes after Prepare, not this executor's work.
        match self.mode {
            "revoke" => stored.version += 1,
            "input" => {
                stored.version += 1;
                stored.input_revision += 1;
            }
            "lease" => {
                stored.version += 1;
                stored.lease_token += 1;
            }
            _ => {}
        }
        let output = ToolRunOutput {
            content: "confirmed result".into(),
            image_data_url: None,
        };
        let outcome = if self.mode == "backend-error" {
            Err(AgentError {
                kind: AgentErrorKind::Internal,
                message: "after commit".into(),
                retryable: false,
                safe_for_model: false,
                error_code: None,
            })
        } else {
            Ok(ExecOutcome::Executed {
                data_envelope: Some(original_results::original(&output, false)),
                output,
                event_id: Some("durable-result".into()),
            })
        };
        ExecCompletion {
            outcome,
            version_advance: (self.mode != "missing-receipt").then_some(receipt),
        }
    }

    async fn ack_delivery(&self, event_id: &str) -> Result<(), AgentError> {
        assert_eq!(event_id, "durable-result");
        assert!(
            self.store
                .0
                .borrow()
                .conversation
                .iter()
                .any(|m| m.message_id == event_id)
        );
        self.acknowledged.set(true);
        Ok(())
    }
}

async fn execute(
    mode: &'static str,
) -> (
    Result<Option<LoopOutcome>, AgentError>,
    PersistedAgentSession,
    i64,
    bool,
) {
    let mut held = PersistedAgentSession::new("conv", "actor", "device", 1, exec_scope(), "now");
    held.surface = AgentSessionSurface::DeviceAssistant;
    held.turn_state = TurnState::Running;
    held.current_turn_id = Some("turn".into());
    held.input_revision = 1;
    held.lease_token = 1;
    held.version = 10;
    held.conversation.push(ChatMessage::assistant_tool_calls(
        "proposal",
        "",
        vec![ToolCallRef {
            id: "c1".into(),
            name: "exec_command".into(),
            arguments_json: "{}".into(),
        }],
    ));
    let store = CasStore(RefCell::new(held.clone()));
    let runtime = TransactionalTools {
        store: &store,
        mode,
        observed_version: Cell::new(-1),
        acknowledged: Cell::new(false),
    };
    let dummy = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let clock = || "now".into();
    let mut deps = deps(&dummy, &model, &runtime, &[], &clock);
    deps.session_seam = &store;
    let result = run_mutating(
        &deps,
        &mut held,
        "turn",
        &ToolCall {
            id: "c1".into(),
            name: "exec_command".into(),
            arguments_json: "{}".into(),
        },
        &[],
        &mut || "minted".into(),
        &mut None,
        &mut NullTurnSink,
    )
    .await;
    let observed = runtime.observed_version.get();
    let ack = runtime.acknowledged.get();
    (result, store.0.into_inner(), observed, ack)
}

#[tokio::test]
async fn committed_version_is_captured_after_save_and_result_saved_before_ack() {
    let (result, saved, observed, ack) = execute("success").await;
    result.unwrap();
    assert_eq!(observed, 11);
    assert_eq!(saved.version, 13);
    assert!(ack);
    assert!(
        saved
            .conversation
            .iter()
            .any(|m| m.message_id == "durable-result")
    );
}

#[tokio::test]
async fn failure_after_commit_keeps_own_version_without_acknowledging_result() {
    let (result, saved, observed, ack) = execute("backend-error").await;
    assert_eq!(result.unwrap_err().message, "after commit");
    assert_eq!(observed, 11);
    assert_eq!(saved.version, 13);
    assert_eq!(saved.turn_state, TurnState::Running);
    assert!(!ack);
    assert_eq!(saved.conversation.len(), 1);
}

#[tokio::test]
async fn independent_writes_or_bad_receipts_never_save_or_ack_old_result() {
    for mode in ["revoke", "input", "lease", "wrong-call", "missing-receipt"] {
        let (result, saved, _, ack) = execute(mode).await;
        assert_eq!(
            result.unwrap_err().kind,
            AgentErrorKind::SessionUnavailable,
            "{mode}"
        );
        assert!(!ack, "{mode}");
        assert_eq!(saved.turn_state, TurnState::AwaitingApproval);
        assert_eq!(saved.conversation.len(), 1);
        assert_eq!(saved.input_revision, if mode == "input" { 2 } else { 1 });
        assert_eq!(saved.lease_token, if mode == "lease" { 2 } else { 1 });
    }
}
