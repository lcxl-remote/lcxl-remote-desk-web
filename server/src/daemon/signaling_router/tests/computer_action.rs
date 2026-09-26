//! The same central action routing is used by OSS and Manager.
use super::*;
use desk_agent_protocol::computer_use::*;

fn plan() -> SealedComputerActionPlan {
    use desk_agent_protocol::browser_control::*;
    let expiry = (chrono::Utc::now() + chrono::Duration::minutes(1)).to_rfc3339();
    SealedComputerActionPlan {
        turn_scope: None,
        schema_version: COMPUTER_USE_SCHEMA_VERSION,
        work_id: "1".into(),
        action_request_id: "action-1".into(),
        execution_generation: "generation-1".into(),
        device_id: "host-client".into(),
        interactive_session_incarnation: "worker-1".into(),
        adapter: ComputerUseAdapterRef {
            kind: ComputerUseAdapterKind::BrowserExtension,
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
async fn disabled_switch_rejects_new_computer_action_before_dispatch() {
    let ctx = make_ctx().await;
    ctx.settings.write().await.ai_assistant.enabled = false;
    let mut output = ctx.outbound_tx.subscribe();
    let plan = plan();
    let dispatch = SignalingModel::new(
        &plan.execution_generation,
        SignalingType::DispatchComputerAction,
        None,
        None,
        Some(serde_json::to_value(&plan).unwrap()),
        None,
    );
    handle_computer_action_inbound(&ctx, &dispatch)
        .await
        .unwrap();
    let denied: SignalingModel = serde_json::from_str(&output.try_recv().unwrap()).unwrap();
    let completed: ComputerActionCompleted = denied.get_data().unwrap();
    assert_eq!(
        completed.result,
        ComputerActionResultClass::DefinitelyNotStarted
    );
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
        file_recovery_registration: None,
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
    assert!(payload.turn_authority.is_none());
    assert_eq!(payload.plan, plan);
    assert!(output.try_recv().is_err());
    // Namespace binding is derived from both the live connection and the
    // central registration. Unmatched identities never reach the worker.
    ctx.file_recovery_authority = Some("a".repeat(64));
    for (authority, os_user, accepted) in [
        ("a".repeat(64), "501", true),
        ("b".repeat(64), "501", false),
        ("a".repeat(64), "", false),
        ("a".repeat(64), "501\n", false),
    ] {
        let authz = ctx.inbound_authz.as_mut().unwrap();
        authz.session_id = Some("conversation".into());
        authz.file_recovery_registration =
            Some(desk_agent_protocol::authz::FileRecoveryRegistration {
                execution_epoch: 7,
                authority,
                os_user: os_user.into(),
            });
        handle_computer_action_inbound(&ctx, &model).await.unwrap();
        let ServiceToWorker::ComputerActionPlan(payload) = receiver.try_recv().unwrap() else {
            panic!("action expected")
        };
        assert_eq!(payload.file_recovery.is_some(), accepted);
        assert_eq!(payload.turn_authority, Some("a".repeat(64)));
        if let Some(binding) = payload.file_recovery {
            assert_eq!(binding.authority, "a".repeat(64));
            assert_eq!(binding.os_user, "501");
            assert_eq!(binding.execution_epoch, 7);
            assert_eq!(binding.conversation_id, "conversation");
        }
    }
    // A claimed turn must match the centrally authenticated conversation.
    let mut scoped = plan.clone();
    scoped.turn_scope = Some(ComputerActionTurnScope {
        conversation_id: "conversation".into(),
        turn_id: "turn".into(),
        input_revision: 2,
        lease_token: 3,
    });
    let scoped_model = SignalingModel::new(
        &scoped.execution_generation,
        SignalingType::DispatchComputerAction,
        None,
        None,
        Some(serde_json::to_value(&scoped).unwrap()),
        None,
    );
    for conversation in [None, Some("other"), Some("conversation")] {
        ctx.inbound_authz.as_mut().unwrap().session_id = conversation.map(str::to_owned);
        handle_computer_action_inbound(&ctx, &scoped_model)
            .await
            .unwrap();
        if conversation == Some("conversation") {
            let ServiceToWorker::ComputerActionPlan(payload) = receiver.try_recv().unwrap() else {
                panic!("action expected")
            };
            assert_eq!(payload.plan.turn_scope, scoped.turn_scope);
            assert_eq!(payload.turn_authority, ctx.file_recovery_authority);
            assert!(output.try_recv().is_err());
        } else {
            read(output.try_recv().unwrap());
            assert!(receiver.try_recv().is_err());
        }
    }
    ctx.worker_mgr.enable_session_targeting_for_test();
    handle_computer_action_inbound(&ctx, &model).await.unwrap();
    read(output.try_recv().unwrap());
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn central_stop_preserves_identity_without_renewing_action_authority() {
    use desk_agent_protocol::authz::*;
    for issuer in ["manager", "signal"] {
        let mut ctx = make_ctx().await;
        let mut output = ctx.outbound_tx.subscribe();
        ctx.host_control_hub
            .remote_access_gate()
            .initialize_from_store(crate::daemon::remote_access::RemoteAccessState::locked(
                2,
                "lock-1".into(),
                chrono::Utc::now().to_rfc3339(),
                false,
            ));
        let cancel = ComputerActionCancel {
            work_id: "1".into(),
            action_request_id: "action-1".into(),
            execution_generation: "generation-1".into(),
            reason: "owner stopped".into(),
        };
        let model = SignalingModel::new(
            "stop-1",
            SignalingType::CancelComputerAction,
            None,
            None,
            Some(serde_json::to_value(&cancel).unwrap()),
            None,
        );
        let stamp = AuthorizationBlock {
            file_recovery_registration: None,
            version: AUTHORIZATION_BLOCK_VERSION,
            scope: AgentScope {
                granted: vec![],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            orchestrator_grants: vec![],
            max_risk: desk_agent_protocol::RiskLevel::Low,
            actor: AuthzActor { user_id: Some(7) },
            device: AuthzDevice {
                device_id: Some(11),
            },
            request_id: model.request_id.clone(),
            session_id: None,
            expires_at: Some((chrono::Utc::now() + chrono::Duration::seconds(10)).to_rfc3339()),
            issuer: issuer.into(),
            audience: "host-client".into(),
            signature: None,
            exec_admission_policy: ExecAdmissionPolicy::OwnerInteractive,
        };
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        ctx.worker_mgr.install_active_for_test(sender).await;
        for case in ["missing", "actor", "request", "expired", "payload"] {
            ctx.inbound_authz = Some(stamp.clone());
            let mut malformed = model.clone();
            match case {
                "missing" => ctx.inbound_authz = None,
                "actor" => ctx.inbound_authz.as_mut().unwrap().actor.user_id = Some(0),
                "request" => ctx
                    .inbound_authz
                    .as_mut()
                    .unwrap()
                    .request_id
                    .push_str("-other"),
                "expired" => {
                    ctx.inbound_authz.as_mut().unwrap().expires_at =
                        Some((chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339())
                }
                "payload" => {
                    malformed = SignalingModel::new(
                        "stop-1",
                        SignalingType::CancelComputerAction,
                        None,
                        None,
                        Some(serde_json::json!({})),
                        None,
                    )
                }
                _ => unreachable!(),
            }
            route(&malformed, &ctx).await.unwrap();
            let frame: SignalingModel = serde_json::from_str(&output.try_recv().unwrap()).unwrap();
            assert_eq!(
                frame.signaling_type,
                SignalingType::ComputerActionStateReported,
                "{case}"
            );
            assert_eq!(frame.request_id, model.request_id);
            assert_ne!(frame.response_state.unwrap().error_code, 0);
            assert!(receiver.try_recv().is_err());
        }
        ctx.inbound_authz = Some(stamp);
        route(&model, &ctx).await.unwrap();
        let ServiceToWorker::ComputerActionCancel(payload) = receiver.try_recv().unwrap() else {
            panic!("stop expected")
        };
        assert_eq!(payload.cancel, cancel);
        assert_eq!(payload.approved_actor_id, "7");
        assert_eq!(payload.request_id, model.request_id);
        assert!(payload.connection_id.is_none());
        assert!(output.try_recv().is_err());
        let dispatch = SignalingModel::new(
            "dispatch-while-locked",
            SignalingType::DispatchComputerAction,
            None,
            None,
            Some(serde_json::to_value(plan()).unwrap()),
            None,
        );
        route(&dispatch, &ctx).await.unwrap();
        let denied: SignalingModel = serde_json::from_str(&output.try_recv().unwrap()).unwrap();
        assert_eq!(
            denied.signaling_type,
            SignalingType::ComputerActionCompleted
        );
        assert_eq!(
            denied.response_state.unwrap().error_code,
            DeskErrorCode::REMOTE_ACCESS_LOCKED.code()
        );
        assert!(receiver.try_recv().is_err());
        ctx.worker_mgr.enable_session_targeting_for_test();
        route(&model, &ctx).await.unwrap();
        let frame: SignalingModel = serde_json::from_str(&output.try_recv().unwrap()).unwrap();
        assert_eq!(
            frame.signaling_type,
            SignalingType::ComputerActionStateReported
        );
        assert_eq!(frame.request_id, model.request_id);
        assert_ne!(frame.response_state.unwrap().error_code, 0);
        assert!(receiver.try_recv().is_err());
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn turn_status_reaches_only_original_worker_even_while_remote_access_is_locked() {
    use desk_agent_protocol::computer_turn::{
        ComputerActionTurnQuery, ComputerActionTurnState, ComputerActionTurnStatus,
    };
    let mut ctx = make_ctx().await;
    ctx.file_recovery_authority = Some("a".repeat(64));
    let (old_tx, mut old_rx) = tokio::sync::mpsc::unbounded_channel();
    let old_incarnation = ctx.worker_mgr.install_active_for_test(old_tx).await;
    let request = desk_ipc_protocol::message::ComputerTurnQueryPayload {
        request_id: uuid::Uuid::new_v4().to_string(),
        authority: "a".repeat(64),
        control_generation: 3,
        query: ComputerActionTurnQuery {
            actor_id: "7".into(),
            scope: ComputerActionTurnScope {
                conversation_id: "conversation".into(),
                turn_id: "turn".into(),
                input_revision: 1,
                lease_token: 2,
            },
        },
    };
    assert!(
        ctx.worker_mgr
            .register_turn_query(None, old_incarnation, &request)
            .await
    );
    let (new_tx, mut new_rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.worker_mgr.install_active_for_test(new_tx).await;
    ctx.host_control_hub
        .remote_access_gate()
        .initialize_from_store(crate::daemon::remote_access::RemoteAccessState::locked(
            2,
            "lock".into(),
            chrono::Utc::now().to_rfc3339(),
            false,
        ));
    let status = ComputerActionTurnStatus {
        query: request.query.clone(),
        state: ComputerActionTurnState::Revoked,
    };
    let model = SignalingModel::success_response(
        &request.request_id,
        SignalingType::ComputerActionTurnStatus,
        None,
        None,
        Some(&status),
    )
    .unwrap();
    let wrong_source = RouterContext {
        file_recovery_authority: Some("b".repeat(64)),
        ..ctx.clone()
    };
    route(&model, &wrong_source).await.unwrap();
    assert!(old_rx.try_recv().is_err());
    route(&model, &ctx).await.unwrap();
    let ServiceToWorker::ComputerTurnStatus(received) = old_rx.try_recv().unwrap() else {
        panic!("status expected")
    };
    assert_eq!(received.request, request);
    assert!(new_rx.try_recv().is_err());
    route(&model, &ctx).await.unwrap();
    assert!(old_rx.try_recv().is_err());
}
