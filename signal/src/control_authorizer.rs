//! Signal single-account policy decision point for control-end AI frames.
//!
//! The portable signal server is the OSS central brain (thin-edge model): it
//! authorizes and wraps the control-end AI frames (`AgentRequest` / `ConfirmExec`)
//! as they are relayed to the edge, and owns the centrally-orchestrated frames
//! (`Diagnose` / terminal copilot / completion) which it runs itself rather than
//! relaying. This mirrors the manager's `ManagerControlAuthorizer`, but as a
//! **separate, much simpler implementation**: OSS signal is single-account, so
//! there is no organization governance matrix, no cross-instance ledger / quota /
//! presence routing — the one account is treated as the owner of every device it
//! reaches, with the edge's local `execution_mode` ceiling the real bound.
//!
//! Trusted-field discipline is identical to the manager's: the actor is the
//! single account (server-resolved from the cookie session, never self-reported),
//! the target device is resolved from the **receiving** token-authenticated
//! `Server` connection (a control end can never claim to *be* a device), and the
//! decision is stamped into an [`AuthorizationBlock`]. The edge (PEP) validates
//! the binding and enforces the carried decision.
//!
//! Single-node assumption: short-lived approval/result waiters stay in process
//! memory, while dispatched Agent execution tasks and completion delivery are
//! durable in the local SQLite database (it is not horizontally scaled like the
//! manager).

use std::sync::Arc;

use actix_web::web;
use desk_agent_protocol::authz::{
    AUTHORIZATION_BLOCK_VERSION, AuthorizationBlock, AuthorizedControlPayload, AuthzActor,
    AuthzDevice, ExecAdmissionPolicy,
};
use desk_agent_protocol::diagnose::DiagnoseRequestData;
use desk_agent_protocol::exec::ResolveExecData;
use desk_agent_protocol::{AgentScope, Capability, ExecutionMode, RiskLevel};
use desk_signal_facade::model::auth_context::{AuthContext, AuthKind};
use desk_signal_facade::model::connection::{ConnectionState, SharedConnectionMap};
use desk_signal_facade::model::signal::{RemoteDeskTypeEnum, SignalingModel, SignalingType};
use desk_signal_facade::service::{ControlFrameAuthorizer, ControlFrameOutcome};
use desk_utils::error::DeskErrorCode;
use sea_orm::DatabaseConnection;

use crate::collect_pending::CollectPendingStore;
use crate::model_provider;

/// The single account's synthetic user id. OSS signal has no multi-user table;
/// every authenticated operator is the one account, so a fixed id stamps the
/// `actor` for audit attribution and exec ownership.
pub const SINGLE_ACCOUNT_USER_ID: i32 = 1;

/// The single account's synthetic token id. OSS signal validates node tokens
/// without a per-token registry row, so a fixed id tags the token-authenticated
/// (`Server`) connections in the auth context.
pub const SINGLE_ACCOUNT_TOKEN_ID: i32 = 1;

/// Validity window of an injected authorization block (seconds). Matches the
/// manager's `AUTHZ_TTL_SECS`; doubles as the wrapper replay window the edge
/// enforces via `expires_at`.
const AUTHZ_TTL_SECS: i64 = 300;
/// The orchestrator-layer permission gating AI diagnosis.
const AI_DIAGNOSE_GRANT: &str = "ai.diagnose";
/// The orchestrator-layer permission gating the terminal AI copilot / completion.
const AI_COPILOT_GRANT: &str = "ai.terminal_copilot";

/// The nine read-evidence capabilities a diagnosis may draw on.
fn evidence_capabilities() -> [Capability; 9] {
    [
        Capability::SystemInfo,
        Capability::ProcessList,
        Capability::NetworkPorts,
        Capability::ServiceStatus,
        Capability::LogRecent,
        Capability::ContainerList,
        Capability::ContainerInspect,
        Capability::ContainerLogs,
        Capability::ScreenCaptureCurrent,
    ]
}

/// The broad decision the single account is granted over any device it reaches.
/// Read evidence and confirmed/read-only exec are granted; the `mode` is the
/// central grant (the provider config's `execution_mode`), which the edge then
/// clamps with its own local ceiling (`restrict_to`) — signal never widens past
/// the edge's local settings. The risk ceiling is the highest non-blocked level;
/// the edge's confirm-exec gate is the real bound.
fn single_account_decision(mode: ExecutionMode) -> (AgentScope, Vec<String>, RiskLevel) {
    let mut granted: Vec<Capability> = evidence_capabilities().to_vec();
    granted.push(Capability::ShellExecReadonly);
    granted.push(Capability::ShellExecConfirmed);
    let scope = AgentScope {
        granted,
        mode,
        expires_at: None,
        policy_name: Some("single-account".to_string()),
    };
    (
        scope,
        vec![
            AI_DIAGNOSE_GRANT.to_string(),
            AI_COPILOT_GRANT.to_string(),
            "shell.plan".to_string(),
        ],
        RiskLevel::Critical,
    )
}

/// Resolve the actor user id from a sending connection's auth context. Only a
/// cookie-authenticated control end is a valid actor; a token (node) connection
/// or an anonymous one is not.
fn actor_user_id(ctx: &AuthContext) -> Option<i32> {
    if ctx.auth_kind == AuthKind::CookieAuth {
        ctx.user_id
    } else {
        None
    }
}

/// Why resolving the target device from the receiving connection failed.
struct TargetReject {
    code: DeskErrorCode,
    message: &'static str,
}

/// Resolve the audience (the target edge's `client_id`) from the receiving
/// connection's validated state. The target must be a token-authenticated
/// `Server` carrying a client id; a control end can never satisfy this, so it
/// can never address a frame to a non-device or claim to be one itself. Pure
/// over the validated fields so it is unit-testable without a live connection.
fn resolve_target_audience(
    auth_kind: AuthKind,
    remote_desk_type: RemoteDeskTypeEnum,
    client_id: Option<&str>,
) -> Result<String, TargetReject> {
    let is_server =
        auth_kind == AuthKind::TokenAuth && remote_desk_type == RemoteDeskTypeEnum::Server;
    if !is_server {
        return Err(TargetReject {
            code: DeskErrorCode::PERMISSION_ERROR,
            message: "target is not an authorized device",
        });
    }
    match client_id {
        Some(id) if !id.is_empty() => Ok(id.to_string()),
        _ => Err(TargetReject {
            code: DeskErrorCode::PERMISSION_ERROR,
            message: "target device has no client id",
        }),
    }
}

/// Build the authorized wrapper relay outcome for a resolved decision. Pure over
/// its inputs (the expiry is passed in) so the stamped block is unit-testable.
/// OSS signal carries no device-registry primary key, so `device.device_id` is
/// `None`; the audience (target client id) is the replay/misroute binding the
/// edge enforces. Returns `Forward` with the wrapped frame, or `Reject` if the
/// frame had no payload to authorize / could not be encoded.
#[allow(clippy::too_many_arguments)]
fn build_wrapper_outcome(
    model: &SignalingModel,
    scope: AgentScope,
    orchestrator_grants: Vec<String>,
    max_risk: RiskLevel,
    actor_user_id: i32,
    audience: String,
    issuer: String,
    expires_at_rfc3339: String,
) -> ControlFrameOutcome {
    let Some(inner) = model.get_raw_data().clone() else {
        return ControlFrameOutcome::Reject {
            code: DeskErrorCode::INVALID_PARAMS,
            message: "AI frame had no payload".to_string(),
        };
    };
    let authz = AuthorizationBlock {
        version: AUTHORIZATION_BLOCK_VERSION,
        exec_admission_policy: match scope.mode {
            ExecutionMode::ConfirmEachAction | ExecutionMode::SessionApproved => {
                ExecAdmissionPolicy::OwnerInteractive
            }
            _ => ExecAdmissionPolicy::TemplateOnly,
        },
        scope,
        orchestrator_grants,
        max_risk,
        actor: AuthzActor {
            user_id: Some(actor_user_id),
        },
        device: AuthzDevice { device_id: None },
        request_id: model.request_id.clone(),
        session_id: None,
        expires_at: Some(expires_at_rfc3339),
        issuer,
        audience,
        signature: None,
    };
    let wrapper = AuthorizedControlPayload { inner, authz };
    let data = match serde_json::to_value(&wrapper) {
        Ok(v) => v,
        Err(e) => {
            return ControlFrameOutcome::Reject {
                code: DeskErrorCode::SYSTEM_ERROR,
                message: format!("failed to encode authorization wrapper: {e}"),
            };
        }
    };
    ControlFrameOutcome::Forward(SignalingModel::new(
        &model.request_id,
        model.signaling_type,
        model.from_connection_id.clone(),
        model.to_connection_id.clone(),
        Some(data),
        model.response_state.clone(),
    ))
}

/// Signal single-account policy decision point.
pub struct SignalControlAuthorizer {
    db: DatabaseConnection,
    issuer: String,
    /// In-flight remote-collect store. A `DiagnoseCancel` cancels the pending
    /// collection here (diagnosis is orchestrated centrally; there is no
    /// diagnose task on the edge to relay a cancel to).
    collect_pending: Arc<CollectPendingStore>,
    /// Connection map handle used to stream centrally-orchestrated terminal
    /// copilot / completion results back to the asking browser. Held (not just
    /// borrowed per call) so a spawned, `!Send` model dial can reach the browser
    /// after `authorize` returns.
    connection_map: web::Data<SharedConnectionMap>,
}

impl SignalControlAuthorizer {
    pub fn new(
        db: DatabaseConnection,
        collect_pending: Arc<CollectPendingStore>,
        connection_map: web::Data<SharedConnectionMap>,
    ) -> Self {
        Self {
            db,
            issuer: "signal".to_string(),
            collect_pending,
            connection_map,
        }
    }

    /// Load the central execution-mode grant (the configured provider's
    /// `execution_mode`). On a load error, fail closed to the safe default
    /// (`SuggestOnly`) so a transient DB issue can never widen what the edge runs.
    async fn central_mode_grant(&self) -> ExecutionMode {
        match model_provider::load(&self.db).await {
            Ok(config) => config.execution_mode,
            Err(e) => {
                log::warn!("[control-authz] failed to load provider config, defaulting mode: {e}");
                ExecutionMode::SuggestOnly
            }
        }
    }
}

impl ControlFrameAuthorizer for SignalControlAuthorizer {
    fn authorize<'a>(
        &'a self,
        actor: &'a ConnectionState,
        connection_map: &'a SharedConnectionMap,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ControlFrameOutcome> + Send + 'a>> {
        Box::pin(async move {
            // Start-over for a centrally-orchestrated diagnosis: cancel
            // the pending collection (if any) and never relay — the edge has no
            // diagnose task. Needs only the request id, so it is handled before
            // the relay pre-flight.
            if model.signaling_type == SignalingType::CancelDiagnosis {
                self.collect_pending.cancel(&model.request_id);
                let cancelled = crate::agent_exec::global_agent_exec_pending()
                    .cancel_approvals_for_diagnosis(&actor.model.connection_id, &model.request_id);
                log::info!(
                    "[agent-exec] diagnose cancel request_id={} browser={} cancelled_approvals={}",
                    model.request_id,
                    actor.model.connection_id,
                    cancelled
                );
                return ControlFrameOutcome::Handled;
            }
            // A copilot cancel is consumed centrally (the copilot runs on signal,
            // not the edge); best-effort no-op, never relayed.
            if model.signaling_type == SignalingType::CancelTerminalCopilot {
                return ControlFrameOutcome::Handled;
            }
            // A signal-owned agentic exec parks its approval here. Consume only
            // when the request id and browser connection both match; otherwise
            // this is the ordinary browser-initiated ConfirmExec flow and still
            // relays to the host unchanged.
            if model.signaling_type == SignalingType::ResolveExecution {
                if let Ok(data) = model.get_data::<ResolveExecData>()
                    && crate::agent_exec::global_agent_exec_pending()
                        .resolve(&actor.model.connection_id, &data)
                {
                    log::info!(
                        "[agent-exec] approval resolved exec_request_id={} browser={} decision={:?}",
                        data.exec_request_id.0,
                        actor.model.connection_id,
                        data.decision
                    );
                    return ControlFrameOutcome::Handled;
                }
                return ControlFrameOutcome::Forward(model.clone());
            }

            // Actor must be a cookie-authenticated control end (the single
            // account); the server resolves this, the control end cannot fake it.
            let Some(actor_user_id) = actor_user_id(&actor.auth_context) else {
                return ControlFrameOutcome::Reject {
                    code: DeskErrorCode::PERMISSION_ERROR,
                    message: "AI frames require an authenticated operator".to_string(),
                };
            };

            // Target device: resolved from the receiving connection's validated
            // state, never from a control-end self-report.
            let Some(to_id) = model.to_connection_id.clone() else {
                return ControlFrameOutcome::Reject {
                    code: DeskErrorCode::INVALID_PARAMS,
                    message: "AI frame missing target connection".to_string(),
                };
            };
            let target_descriptor = {
                let map = connection_map.read().await;
                match map.get(&to_id) {
                    None => Err(TargetReject {
                        code: DeskErrorCode::REMOTE_DESK_OFFLINE,
                        message: "target host is not connected",
                    }),
                    Some(target) => resolve_target_audience(
                        target.auth_context.auth_kind,
                        target.auth_context.remote_desk_type,
                        target.model.version_info.client_id.as_deref(),
                    )
                    .map(|audience| {
                        (
                            audience,
                            target.model.version_info.available_exec_shell_list(),
                            target
                                .model
                                .version_info
                                .max_ai_command_runtime_ms
                                .unwrap_or(desk_agent_protocol::exec_policy::DEFAULT_TIMEOUT_MS),
                        )
                    }),
                }
            };
            let (audience, available_exec_shells, max_command_runtime_ms) = match target_descriptor
            {
                Ok(descriptor) => descriptor,
                Err(reject) => {
                    return ControlFrameOutcome::Reject {
                        code: reject.code,
                        message: reject.message.to_string(),
                    };
                }
            };

            let mode = self.central_mode_grant().await;
            let (scope, orchestrator_grants, max_risk) = single_account_decision(mode);

            match model.signaling_type {
                // Acting on an already-authorized execution does not mint a new
                // plan. The authenticated single-account owner may forward a
                // cancel/query directly; the host answers from its durable ledger.
                SignalingType::ControlExecution => ControlFrameOutcome::Forward(model.clone()),
                // Single round-trip device frames: wrap with the decision and
                // relay to the edge, which re-checks and enforces.
                SignalingType::InvokeAgentCapability | SignalingType::PreviewExecution => {
                    let expires_at = (chrono::Utc::now()
                        + chrono::Duration::seconds(AUTHZ_TTL_SECS))
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                    build_wrapper_outcome(
                        model,
                        scope,
                        orchestrator_grants,
                        max_risk,
                        actor_user_id,
                        audience,
                        self.issuer.clone(),
                        expires_at,
                    )
                }
                // Diagnose is orchestrated centrally: signal pushes a
                // `CollectRequest` to the resolved edge, reassembles the evidence,
                // dials the model, and streams the result back — never relaying the
                // question to the edge. The orchestration uses signal's own
                // provider credentials, so the relay wrapper / audience are not
                // used on this path.
                SignalingType::DiagnoseDevice => {
                    let request = match model.get_data::<DiagnoseRequestData>() {
                        Ok(r) => r,
                        Err(e) => {
                            return ControlFrameOutcome::Reject {
                                code: DeskErrorCode::INVALID_PARAMS,
                                message: format!("invalid Diagnose payload: {e}"),
                            };
                        }
                    };
                    let browser_connection_id = actor.model.connection_id.clone();
                    crate::diagnose_orchestrator::start_diagnosis(
                        connection_map,
                        &self.collect_pending,
                        &model.request_id,
                        &to_id,
                        &browser_connection_id,
                        actor_user_id,
                        audience,
                        scope,
                        max_risk,
                        match mode {
                            ExecutionMode::ConfirmEachAction | ExecutionMode::SessionApproved => {
                                ExecAdmissionPolicy::OwnerInteractive
                            }
                            _ => ExecAdmissionPolicy::TemplateOnly,
                        },
                        available_exec_shells,
                        max_command_runtime_ms,
                        request,
                    )
                    .await;
                    ControlFrameOutcome::Handled
                }
                // The terminal copilot / completion run centrally too: signal
                // dials its own model over the inline terminal context the browser
                // supplied (no edge round-trip) and streams the result back. The
                // dial is `!Send` and latency-sensitive, so it is spawned and the
                // frame reported `Handled`. The relay wrapper / audience are not
                // used on this path (the question is never relayed to the edge).
                SignalingType::GenerateTerminalCompletions => {
                    let ask = match model
                        .get_data::<desk_agent_protocol::terminal_complete::TerminalCompleteAsk>()
                    {
                        Ok(a) => a,
                        Err(e) => {
                            return ControlFrameOutcome::Reject {
                                code: DeskErrorCode::INVALID_PARAMS,
                                message: format!("invalid TerminalCompleteAsk payload: {e}"),
                            };
                        }
                    };
                    actix_web::rt::spawn(crate::terminal_orchestrator::run_completion(
                        self.connection_map.clone(),
                        self.db.clone(),
                        model.request_id.clone(),
                        actor.model.connection_id.clone(),
                        ask,
                    ));
                    ControlFrameOutcome::Handled
                }
                SignalingType::AskTerminalCopilot => {
                    let ask = match model
                        .get_data::<desk_agent_protocol::terminal_copilot::TerminalCopilotAsk>()
                    {
                        Ok(a) => a,
                        Err(e) => {
                            return ControlFrameOutcome::Reject {
                                code: DeskErrorCode::INVALID_PARAMS,
                                message: format!("invalid TerminalCopilotAsk payload: {e}"),
                            };
                        }
                    };
                    actix_web::rt::spawn(crate::terminal_orchestrator::run_copilot(
                        self.connection_map.clone(),
                        self.db.clone(),
                        model.request_id.clone(),
                        actor.model.connection_id.clone(),
                        ask,
                    ));
                    ControlFrameOutcome::Handled
                }
                // The relay branch only routes the frame types above through the
                // authorizer; any other type here is a routing bug.
                other => ControlFrameOutcome::Reject {
                    code: DeskErrorCode::INVALID_PARAMS,
                    message: format!("unexpected control frame for authorization: {other}"),
                },
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnose_model(request_id: &str, to: Option<&str>) -> SignalingModel {
        let data = serde_json::to_value(DiagnoseRequestData {
            question: "why slow?".to_string(),
            ..Default::default()
        })
        .unwrap();
        SignalingModel::new(
            request_id,
            SignalingType::DiagnoseDevice,
            Some("browser-1".to_string()),
            to.map(str::to_string),
            Some(data),
            None,
        )
    }

    fn confirm_exec_model(request_id: &str, to: Option<&str>) -> SignalingModel {
        use desk_agent_protocol::exec::ConfirmExecData;
        use desk_agent_protocol::{AgentOperation, ExecInput, ExecTarget, OperationInput};
        let data = serde_json::to_value(ConfirmExecData {
            operation: AgentOperation {
                risk_hint: None,
                input: OperationInput::Exec(ExecInput {
                    target: ExecTarget::Shell {
                        shell: "bash".to_string(),
                    },
                    command: "systemctl status nginx".to_string(),
                    cwd: Some("/srv".to_string()),
                    timeout_ms: 0,
                    max_stdout_bytes: 0,
                    max_stderr_bytes: 0,
                }),
            },
            reason: Some("operator promoted a copilot suggestion".to_string()),
            org_id: None,
        })
        .unwrap();
        SignalingModel::new(
            request_id,
            SignalingType::PreviewExecution,
            Some("browser-1".to_string()),
            to.map(str::to_string),
            Some(data),
            None,
        )
    }

    #[test]
    fn evidence_capabilities_are_the_nine_reads() {
        let caps = evidence_capabilities();
        assert_eq!(caps.len(), 9);
        assert!(caps.contains(&Capability::SystemInfo));
        assert!(caps.contains(&Capability::ScreenCaptureCurrent));
        // No exec capability leaks into the read-evidence set.
        assert!(!caps.contains(&Capability::ShellExecConfirmed));
    }

    #[test]
    fn single_account_decision_grants_broadly_and_carries_central_mode() {
        let (scope, grants, max_risk) = single_account_decision(ExecutionMode::ConfirmEachAction);
        assert!(scope.granted.contains(&Capability::SystemInfo));
        assert!(scope.granted.contains(&Capability::ShellExecReadonly));
        assert!(scope.granted.contains(&Capability::ShellExecConfirmed));
        // The central grant carries the provider config's execution mode verbatim;
        // the edge clamps it locally.
        assert_eq!(scope.mode, ExecutionMode::ConfirmEachAction);
        assert_eq!(scope.policy_name.as_deref(), Some("single-account"));
        assert!(grants.contains(&AI_DIAGNOSE_GRANT.to_string()));
        assert!(grants.contains(&AI_COPILOT_GRANT.to_string()));
        assert_eq!(max_risk, RiskLevel::Critical);
    }

    #[test]
    fn actor_user_id_only_for_cookie_auth() {
        // A cookie control end resolves to its (single-account) user id.
        let cookie = AuthContext::cookie(SINGLE_ACCOUNT_USER_ID, RemoteDeskTypeEnum::Browser);
        assert_eq!(actor_user_id(&cookie), Some(SINGLE_ACCOUNT_USER_ID));
        // A token (node) connection is never a valid actor.
        let token = AuthContext::token_auth(SINGLE_ACCOUNT_USER_ID, 1, RemoteDeskTypeEnum::Server);
        assert_eq!(actor_user_id(&token), None);
        // An anonymous connection is never a valid actor.
        let anon = AuthContext::anonymous(RemoteDeskTypeEnum::Browser);
        assert_eq!(actor_user_id(&anon), None);
    }

    #[test]
    fn target_resolves_only_for_token_server_with_client_id() {
        // A token-auth Server with a client id resolves to that audience.
        assert_eq!(
            resolve_target_audience(
                AuthKind::TokenAuth,
                RemoteDeskTypeEnum::Server,
                Some("client-abc"),
            )
            .map_err(|r| r.message),
            Ok("client-abc".to_string())
        );
        // A cookie browser claiming to be a target is rejected — a control end can
        // never be addressed as a device.
        assert!(
            resolve_target_audience(
                AuthKind::CookieAuth,
                RemoteDeskTypeEnum::Browser,
                Some("client-abc"),
            )
            .is_err()
        );
        // A token Server without a client id has no audience to bind to.
        assert!(
            resolve_target_audience(AuthKind::TokenAuth, RemoteDeskTypeEnum::Server, None).is_err()
        );
        // A token connection self-reporting Browser is not a device either.
        assert!(
            resolve_target_audience(
                AuthKind::TokenAuth,
                RemoteDeskTypeEnum::Browser,
                Some("client-abc"),
            )
            .is_err()
        );
    }

    #[test]
    fn wrapper_stamps_server_resolved_identity_and_binding() {
        let model = diagnose_model("req-1", Some("edge-1"));
        let (scope, grants, max_risk) = single_account_decision(ExecutionMode::ReadOnly);
        let frame = match build_wrapper_outcome(
            &model,
            scope,
            grants,
            max_risk,
            SINGLE_ACCOUNT_USER_ID,
            "client-abc".to_string(),
            "signal".to_string(),
            "2999-01-01T00:00:00Z".to_string(),
        ) {
            ControlFrameOutcome::Forward(frame) => frame,
            _ => panic!("expected a wrapped Forward outcome"),
        };
        // The relay frame keeps the routing identity of the original.
        assert_eq!(frame.request_id, "req-1");
        assert_eq!(frame.to_connection_id.as_deref(), Some("edge-1"));
        // The stamped block carries server-resolved fields the control end cannot
        // self-report: the single-account actor, the target audience binding, and
        // the request-id replay binding.
        let wrapper: AuthorizedControlPayload<DiagnoseRequestData> =
            serde_json::from_value(frame.get_raw_data().clone().unwrap()).unwrap();
        assert_eq!(wrapper.authz.actor.user_id, Some(SINGLE_ACCOUNT_USER_ID));
        assert_eq!(wrapper.authz.device.device_id, None);
        assert_eq!(wrapper.authz.audience, "client-abc");
        assert_eq!(wrapper.authz.request_id, "req-1");
        assert_eq!(wrapper.authz.issuer, "signal");
        // The inner payload is preserved verbatim (control end carries no trusted
        // field; the question survives the round-trip).
        assert_eq!(wrapper.inner.question, "why slow?");
        // The block validates against the target audience + frame request id.
        assert!(
            wrapper
                .authz
                .validate("req-1", "client-abc", "2026-01-01T00:00:00Z")
                .is_ok()
        );
        // ...and is rejected against a different device audience (no replay /
        // misroute).
        assert!(
            wrapper
                .authz
                .validate("req-1", "other-device", "2026-01-01T00:00:00Z")
                .is_err()
        );
    }

    #[test]
    fn confirm_exec_frame_is_wrapped_for_relay() {
        // The operator-promoted copilot exec frame (ConfirmExec) is wrapped by the
        // central brain exactly like AgentRequest: the inner ConfirmExecData
        // survives verbatim (command + cwd preserved) and the stamped block binds
        // to the resolved audience + request id so the edge can validate it.
        use desk_agent_protocol::exec::ConfirmExecData;
        use desk_agent_protocol::{ExecTarget, OperationInput};
        let model = confirm_exec_model("req-9", Some("edge-1"));
        let (scope, grants, max_risk) = single_account_decision(ExecutionMode::ConfirmEachAction);
        // The single-account PDP grants the confirmed-exec capability, so the relay
        // is authorized to carry an exec frame at all.
        assert!(scope.granted.contains(&Capability::ShellExecConfirmed));
        let frame = match build_wrapper_outcome(
            &model,
            scope,
            grants,
            max_risk,
            SINGLE_ACCOUNT_USER_ID,
            "edge-1".to_string(),
            "signal".to_string(),
            "2999-01-01T00:00:00Z".to_string(),
        ) {
            ControlFrameOutcome::Forward(frame) => frame,
            _ => panic!("expected a wrapped Forward outcome"),
        };
        assert_eq!(frame.signaling_type, SignalingType::PreviewExecution);
        assert_eq!(frame.request_id, "req-9");
        assert_eq!(frame.to_connection_id.as_deref(), Some("edge-1"));
        let wrapper: AuthorizedControlPayload<ConfirmExecData> =
            serde_json::from_value(frame.get_raw_data().clone().unwrap()).unwrap();
        assert_eq!(wrapper.authz.actor.user_id, Some(SINGLE_ACCOUNT_USER_ID));
        assert_eq!(wrapper.authz.audience, "edge-1");
        assert_eq!(wrapper.authz.request_id, "req-9");
        // The exact command and working directory the operator chose ride through
        // unchanged; the edge re-classifies them server-side.
        match wrapper.inner.operation.input {
            OperationInput::Exec(exec) => {
                assert_eq!(exec.command, "systemctl status nginx");
                assert_eq!(exec.cwd.as_deref(), Some("/srv"));
                assert!(matches!(exec.target, ExecTarget::Shell { shell } if shell == "bash"));
            }
            _ => panic!("expected an exec operation"),
        }
        assert!(
            wrapper
                .authz
                .validate("req-9", "edge-1", "2026-01-01T00:00:00Z")
                .is_ok()
        );
    }

    #[test]
    fn wrapper_fails_closed_without_payload() {
        // A frame with no payload cannot be wrapped (nothing to authorize).
        let model = SignalingModel::new(
            "req-1",
            SignalingType::InvokeAgentCapability,
            Some("browser-1".to_string()),
            Some("edge-1".to_string()),
            None,
            None,
        );
        let (scope, grants, max_risk) = single_account_decision(ExecutionMode::ReadOnly);
        let outcome = build_wrapper_outcome(
            &model,
            scope,
            grants,
            max_risk,
            SINGLE_ACCOUNT_USER_ID,
            "client-abc".to_string(),
            "signal".to_string(),
            "2999-01-01T00:00:00Z".to_string(),
        );
        assert!(matches!(
            outcome,
            ControlFrameOutcome::Reject {
                code: DeskErrorCode::INVALID_PARAMS,
                ..
            }
        ));
    }
}
