//! Compiled personal-owner Assistant policy shared by OSS and Manager.
//!
//! This is not the organization/fleet policy counter or a grant schema version.
//! Bump it when the compiled personal Assistant authorization contract changes.
//! A new user turn may adopt it; old grants, decisions and action receipts must
//! retain their original authority. Runtime readiness and revocation are still
//! checked independently, even when this revision matches.

use crate::{
    seam::ClaimTurnParams,
    session::{AgentSessionSurface, TriggerOrigin},
};
use desk_agent_protocol::{AgentError, AgentErrorKind};

pub const PERSONAL_ASSISTANT_POLICY_REVISION: i64 = 1;

pub fn require_current_policy(revision: i64) -> Result<(), AgentError> {
    if revision == PERSONAL_ASSISTANT_POLICY_REVISION {
        Ok(())
    } else {
        Err(AgentError {
            kind: AgentErrorKind::SessionUnavailable,
            message: "Assistant policy changed; synchronize and send a new request".into(),
            retryable: false,
            safe_for_model: false,
            error_code: None,
        })
    }
}

/// Call before changing any persisted turn/lease state. Only explicit user
/// input can start a turn under a different compiled contract; an automatic
/// continuation must keep the existing session's current policy unchanged.
pub fn validate_claim(
    surface: AgentSessionSurface,
    previous_revision: Option<i64>,
    params: &ClaimTurnParams,
) -> Result<(), AgentError> {
    if surface != AgentSessionSurface::DeviceAssistant {
        return Ok(());
    }
    require_current_policy(params.policy_revision)?;
    if params.trigger_origin != TriggerOrigin::User {
        require_current_policy(previous_revision.unwrap_or(0))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{AgentScope, ExecutionMode};

    #[test]
    fn only_user_input_may_adopt_the_compiled_personal_policy() {
        let mut claim = ClaimTurnParams {
            conversation_id: "run".into(),
            actor_id: "owner".into(),
            device_id: "device".into(),
            policy_revision: PERSONAL_ASSISTANT_POLICY_REVISION,
            current_pdp_scope: AgentScope {
                granted: vec![],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            turn_id: "turn".into(),
            request_id: None,
            connection_id: None,
            now: "2026-08-30T00:00:00Z".into(),
            trigger_origin: TriggerOrigin::User,
        };
        for previous in [None, Some(0), Some(1), Some(2)] {
            assert!(validate_claim(AgentSessionSurface::DeviceAssistant, previous, &claim).is_ok());
            claim.trigger_origin = TriggerOrigin::PermissionDecision;
            assert_eq!(
                validate_claim(AgentSessionSurface::DeviceAssistant, previous, &claim).is_ok(),
                previous == Some(PERSONAL_ASSISTANT_POLICY_REVISION)
            );
            claim.trigger_origin = TriggerOrigin::ExecCompletion;
            assert_eq!(
                validate_claim(AgentSessionSurface::DeviceAssistant, previous, &claim).is_ok(),
                previous == Some(PERSONAL_ASSISTANT_POLICY_REVISION)
            );
            claim.trigger_origin = TriggerOrigin::User;
        }
        for invalid in [-1, 0, 2, i64::MAX] {
            claim.policy_revision = invalid;
            assert!(
                validate_claim(AgentSessionSurface::DeviceAssistant, Some(invalid), &claim)
                    .is_err()
            );
            assert!(
                validate_claim(AgentSessionSurface::TerminalCopilot, Some(invalid), &claim).is_ok()
            );
        }
    }
}
