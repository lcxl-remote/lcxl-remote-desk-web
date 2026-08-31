//! The same central action routing is used by OSS and Manager.
use super::*;
use desk_agent_protocol::computer_use::*;

fn plan() -> SealedComputerActionPlan {
    use desk_agent_protocol::browser_control::*;
    let expiry = (chrono::Utc::now() + chrono::Duration::minutes(1)).to_rfc3339();
    SealedComputerActionPlan {
        schema_version: COMPUTER_USE_SCHEMA_VERSION,
        work_id: "1".into(),
        action_request_id: "action-1".into(),
        execution_generation: "generation-1".into(),
        device_id: "host-client".into(),
        interactive_session_incarnation: "worker-1".into(),
        adapter: ComputerUseAdapterRef {
            kind: ComputerUseAdapterKind::BrowserDevtoolsMcp,
            version: "1".into(),
        },
        approval_id: "approval-1".into(),
        approved_actor_id: "7".into(),
        draft_hash: "a".repeat(64),
        expires_at: expiry.clone(),
        timeout_ms: 1000,
        actions: vec![ComputerActionStep {
            target: ObjectRef {
                token: "opaque".into(),
                snapshot_id: "snapshot-1".into(),
                object_kind: ObjectKind::BrowserSurface,
                expires_at: expiry,
            },
            action: ComputerActionKind::Browser(BrowserActionRequest {
                schema_version: 1,
                call_id: "action-1".into(),
                action: BrowserAction::OpenPage {
                    target: BrowserNavigationTarget {
                        url: "https://example.com/".into(),
                        origin: BrowserOrigin {
                            kind: BrowserOriginKind::Https,
                            host_ascii: "example.com".into(),
                            port: 443,
                        },
                    },
                },
            }),
            before_summary: "No page".into(),
            after_intent: "Open page".into(),
            verification: "Read page".into(),
        }],
    }
}

#[tokio::test]
async fn central_computer_action_dispatches_without_peer_and_rejects_missing_worker_as_completed() {
    let mut ctx = make_ctx().await;
    let mut output = ctx.outbound_tx.subscribe();
    let plan = plan();
    plan.validate().unwrap();
    let model = SignalingModel::new(
        &plan.execution_generation,
        SignalingType::DispatchComputerAction,
        None,
        None,
        Some(serde_json::to_value(&plan).unwrap()),
        None,
    );
    // Missing authorization is a typed, definitely-not-started completion.
    handle_computer_action_inbound(&ctx, &model).await.unwrap();
    let read = |text: String| {
        let frame: SignalingModel = serde_json::from_str(&text).unwrap();
        assert_eq!(frame.signaling_type, SignalingType::ComputerActionCompleted);
        assert_eq!(frame.request_id, plan.execution_generation);
        assert!(frame.to_connection_id.is_none());
        let completed: ComputerActionCompleted = frame.get_data().unwrap();
        assert_eq!(
            completed.result,
            ComputerActionResultClass::DefinitelyNotStarted
        );
        assert_eq!(completed.action_request_id, plan.action_request_id);
        assert_eq!(completed.work_id, plan.work_id);
        assert!(completed.output.is_none());
    };
    read(output.try_recv().unwrap());
    ctx.inbound_authz = Some(desk_agent_protocol::authz::AuthorizationBlock {
        version: desk_agent_protocol::authz::AUTHORIZATION_BLOCK_VERSION,
        scope: AgentScope {
            granted: vec![plan.actions[0].action.required_capability()],
            mode: ExecutionMode::ConfirmEachAction,
            expires_at: None,
            policy_name: None,
        },
        orchestrator_grants: vec!["browser.page.open".into()],
        max_risk: desk_agent_protocol::RiskLevel::High,
        actor: desk_agent_protocol::authz::AuthzActor { user_id: Some(7) },
        device: desk_agent_protocol::authz::AuthzDevice {
            device_id: Some(11),
        },
        request_id: plan.execution_generation.clone(),
        session_id: None,
        expires_at: Some(plan.expires_at.clone()),
        issuer: "manager".into(),
        audience: plan.device_id.clone(),
        signature: None,
        exec_admission_policy: desk_agent_protocol::authz::ExecAdmissionPolicy::OwnerInteractive,
    });
    handle_computer_action_inbound(&ctx, &model).await.unwrap();
    read(output.try_recv().unwrap());
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    ctx.worker_mgr.install_active_for_test(sender).await;
    handle_computer_action_inbound(&ctx, &model).await.unwrap();
    let ServiceToWorker::ComputerActionPlan(payload) = receiver.try_recv().unwrap() else {
        panic!("action expected")
    };
    assert!(payload.connection_id.is_none());
    assert_eq!(payload.plan, plan);
    assert!(output.try_recv().is_err());
    ctx.worker_mgr.enable_session_targeting_for_test();
    handle_computer_action_inbound(&ctx, &model).await.unwrap();
    read(output.try_recv().unwrap());
    assert!(receiver.try_recv().is_err());
}
