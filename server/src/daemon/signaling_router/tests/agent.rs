use super::*;

// ---- AI agent plane: two-phase parse + authz + routing ----

pub(super) fn agent_request_model(raw: serde_json::Value) -> SignalingModel {
    SignalingModel::new(
        "req-ai-1",
        SignalingType::AgentRequest,
        Some("conn-1".to_string()),
        None,
        Some(raw),
        None,
    )
}

pub(super) fn read_outcome(rx: &mut broadcast::Receiver<String>) -> AgentOutcome {
    read_response(rx)
        .get_data::<AgentOutcome>()
        .expect("AgentResponse must carry an AgentOutcome")
}

/// A fully-known read request is accepted.
#[test]
fn two_phase_parse_accepts_known_read_kind() {
    let raw = serde_json::json!({
        "operation": {
            "risk_hint": null,
            "input": {
                "kind": "read_context",
                "params": { "kind": { "kind": "process_list", "params": {} } }
            }
        },
        "reason": null
    });
    assert!(validate_agent_request_kinds(&raw).is_ok());
}

/// An unknown *outer* kind (newer control end) degrades to
/// `UnsupportedCapability`, never a serde parse error.
#[test]
fn two_phase_parse_rejects_unknown_outer_kind() {
    let raw = serde_json::json!({
        "operation": { "input": { "kind": "telepathy", "params": {} } }
    });
    let err = validate_agent_request_kinds(&raw).expect_err("unknown outer kind");
    assert_eq!(err.kind, AgentErrorKind::UnsupportedCapability);
}

/// An unknown *inner* read kind is the case a single-pass (outer-only)
/// check would miss: it would slip through to the typed `from_value` and
/// hard-fail. The descent to `operation.input.params.kind.kind`
/// catches it as `UnsupportedCapability`.
#[test]
fn two_phase_parse_rejects_unknown_inner_read_kind() {
    let raw = serde_json::json!({
        "operation": {
            "input": {
                "kind": "read_context",
                "params": { "kind": { "kind": "quantum_state", "params": {} } }
            }
        }
    });
    let err = validate_agent_request_kinds(&raw).expect_err("unknown inner kind");
    assert_eq!(err.kind, AgentErrorKind::UnsupportedCapability);
}

/// Authorization is a pure set-membership check: the granted scope
/// admits its capabilities and denies everything else. This is the
/// `PermissionDenied` mechanism a future policy engine narrows.
#[test]
fn authorize_respects_granted_set() {
    assert!(authorize(
        Capability::ProcessList,
        &default_read_scope().granted
    ));
    assert!(!authorize(Capability::ProcessList, &[]));
    assert!(!authorize(
        Capability::ScreenCaptureCurrent,
        &[Capability::SystemInfo]
    ));
}

/// Unknown read kind routed through the full handler emits an
/// outbound `AgentResponse(AgentOutcome::Err(UnsupportedCapability))`
/// and never forwards anything to the worker.
#[tokio::test]
async fn agent_request_unknown_kind_emits_unsupported_outcome() {
    let (ctx, mut rx) = make_ctx_with_rx().await;
    let raw = serde_json::json!({
        "operation": {
            "input": {
                "kind": "read_context",
                "params": { "kind": { "kind": "quantum_state", "params": {} } }
            }
        },
        "reason": null
    });
    handle_agent_request_inbound(&ctx, &agent_request_model(raw))
        .await
        .unwrap();
    match read_outcome(&mut rx) {
        AgentOutcome::Err(e) => assert_eq!(e.kind, AgentErrorKind::UnsupportedCapability),
        other => panic!("unexpected outcome: {other:?}"),
    }
}

/// `exec` parses cleanly but derives no capability, so the
/// handler rejects it as `UnsupportedCapability` without forwarding.
#[tokio::test]
async fn agent_request_exec_is_unsupported_until_m2() {
    let (ctx, mut rx) = make_ctx_with_rx().await;
    let raw = serde_json::json!({
        "operation": {
            "input": {
                "kind": "exec",
                "params": {
                    "target": { "type": "shell", "shell": "powershell" },
                    "command": "Get-Service",
                    "cwd": null,
                    "timeout_ms": 1000,
                    "max_stdout_bytes": 1024,
                    "max_stderr_bytes": 1024
                }
            }
        },
        "reason": null
    });
    handle_agent_request_inbound(&ctx, &agent_request_model(raw))
        .await
        .unwrap();
    match read_outcome(&mut rx) {
        AgentOutcome::Err(e) => assert_eq!(e.kind, AgentErrorKind::UnsupportedCapability),
        other => panic!("unexpected outcome: {other:?}"),
    }
}

/// A valid read forwards a typed `ServiceToWorker::AgentRequest` with
/// every trusted field stamped server-side: `request_id` from the
/// signaling model, the actor injected (never self-reported by the
/// control end), and the connection correlated.
#[tokio::test]
async fn agent_request_valid_forwards_with_server_injected_fields() {
    use desk_agent_protocol::{
        AgentOperation, ContextKind, OperationInput, ProcessListParams, ReadContextInput,
    };
    let ctx = make_ctx().await;
    let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    ctx.worker_mgr.install_active_for_test(ipc_tx).await;

    let req = AgentRequestData {
        operation: AgentOperation {
            risk_hint: None,
            input: OperationInput::ReadContext(ReadContextInput {
                kind: ContextKind::ProcessList(ProcessListParams::default()),
            }),
        },
        reason: Some("diagnose cpu".to_string()),
        org_id: None,
    };
    let raw = serde_json::to_value(&req).unwrap();
    handle_agent_request_inbound(&ctx, &agent_request_model(raw))
        .await
        .unwrap();

    match ipc_rx
        .try_recv()
        .expect("worker should receive AgentRequest")
    {
        ServiceToWorker::AgentRequest(p) => {
            assert_eq!(p.request_id, "req-ai-1");
            assert_eq!(p.connection_id.as_deref(), Some("conn-1"));
            // request_id is re-stamped from the signaling model, not
            // trusted from the (absent) control-end value.
            assert_eq!(p.envelope.request_id.0, "req-ai-1");
            // actor is server-injected.
            assert_eq!(p.envelope.actor.actor_type, ActorType::System);
            // reason flows through to the audit metadata.
            assert_eq!(p.envelope.audit.reason.as_deref(), Some("diagnose cpu"));
        }
        other => panic!("unexpected IPC: {other:?}"),
    }
}

/// Provider credentials live on the central brain, so the edge no longer
/// blocks AI reads on a local "gateway configured" gate. A valid read on a
/// host with no worker proceeds past authorization (default local read scope)
/// and reports `TargetOffline` — not the removed "not configured" rejection.
#[tokio::test]
async fn agent_request_without_local_gateway_proceeds_to_authorization() {
    use desk_agent_protocol::{
        AgentOperation, ContextKind, OperationInput, ProcessListParams, ReadContextInput,
    };
    let (ctx, mut rx) = make_ctx_with_rx().await;
    // Default settings: no local model config exists to configure anymore.
    let req = AgentRequestData {
        operation: AgentOperation {
            risk_hint: None,
            input: OperationInput::ReadContext(ReadContextInput {
                kind: ContextKind::ProcessList(ProcessListParams::default()),
            }),
        },
        reason: None,
        org_id: None,
    };
    let raw = serde_json::to_value(&req).unwrap();
    handle_agent_request_inbound(&ctx, &agent_request_model(raw))
        .await
        .unwrap();
    match read_outcome(&mut rx) {
        AgentOutcome::Err(e) => assert_eq!(e.kind, AgentErrorKind::TargetOffline),
        other => panic!("unexpected outcome: {other:?}"),
    }
}
