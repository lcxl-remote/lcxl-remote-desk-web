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
use crate::{AgentOutcome, ExecInput};

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
    /// PEP refused the plan before any worker handoff. Change did not run.
    RejectedBeforeDispatch {
        /// Model-safe reason (PEP failure class).
        reason: String,
    },
    /// Plan accepted but could not be handed to the worker. Change did not run.
    DispatchFailedBeforeWorker {
        /// Model-safe reason (worker offline / IPC send failure).
        reason: String,
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
    /// Whether this disposition *proves* the change was not executed. Only the
    /// two pre-dispatch variants do; `Executed` ran and `ExecutionStateUnknown`
    /// is, by definition, uncertain.
    pub fn proves_not_executed(&self) -> bool {
        matches!(
            self,
            EdgeExecDisposition::RejectedBeforeDispatch { .. }
                | EdgeExecDisposition::DispatchFailedBeforeWorker { .. }
        )
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
                reason: "blocklist".into(),
            }
            .proves_not_executed()
        );
        assert!(
            EdgeExecDisposition::DispatchFailedBeforeWorker {
                reason: "worker offline".into(),
            }
            .proves_not_executed()
        );
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
            EdgeExecDisposition::RejectedBeforeDispatch {
                reason: "pep_rejected:authz".into(),
            },
            EdgeExecDisposition::DispatchFailedBeforeWorker {
                reason: "worker unavailable".into(),
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
            reason: "x".into(),
        })
        .unwrap();
        assert!(
            json.contains("\"kind\":\"rejected_before_dispatch\""),
            "{json}"
        );
    }

    fn sample_plan() -> ExecPlan {
        use crate::RiskLevel;
        use crate::exec::{ApprovalId, ExecPlanDraft, ExecRequestId, ExecShellKind};
        ExecPlan::from_draft(
            ExecRequestId("req-1".into()),
            ApprovalId("appr-1".into()),
            ExecPlanDraft {
                program: "powershell".into(),
                argv: vec!["-Command".into(), "Get-Service".into()],
                cwd: None,
                shell: ExecShellKind::Powershell,
                risk: RiskLevel::Low,
                template_id: "get_service".into(),
                fingerprint: "fp-1".into(),
                timeout_ms: 10_000,
                max_stdout_bytes: 65_536,
                max_stderr_bytes: 65_536,
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
