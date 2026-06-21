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

use crate::AgentOutcome;

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
}
