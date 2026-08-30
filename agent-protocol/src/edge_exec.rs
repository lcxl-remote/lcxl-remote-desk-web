//! Fleet batch-execution wire payloads (manager ↔ desk-server daemon).
//!
//! The manager is the policy decision point (PDP): it previews, approves, and
//! seals an [`crate::exec::ExecPlan`] per device, then ships it to the desk
//! server over `EdgeExecRequest` wrapped in an
//! [`crate::authz::AuthorizedControlPayload`]. The daemon is the policy
//! enforcement point (PEP): it independently re-validates the plan (authz,
//! blocklist, exact-argv whitelist, fingerprint, `risk <= max_risk`) before
//! handing the argv to the worker.
//!
//! The reply rides `EdgeExecResult` carrying a structured
//! [`EdgeExecDisposition`] rather than a bare [`AgentOutcome`]. The structured
//! variants let the manager decide the terminal status **without parsing an
//! `AgentErrorKind` or a message string**: only `RejectedBeforeDispatch` /
//! `DispatchFailedBeforeWorker` prove the change was *not* executed, so a
//! mutating plan that ends in `ExecutionStateUnknown` (or yields no result at
//! all) is held for human review instead of being falsely reported as failed.
//!
//! At-most-once safety lives in the manager's intent-before-send ledger; these
//! wire types only carry the per-attempt correlation `request_id` (the manager
//! writes results back by `request_id` + claim token + attempt). No execution id
//! is on the wire in v1.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::exec::ExecPlan;
use crate::{AgentError, AgentErrorKind, AgentOutcome, ExecInput};

/// Two-phase Administrator execution handshake. Session-user plans must omit
/// this field entirely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum PrivilegedExecRequest {
    /// Ask the host to perform registration-bound local polkit authorization.
    /// No execution authority is consumed and no command may start in this phase.
    Authorize,
    /// After a fresh central re-authorization, consume the exact one-shot permit
    /// returned by [`EdgeExecDisposition::PrivilegedAuthorizationReady`].
    Dispatch { permit_id: String },
}

/// Central → daemon (`EdgeExecRequest`): the sealed [`ExecPlan`] plus the
/// source-specific context the daemon PEP needs to re-validate it. Wrapped in an
/// [`crate::authz::AuthorizedControlPayload`] on the wire.
///
/// The `source` tag is **required**: a frame without it — or an `Agentic` frame
/// missing `validation_input` — is rejected. There is no legacy bare-`ExecPlan`
/// form; both the fleet executor and the agentic edge sender always emit a tagged
/// variant, and the daemon dispatches on the tag to the matching validator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum EdgeExecRequestPayload {
    /// Fleet batch execution. The plan was sealed from an **operator** template
    /// rendered with the fixed fleet limits (no per-request input, no cwd), so the
    /// daemon re-validates it by reproducing that authoritative render from its
    /// operator snapshot — never a per-turn classification.
    Fleet { plan: ExecPlan },
    /// Agentic confirmed execution. The plan was sealed from a per-turn
    /// classification of `validation_input` (a built-in **or** operator template,
    /// clamped per-turn limits + the input's cwd), which the fleet render cannot
    /// reproduce. The daemon re-runs the same classifier over `validation_input`
    /// to reproduce the plan field-for-field.
    ///
    /// `validation_input` is a **daemon-only validation envelope**: it is used to
    /// re-derive the argv and then discarded — it is never forwarded to the worker,
    /// preserving the "worker never sees the command string" invariant (only the
    /// frozen [`ExecPlan`] argv reaches the worker).
    Agentic {
        plan: ExecPlan,
        validation_input: ExecInput,
        /// Control-end connection whose immutable host-side session binding
        /// anchors this Assistant task. The central brain treats this as opaque;
        /// only the edge daemon resolves it through its own connection-to-session
        /// table. Missing keeps the 0/1/N behavior for older callers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_connection_id: Option<String>,
        /// Required for Administrator plans and forbidden for SessionUser plans.
        /// Optional on the wire only so mixed-version peers fail closed in the PEP
        /// instead of failing JSON decoding before a structured reply can be sent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        privileged: Option<PrivilegedExecRequest>,
    },
}

impl EdgeExecRequestPayload {
    /// The sealed plan, regardless of source.
    pub fn plan(&self) -> &ExecPlan {
        match self {
            EdgeExecRequestPayload::Fleet { plan } => plan,
            EdgeExecRequestPayload::Agentic { plan, .. } => plan,
        }
    }

    /// Consume the envelope, yielding the sealed plan and dropping any daemon-only
    /// validation context (the `validation_input`). The single place the plan is
    /// extracted for worker dispatch, so the validation envelope can never leak
    /// past the PEP.
    pub fn into_plan(self) -> ExecPlan {
        match self {
            EdgeExecRequestPayload::Fleet { plan } => plan,
            EdgeExecRequestPayload::Agentic { plan, .. } => plan,
        }
    }

    pub fn session_connection_id(&self) -> Option<&str> {
        match self {
            EdgeExecRequestPayload::Fleet { .. } => None,
            EdgeExecRequestPayload::Agentic {
                session_connection_id,
                ..
            } => session_connection_id.as_deref(),
        }
    }

    pub fn privileged_request(&self) -> Option<&PrivilegedExecRequest> {
        match self {
            EdgeExecRequestPayload::Fleet { .. } => None,
            EdgeExecRequestPayload::Agentic { privileged, .. } => privileged.as_ref(),
        }
    }
}

/// Daemon → manager: the structured outcome class for one fleet execution
/// attempt. Drives the manager's terminal-status mapping directly; the manager
/// never inspects `AgentErrorKind` or a message string to decide whether the
/// change ran.
///
/// - [`EdgeExecDisposition::RejectedBeforeDispatch`]: the daemon PEP rejected
///   the plan (authz/blocklist/whitelist/fingerprint/max_risk) **before** any
///   handoff to the worker — the change definitely did not run → `denied`.
/// - [`EdgeExecDisposition::DispatchFailedBeforeWorker`]: the daemon accepted
///   the plan but could not hand it to the worker (worker offline / IPC send
///   failed) **before** execution started — the change definitely did not run →
///   `failed` / `offline`.
/// - [`EdgeExecDisposition::Executed`]: the worker ran the plan to completion;
///   the wrapped [`AgentOutcome`] carries the exit code / per-call error.
/// - [`EdgeExecDisposition::ExecutionStateUnknown`]: the plan was handed to the
///   worker but the result is unknown (the daemon lost the worker mid-flight).
///   A mutating plan in this state is held for review, never reported as failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EdgeExecDisposition {
    /// Intermediate response: local polkit authorization succeeded, but the host
    /// has not consumed dispatch authority and the command has not started. The
    /// central brain must freshly re-authorize the exact generation and send a
    /// `PrivilegedExecRequest::Dispatch` before the permit expires.
    PrivilegedAuthorizationReady { permit_id: String },
    /// PEP refused the plan before any worker handoff. Change did not run.
    RejectedBeforeDispatch {
        /// Structured failure preserved across the edge and manager routing hops.
        error: AgentError,
    },
    /// The host is at its own concurrency ceiling. Change did **not** run, and
    /// unlike a policy rejection this will succeed later — the caller should retry
    /// rather than treat the target as settled.
    HostAtCapacity {
        /// Retryable capacity failure.
        error: AgentError,
    },
    /// Plan accepted but could not be handed to the worker. Change did not run.
    DispatchFailedBeforeWorker {
        /// Worker/session/transport failure that happened before execution.
        error: AgentError,
    },
    /// Worker ran the plan to completion.
    Executed {
        /// The execution result (exit code / per-call error).
        outcome: AgentOutcome,
    },
    /// Handed to the worker but the result is unknown (worker lost mid-flight).
    ExecutionStateUnknown {
        /// Model-safe reason describing why the result is unknown.
        reason: String,
    },
}

impl EdgeExecDisposition {
    /// Construct a model-safe pre-dispatch error with no business-code mapping.
    pub fn safe_error(
        kind: AgentErrorKind,
        message: impl Into<String>,
        retryable: bool,
    ) -> AgentError {
        AgentError {
            kind,
            message: message.into(),
            retryable,
            safe_for_model: true,
            error_code: None,
        }
    }

    /// Whether this disposition *proves* the change was not executed. Only the
    /// pre-dispatch variants do; `Executed` ran and `ExecutionStateUnknown` is, by
    /// definition, uncertain.
    pub fn proves_not_executed(&self) -> bool {
        matches!(
            self,
            EdgeExecDisposition::RejectedBeforeDispatch { .. }
                | EdgeExecDisposition::DispatchFailedBeforeWorker { .. }
                | EdgeExecDisposition::HostAtCapacity { .. }
        )
    }

    /// This response advances a two-phase handshake and must never settle a task.
    pub fn is_intermediate(&self) -> bool {
        matches!(self, Self::PrivilegedAuthorizationReady { .. })
    }

    /// Turn the host's authoritative view of a dispatch into a disposition, for an
    /// upstream whose live result never arrived (the wait elapsed, or the frame
    /// was lost). This is the move from *guessing* to *asking*: a bare timeout used
    /// to be reported as [`Self::ExecutionStateUnknown`] whether or not the command
    /// had run, and here the host — the only party that knows — decides instead.
    ///
    /// The mapping:
    /// - [`ExecState::Terminal`] with its stored result → replay it verbatim as
    ///   [`Self::Executed`]. A terminal state whose result has aged out of the
    ///   ledger is still genuinely uncertain to the upstream, so it stays unknown.
    /// - [`ExecState::SpawnFailed`] / [`ExecState::Unknown`] → the host never ran
    ///   it (a failed spawn, or no ledger record at all), so this *proves not
    ///   executed*: reported as [`Self::DispatchFailedBeforeWorker`], which is
    ///   retryable rather than held for review.
    /// - [`ExecState::Indeterminate`] → the host lost track of it across a crash;
    ///   this is the one truly-unknown case the whole design narrows down to.
    /// - [`ExecState::Reserved`] / [`ExecState::Running`] → still in flight, so the
    ///   upstream has no settled answer to adopt yet: unknown, and it may ask again.
    pub fn from_reconciled_state(reply: &crate::exec_lifecycle::ExecStateReplyPayload) -> Self {
        use crate::exec_lifecycle::ExecState;
        match reply.state {
            ExecState::Terminal => match reply
                .result_json
                .as_deref()
                .and_then(|json| serde_json::from_str::<AgentOutcome>(json).ok())
            {
                Some(outcome) => EdgeExecDisposition::Executed { outcome },
                // Terminal, but the result is no longer holdable (aged out, or
                // unreadable): the upstream cannot recover the answer, so it is
                // honestly uncertain rather than reported as a specific outcome.
                None => EdgeExecDisposition::ExecutionStateUnknown {
                    reason: "the host finished this command but no longer holds its result".into(),
                },
            },
            ExecState::SpawnFailed => EdgeExecDisposition::DispatchFailedBeforeWorker {
                error: Self::safe_error(
                    AgentErrorKind::SessionUnavailable,
                    reply
                        .detail
                        .clone()
                        .unwrap_or_else(|| "the command failed to start on the host".into()),
                    true,
                ),
            },
            ExecState::Unknown => EdgeExecDisposition::DispatchFailedBeforeWorker {
                error: Self::safe_error(
                    AgentErrorKind::SessionUnavailable,
                    "the host has no record of accepting this command",
                    true,
                ),
            },
            ExecState::Indeterminate => EdgeExecDisposition::ExecutionStateUnknown {
                reason: reply.detail.clone().unwrap_or_else(|| {
                    "the host lost track of this command across a restart".into()
                }),
            },
            ExecState::Reserved | ExecState::Running => {
                EdgeExecDisposition::ExecutionStateUnknown {
                    reason: "the command is still running on the host".into(),
                }
            }
        }
    }
}

/// Daemon → manager reply for a `EdgeExecRequest`, correlated by the
/// per-attempt `request_id` the manager minted before sending. The manager's
/// pending store is bound to the originating edge connection, so a stray result
/// from another connection is dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct EdgeExecResultPayload {
    /// The per-attempt correlation id echoed back from the originating request.
    pub request_id: String,
    /// Structured outcome class driving the manager's terminal-status mapping.
    pub disposition: EdgeExecDisposition,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentError, AgentErrorKind, ExecOutput, OperationOutput};

    fn executed_ok() -> EdgeExecDisposition {
        EdgeExecDisposition::Executed {
            outcome: AgentOutcome::Ok(OperationOutput::Exec(ExecOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
                stdout_truncated: false,
                stderr_truncated: false,
                duration_ms: 1,
                redactions: vec![],
            })),
        }
    }

    #[test]
    fn pre_dispatch_variants_prove_not_executed() {
        assert!(
            EdgeExecDisposition::RejectedBeforeDispatch {
                error: EdgeExecDisposition::safe_error(
                    AgentErrorKind::RiskBlocked,
                    "blocklist",
                    false,
                ),
            }
            .proves_not_executed()
        );
        assert!(
            EdgeExecDisposition::DispatchFailedBeforeWorker {
                error: EdgeExecDisposition::safe_error(
                    AgentErrorKind::SessionUnavailable,
                    "worker offline",
                    true,
                ),
            }
            .proves_not_executed()
        );
    }

    fn ok_outcome_json() -> String {
        serde_json::to_string(&AgentOutcome::Ok(OperationOutput::Exec(ExecOutput {
            exit_code: 0,
            stdout: "done".into(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            duration_ms: 5,
            redactions: vec![],
        })))
        .unwrap()
    }

    fn state_reply(
        state: crate::exec_lifecycle::ExecState,
        result_json: Option<String>,
    ) -> crate::exec_lifecycle::ExecStateReplyPayload {
        crate::exec_lifecycle::ExecStateReplyPayload {
            execution_generation: "gen-1".into(),
            state,
            containment_identity: None,
            running_ms: None,
            detail: None,
            result_json,
        }
    }

    /// A terminal state replays the host's stored result verbatim, so an upstream
    /// that lost the live result frame recovers the real answer instead of a guess.
    #[test]
    fn a_terminal_state_replays_the_stored_result() {
        use crate::exec_lifecycle::ExecState;
        let reply = state_reply(ExecState::Terminal, Some(ok_outcome_json()));
        match EdgeExecDisposition::from_reconciled_state(&reply) {
            EdgeExecDisposition::Executed { outcome } => {
                assert!(matches!(outcome, AgentOutcome::Ok(_)));
            }
            other => panic!("expected the stored result replayed, got {other:?}"),
        }
    }

    /// A terminal state whose result has aged out is honestly uncertain, not a
    /// fabricated success.
    #[test]
    fn a_terminal_state_without_a_result_is_unknown_not_invented() {
        use crate::exec_lifecycle::ExecState;
        let reply = state_reply(ExecState::Terminal, None);
        assert!(matches!(
            EdgeExecDisposition::from_reconciled_state(&reply),
            EdgeExecDisposition::ExecutionStateUnknown { .. }
        ));
    }

    /// A host with no record of the dispatch, or a failed spawn, *proves* the
    /// command did not run — the win over a bare timeout, which held it for review.
    #[test]
    fn no_record_or_failed_spawn_proves_not_executed() {
        use crate::exec_lifecycle::ExecState;
        for state in [ExecState::Unknown, ExecState::SpawnFailed] {
            let disposition = EdgeExecDisposition::from_reconciled_state(&state_reply(state, None));
            assert!(
                disposition.proves_not_executed(),
                "{state:?} should prove not executed, got {disposition:?}"
            );
        }
    }

    /// The one genuinely-uncertain case: the host crashed and cannot say. This is
    /// what `ExecutionStateUnknown` is narrowed down to.
    #[test]
    fn only_a_lost_host_stays_unknown() {
        use crate::exec_lifecycle::ExecState;
        let reply = state_reply(ExecState::Indeterminate, None);
        let disposition = EdgeExecDisposition::from_reconciled_state(&reply);
        assert!(matches!(
            disposition,
            EdgeExecDisposition::ExecutionStateUnknown { .. }
        ));
        assert!(!disposition.proves_not_executed());
    }

    /// A still-running command has no settled answer to adopt, so the upstream
    /// stays uncertain rather than inventing one.
    #[test]
    fn a_still_running_command_has_no_settled_answer() {
        use crate::exec_lifecycle::ExecState;
        for state in [ExecState::Reserved, ExecState::Running] {
            assert!(matches!(
                EdgeExecDisposition::from_reconciled_state(&state_reply(state, None)),
                EdgeExecDisposition::ExecutionStateUnknown { .. }
            ));
        }
    }

    #[test]
    fn executed_and_unknown_do_not_prove_not_executed() {
        assert!(!executed_ok().proves_not_executed());
        assert!(
            !EdgeExecDisposition::ExecutionStateUnknown {
                reason: "connection lost".into(),
            }
            .proves_not_executed()
        );
    }

    #[test]
    fn payload_json_round_trips_each_variant() {
        let variants = vec![
            EdgeExecDisposition::PrivilegedAuthorizationReady {
                permit_id: "permit-1".into(),
            },
            EdgeExecDisposition::RejectedBeforeDispatch {
                error: EdgeExecDisposition::safe_error(
                    AgentErrorKind::PermissionDenied,
                    "pep_rejected:authz",
                    false,
                ),
            },
            EdgeExecDisposition::DispatchFailedBeforeWorker {
                error: EdgeExecDisposition::safe_error(
                    AgentErrorKind::SessionUnavailable,
                    "worker unavailable",
                    true,
                ),
            },
            EdgeExecDisposition::HostAtCapacity {
                error: EdgeExecDisposition::safe_error(
                    AgentErrorKind::HostAtCapacity,
                    "host busy",
                    true,
                ),
            },
            executed_ok(),
            EdgeExecDisposition::Executed {
                outcome: AgentOutcome::Err(AgentError {
                    kind: AgentErrorKind::Timeout,
                    message: "timed out".into(),
                    retryable: false,
                    safe_for_model: true,
                    error_code: None,
                }),
            },
            EdgeExecDisposition::ExecutionStateUnknown {
                reason: "lost worker".into(),
            },
        ];
        for disposition in variants {
            let payload = EdgeExecResultPayload {
                request_id: "attempt_1".into(),
                disposition,
            };
            let json = serde_json::to_string(&payload).expect("json encode");
            let back: EdgeExecResultPayload = serde_json::from_str(&json).expect("json decode");
            assert_eq!(payload, back);
        }
    }

    #[test]
    fn disposition_tag_is_snake_case() {
        let json = serde_json::to_string(&EdgeExecDisposition::RejectedBeforeDispatch {
            error: EdgeExecDisposition::safe_error(AgentErrorKind::PermissionDenied, "x", false),
        })
        .unwrap();
        assert!(
            json.contains("\"kind\":\"rejected_before_dispatch\""),
            "{json}"
        );
    }

    fn sample_plan() -> ExecPlan {
        use crate::RiskLevel;
        use crate::exec::{
            ApprovalId, ExecExecutionBasis, ExecPlanDraft, ExecRequestId, ExecShellKind,
        };
        ExecPlan::from_draft(
            ExecRequestId("req-1".into()),
            "gen-1",
            ApprovalId("appr-1".into()),
            ExecPlanDraft {
                program: "powershell".into(),
                argv: vec!["-Command".into(), "Get-Service".into()],
                cwd: None,
                shell: ExecShellKind::Powershell,
                risk: RiskLevel::Low,
                execution_basis: ExecExecutionBasis::Template,
                principal: crate::exec::ExecutionPrincipal::SessionUser,
                template_id: "get_service".into(),
                fingerprint: "fp-1".into(),
                timeout_ms: 10_000,
                max_stdout_bytes: 65_536,
                max_stderr_bytes: 65_536,
                containment: crate::exec::ExecContainmentSnapshot::default(),
            },
        )
    }

    fn sample_input() -> ExecInput {
        use crate::ExecTarget;
        ExecInput {
            target: ExecTarget::Shell {
                shell: "powershell".into(),
            },
            command: "Get-Service".into(),
            cwd: None,
            timeout_ms: 10_000,
            max_stdout_bytes: 65_536,
            max_stderr_bytes: 65_536,
        }
    }

    #[test]
    fn request_payload_variants_round_trip_and_expose_plan() {
        let fleet = EdgeExecRequestPayload::Fleet {
            plan: sample_plan(),
        };
        let agentic = EdgeExecRequestPayload::Agentic {
            plan: sample_plan(),
            validation_input: sample_input(),
            session_connection_id: Some("controller-1".into()),
            privileged: None,
        };
        for payload in [fleet.clone(), agentic.clone()] {
            let json = serde_json::to_string(&payload).expect("encode");
            let back: EdgeExecRequestPayload = serde_json::from_str(&json).expect("decode");
            assert_eq!(payload, back);
            assert_eq!(back.plan(), &sample_plan());
            assert_eq!(back.into_plan(), sample_plan());
        }
        // The tag distinguishes the two sources.
        assert!(
            serde_json::to_string(&fleet)
                .unwrap()
                .contains("\"source\":\"fleet\"")
        );
        assert!(
            serde_json::to_string(&agentic)
                .unwrap()
                .contains("\"source\":\"agentic\"")
        );
    }

    #[test]
    fn privileged_handshake_round_trips_and_never_looks_terminal() {
        let request = EdgeExecRequestPayload::Agentic {
            plan: sample_plan(),
            validation_input: sample_input(),
            session_connection_id: Some("controller-1".into()),
            privileged: Some(PrivilegedExecRequest::Dispatch {
                permit_id: "permit-1".into(),
            }),
        };
        let json = serde_json::to_string(&request).unwrap();
        let decoded: EdgeExecRequestPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, request);
        assert!(matches!(
            decoded.privileged_request(),
            Some(PrivilegedExecRequest::Dispatch { permit_id }) if permit_id == "permit-1"
        ));

        let ready = EdgeExecDisposition::PrivilegedAuthorizationReady {
            permit_id: "permit-1".into(),
        };
        assert!(ready.is_intermediate());
        assert!(!ready.proves_not_executed());
    }

    /// A frame without the `source` tag is rejected — there is no bare-`ExecPlan`
    /// legacy form the daemon would silently accept.
    #[test]
    fn missing_source_tag_is_rejected() {
        let plan_json = serde_json::to_value(sample_plan()).unwrap();
        assert!(serde_json::from_value::<EdgeExecRequestPayload>(plan_json).is_err());
    }

    /// An `Agentic` frame missing its `validation_input` is rejected: the daemon
    /// cannot re-classify without it, so there is no fallback that would let a
    /// plan through unvalidated.
    #[test]
    fn agentic_without_validation_input_is_rejected() {
        let json = serde_json::json!({
            "source": "agentic",
            "plan": serde_json::to_value(sample_plan()).unwrap(),
        });
        assert!(serde_json::from_value::<EdgeExecRequestPayload>(json).is_err());
    }
}
