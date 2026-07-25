//! Manager → daemon authorization carrier.
//!
//! In the fleet topology the manager is the policy decision point (PDP) and the
//! desk-server daemon is the policy enforcement point (PEP). The manager relays
//! the control-end AI frames to the daemon; to carry the authorization decision
//! without polluting the public control payloads (which must structurally not
//! carry trusted fields), it wraps the original payload in an
//! [`AuthorizedControlPayload`] whose `authz` is an [`AuthorizationBlock`].
//!
//! This block is **internal manager↔daemon plumbing**, distinct from the frozen
//! [`crate::AgentScope`]: it additionally carries `max_risk`, orchestrator-layer
//! grants (which cannot live in the closed `Capability` enum), the resolved
//! actor/device identity, and replay-binding fields. The daemon validates the
//! binding before trusting the decision; the bare control payload is never
//! mutated.

use serde::{Deserialize, Serialize};

use crate::{AgentScope, RiskLevel};

/// Current `AuthorizationBlock` wire version. The daemon rejects blocks whose
/// version it does not understand.
pub const AUTHORIZATION_BLOCK_VERSION: u16 = 1;

/// Whether an authorized exec request must match a command template or may
/// fall back to an owner-confirmed free-form command.
///
/// This is a per-request PDP decision, not a persisted feature switch. Missing
/// fields always deserialize to [`TemplateOnly`](Self::TemplateOnly).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecAdmissionPolicy {
    /// Only commands rendered from a built-in or operator template may execute.
    #[default]
    TemplateOnly,
    /// The authenticated device owner may approve one off-template command.
    OwnerInteractive,
}

/// Resolved actor identity (the control end / operator), from the manager's
/// validated connection `AuthContext` — never self-reported by the control end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthzActor {
    pub user_id: Option<i32>,
}

/// Resolved target device identity, from the manager's device registry binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthzDevice {
    pub device_id: Option<i32>,
}

/// The authorization decision the manager (PDP) injects for the daemon (PEP).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizationBlock {
    /// Wire version; the daemon rejects unknown versions.
    pub version: u16,
    /// Granted device capabilities + execution mode (frozen scope shape).
    pub scope: AgentScope,
    /// Allowed orchestrator-layer permissions (e.g. `ai.diagnose`, `shell.plan`)
    /// that cannot be expressed in the closed `Capability` enum.
    pub orchestrator_grants: Vec<String>,
    /// Maximum risk the exec path may reach (ConfirmExec gate).
    pub max_risk: RiskLevel,
    /// Exec classification policy resolved by the trusted central PDP.
    #[serde(default)]
    pub exec_admission_policy: ExecAdmissionPolicy,
    /// Resolved actor identity.
    pub actor: AuthzActor,
    /// Resolved device identity.
    pub device: AuthzDevice,
    /// The request id this block authorizes; must match the frame it rides on.
    pub request_id: String,
    /// Optional session correlation.
    pub session_id: Option<String>,
    /// RFC3339 expiry; the daemon rejects an expired block.
    pub expires_at: Option<String>,
    /// Issuer identity (the manager node).
    pub issuer: String,
    /// Intended audience (the target daemon / device); the daemon rejects a
    /// mismatch so a block cannot be replayed against a different device.
    pub audience: String,
    /// Optional HMAC seal (future hardening; transport trust is the baseline).
    pub signature: Option<String>,
}

/// Why an [`AuthorizationBlock`] failed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthzValidationError {
    /// The block version is not understood by this daemon.
    UnsupportedVersion,
    /// The block's `request_id` does not match the frame it rode on.
    RequestIdMismatch,
    /// The block has expired.
    Expired,
    /// The block's `audience` is not this daemon/device.
    AudienceMismatch,
}

impl AuthorizationBlock {
    /// Validate the block against the frame it arrived with and this daemon's
    /// identity. `now_rfc3339` is the current time as an RFC3339 string;
    /// `expected_audience` is this daemon's audience id.
    ///
    /// Transport trust is the baseline (the daemon's only upstream is the
    /// manager link), so a missing `signature` is accepted; when present it is
    /// reserved for future verification and not yet checked here.
    pub fn validate(
        &self,
        frame_request_id: &str,
        expected_audience: &str,
        now_rfc3339: &str,
    ) -> Result<(), AuthzValidationError> {
        if self.version != AUTHORIZATION_BLOCK_VERSION {
            return Err(AuthzValidationError::UnsupportedVersion);
        }
        if self.request_id != frame_request_id {
            return Err(AuthzValidationError::RequestIdMismatch);
        }
        if self.audience != expected_audience {
            return Err(AuthzValidationError::AudienceMismatch);
        }
        if let Some(expires_at) = &self.expires_at
            && expires_at.as_str() <= now_rfc3339
        {
            // RFC3339 timestamps in UTC ("...Z") are lexicographically ordered,
            // which the manager always emits; compare as strings to keep this
            // crate free of a datetime dependency.
            return Err(AuthzValidationError::Expired);
        }
        Ok(())
    }
}

/// Manager → daemon wrapper: the original control payload plus the manager's
/// authorization decision. The `inner` payload is byte-for-byte the public
/// control type (`AgentRequestData` / `DiagnoseRequestData` / `ConfirmExecData`)
/// — it is never mutated, preserving the "control end carries no trusted field"
/// invariant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthorizedControlPayload<T> {
    pub inner: T,
    pub authz: AuthorizationBlock,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ExecutionMode;

    fn block() -> AuthorizationBlock {
        AuthorizationBlock {
            version: AUTHORIZATION_BLOCK_VERSION,
            scope: AgentScope {
                granted: Vec::new(),
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: Some("p1".to_string()),
            },
            orchestrator_grants: vec!["ai.diagnose".to_string()],
            max_risk: RiskLevel::Medium,
            exec_admission_policy: ExecAdmissionPolicy::TemplateOnly,
            actor: AuthzActor { user_id: Some(7) },
            device: AuthzDevice {
                device_id: Some(42),
            },
            request_id: "req-1".to_string(),
            session_id: Some("sess-1".to_string()),
            expires_at: Some("2999-01-01T00:00:00Z".to_string()),
            issuer: "manager-1".to_string(),
            audience: "device-42".to_string(),
            signature: None,
        }
    }

    #[test]
    fn valid_block_passes() {
        let b = block();
        assert!(
            b.validate("req-1", "device-42", "2026-06-14T00:00:00Z")
                .is_ok()
        );
    }

    #[test]
    fn unsupported_version_rejected() {
        let mut b = block();
        b.version = 999;
        assert_eq!(
            b.validate("req-1", "device-42", "2026-06-14T00:00:00Z"),
            Err(AuthzValidationError::UnsupportedVersion)
        );
    }

    #[test]
    fn wrong_request_id_rejected() {
        let b = block();
        assert_eq!(
            b.validate("other", "device-42", "2026-06-14T00:00:00Z"),
            Err(AuthzValidationError::RequestIdMismatch)
        );
    }

    #[test]
    fn wrong_audience_rejected() {
        let b = block();
        assert_eq!(
            b.validate("req-1", "device-99", "2026-06-14T00:00:00Z"),
            Err(AuthzValidationError::AudienceMismatch)
        );
    }

    #[test]
    fn expired_block_rejected() {
        let mut b = block();
        b.expires_at = Some("2020-01-01T00:00:00Z".to_string());
        assert_eq!(
            b.validate("req-1", "device-42", "2026-06-14T00:00:00Z"),
            Err(AuthzValidationError::Expired)
        );
    }

    #[test]
    fn wrapper_round_trips_and_preserves_inner() {
        // The inner payload is preserved verbatim through a serde round-trip.
        let wrapper = AuthorizedControlPayload {
            inner: crate::diagnose::DiagnoseRequestData {
                question: "why slow?".to_string(),
                ..Default::default()
            },
            authz: block(),
        };
        let json = serde_json::to_string(&wrapper).expect("encode");
        let back: AuthorizedControlPayload<crate::diagnose::DiagnoseRequestData> =
            serde_json::from_str(&json).expect("decode");
        assert_eq!(back.inner.question, "why slow?");
        assert_eq!(back.authz, block());
    }

    #[test]
    fn legacy_block_without_exec_admission_policy_defaults_to_template_only() {
        let mut value = serde_json::to_value(block()).expect("encode");
        value
            .as_object_mut()
            .expect("block object")
            .remove("exec_admission_policy");
        let decoded: AuthorizationBlock = serde_json::from_value(value).expect("decode");
        assert_eq!(
            decoded.exec_admission_policy,
            ExecAdmissionPolicy::TemplateOnly
        );
    }

    #[test]
    fn legacy_reader_can_ignore_the_new_exec_admission_policy_field() {
        #[derive(Deserialize)]
        struct LegacyAuthorizationBlock {
            version: u16,
            request_id: String,
        }

        let value = serde_json::to_value(block()).expect("encode");
        let decoded: LegacyAuthorizationBlock = serde_json::from_value(value).expect("decode");
        assert_eq!(decoded.version, AUTHORIZATION_BLOCK_VERSION);
        assert_eq!(decoded.request_id, "req-1");
    }
}
