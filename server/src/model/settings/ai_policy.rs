//! Edge-local AI execution policy.
//!
//! AI model provider configuration (credentials, base URL, model name, output
//! format) lives on the **central signaling brain**, not on this host. The edge
//! keeps only [`AiExecutionPolicy`]: the local ceiling on how far a centrally
//! authorized AI action may go on this device. It is a top-level field of
//! [`crate::model::settings::Settings`] and carries no secret, so there is no
//! redaction boundary to maintain here (unlike the credentials it replaced).
//!
//! The mode is an upper bound: a central authorization's mode is narrowed by the
//! local one via `restrict_to`, never widened (see the confirm-execution router).

use desk_agent_protocol::ExecutionMode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Whether an [`ExecutionMode`] is one of the three the confirm-execute flow
/// supports. `SessionApproved` / `Automated` are frozen in the protocol enum but
/// not selectable yet (they need a future policy engine); persisting them is
/// rejected so the stored mode stays in the usable set.
fn is_m2_selectable(mode: ExecutionMode) -> bool {
    matches!(
        mode,
        ExecutionMode::SuggestOnly | ExecutionMode::ReadOnly | ExecutionMode::ConfirmEachAction
    )
}

/// Default ceiling on commands running at once on this device. Low on purpose:
/// AI-driven execution is interactive and approved one command at a time, so
/// several at once is already unusual, and the ceiling exists to bound a caller
/// that ignores that rather than to size normal work.
pub const DEFAULT_MAX_CONCURRENT_EXECUTIONS: u32 = 4;

/// Persisted edge-local AI execution policy.
///
/// Holds no model credentials (those live on the central brain); the fields are
/// the local execution ceiling and how much may run at once.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct AiExecutionPolicy {
    /// How far the AI may go in acting on the device. Default `suggest_only`
    /// (the AI only proposes commands). `read_only` / `confirm_each_action`
    /// permit confirmed execution of whitelist templates; every real execution
    /// still requires an explicit per-command user approval. `session_approved`
    /// additionally lets the first approval of a template stand for the rest of
    /// the connection's session. `automated` (run without any confirmation) is
    /// not implemented and is refused. On a central link this caps the centrally
    /// granted mode; it never widens it.
    pub execution_mode: ExecutionMode,
    /// How many commands may run concurrently on this device, across every
    /// caller.
    ///
    /// Enforced by the host itself rather than trusted to the caller. A central
    /// manager also schedules against its own quota, but that binds only work the
    /// manager dispatched — a control end reaching this device through an
    /// open-source signal server bypasses it entirely, so the device keeps its own
    /// ceiling.
    pub max_concurrent_executions: u32,
}

impl Default for AiExecutionPolicy {
    fn default() -> Self {
        Self {
            execution_mode: ExecutionMode::default(),
            max_concurrent_executions: DEFAULT_MAX_CONCURRENT_EXECUTIONS,
        }
    }
}

impl AiExecutionPolicy {
    /// Project the public view returned by the query endpoint.
    pub fn public_view(&self) -> AiExecutionPolicyPublic {
        AiExecutionPolicyPublic {
            execution_mode: self.execution_mode,
        }
    }

    /// Apply an update in place. `None` leaves the stored mode unchanged; a
    /// not-yet-selectable mode (`session_approved` / `automated`) is ignored so
    /// the persisted value stays in the usable set.
    pub fn apply_update(&mut self, update: AiExecutionPolicyUpdate) {
        if let Some(execution_mode) = update.execution_mode
            && is_m2_selectable(execution_mode)
        {
            self.execution_mode = execution_mode;
        }
    }
}

/// Public view returned by `GET /api/desk/settings/ai-policy`. Carries no
/// secret (the policy never held one).
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema, Default)]
pub struct AiExecutionPolicyPublic {
    pub execution_mode: ExecutionMode,
}

/// Update body for `POST /api/desk/settings/ai-policy`.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema, Default)]
pub struct AiExecutionPolicyUpdate {
    /// `None` leaves the stored mode unchanged. A not-yet-selectable mode
    /// (`session_approved` / `automated`) is ignored.
    pub execution_mode: Option<ExecutionMode>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default execution mode is `suggest_only`, and a config written before the
    /// field existed deserializes to it (via `#[serde(default)]`).
    #[test]
    fn execution_mode_defaults_to_suggest_only() {
        assert_eq!(
            AiExecutionPolicy::default().execution_mode,
            ExecutionMode::SuggestOnly
        );
        let s: AiExecutionPolicy = serde_json::from_str("{}").expect("empty config");
        assert_eq!(s.execution_mode, ExecutionMode::SuggestOnly);
    }

    /// Update accepts the three selectable modes and ignores the
    /// not-yet-selectable ones, so the persisted value never leaves the usable
    /// set. `None` leaves the stored mode unchanged.
    #[test]
    fn update_execution_mode_rejects_non_selectable() {
        let mut s = AiExecutionPolicy::default();

        for mode in [
            ExecutionMode::ReadOnly,
            ExecutionMode::ConfirmEachAction,
            ExecutionMode::SuggestOnly,
        ] {
            s.apply_update(AiExecutionPolicyUpdate {
                execution_mode: Some(mode),
            });
            assert_eq!(s.execution_mode, mode);
        }

        s.apply_update(AiExecutionPolicyUpdate {
            execution_mode: Some(ExecutionMode::ConfirmEachAction),
        });
        for mode in [ExecutionMode::SessionApproved, ExecutionMode::Automated] {
            s.apply_update(AiExecutionPolicyUpdate {
                execution_mode: Some(mode),
            });
            assert_eq!(
                s.execution_mode,
                ExecutionMode::ConfirmEachAction,
                "not-selectable mode {mode:?} must not be persisted"
            );
        }

        // None leaves the stored mode unchanged.
        s.apply_update(AiExecutionPolicyUpdate::default());
        assert_eq!(s.execution_mode, ExecutionMode::ConfirmEachAction);
    }

    /// The public view carries the execution mode (it is not a secret).
    #[test]
    fn public_view_reports_execution_mode() {
        let mut s = AiExecutionPolicy::default();
        s.apply_update(AiExecutionPolicyUpdate {
            execution_mode: Some(ExecutionMode::ConfirmEachAction),
        });
        assert_eq!(
            s.public_view().execution_mode,
            ExecutionMode::ConfirmEachAction
        );
    }
}
