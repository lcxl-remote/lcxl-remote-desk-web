//! Trusted capability-ceiling stamp for `RequestRemote` frames.
//!
//! A `RequestRemote` reaching a host through a trusted central signaling server
//! is wrapped in an [`AuthorizedRequestRemote`]: the original frame data plus an
//! [`RequestRemoteAuthz`] block the central server stamps. The host only trusts a
//! stamp from its `TrustedCentral` upstream and drops any bare `RequestRemote`
//! arriving there (defense against a grant session stripping its stamp to
//! masquerade as an owner). The `access_ceiling` says what the session may do:
//! `None` is a central-verified owner/full session, `Some(ceiling)` is the
//! per-code ceiling of a redeemed device / support code.
//!
//! This mirrors the trust model of the AI control-frame `AuthorizationBlock`
//! (`desk-agent-protocol`) — TrustedCentral-only injection, validated
//! request_id / audience / expiry — but is a **separate, self-contained type in
//! this crate** rather than an extension of `AuthorizationBlock`: the ceiling is
//! a `SecuritySettings`, a `signal-facade` domain type, and pulling it into the
//! pure low-level `agent-protocol` protocol crate would break that crate's
//! layering. The two authorization blocks stay parallel and are never mixed.

use serde::{Deserialize, Serialize};

use crate::model::security_settings::SecuritySettings;

/// Wire version of the stamp, so an incompatible host rejects rather than
/// misinterprets an unknown-shaped block.
pub const REQUEST_REMOTE_AUTHZ_VERSION: u16 = 1;

/// Why a [`RequestRemoteAuthz`] failed host-side validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestRemoteAuthzError {
    /// `version` is not [`REQUEST_REMOTE_AUTHZ_VERSION`].
    UnsupportedVersion,
    /// The stamped `request_id` does not match the frame it wraps (a replayed /
    /// mismatched stamp).
    RequestIdMismatch,
    /// The stamp was minted for a different host (`audience` mismatch).
    AudienceMismatch,
    /// The stamp is past its `expires_at`.
    Expired,
}

impl std::fmt::Display for RequestRemoteAuthzError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            Self::UnsupportedVersion => "unsupported request-remote authz version",
            Self::RequestIdMismatch => "request-remote authz request_id mismatch",
            Self::AudienceMismatch => "request-remote authz audience mismatch",
            Self::Expired => "request-remote authz expired",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for RequestRemoteAuthzError {}

/// The trusted stamp a central signaling server puts on a `RequestRemote`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestRemoteAuthz {
    /// Wire version. Checked first by [`Self::validate`].
    pub version: u16,
    /// The session's capability ceiling. `None` = central-verified owner/org full
    /// control (no ceiling); `Some(ceiling)` = the redeemed code's per-code
    /// ceiling (a code with no explicit config is an all-`None` ceiling — every
    /// dimension prompts, never a wide-open `None`).
    pub access_ceiling: Option<SecuritySettings>,
    /// The central-validated grant session id, echoed so the host keys its
    /// grant → connections projection (revocation / egress isolation) off the
    /// **stamped** value, never the browser-writable selector. `None` for owner
    /// sessions (which are not revocable grants).
    pub grant_session_id: Option<String>,
    /// The device's live code generation at stamp time, recorded by the host
    /// alongside the grant so a later dial-code regeneration can direct-close every
    /// in-flight session minted at a superseded generation (`generation ≤ revoked`).
    /// Meaningful only for grant sessions (`grant_session_id.is_some()`); an owner
    /// session is never indexed or revoked, so its value is a `0` placeholder.
    ///
    /// `#[serde(default)]` so a stamp serialized before this field existed decodes to
    /// `0` (a during-rollout host reading an older central's stamp treats it as an
    /// un-revocable generation) rather than failing to deserialize the whole wrapper.
    #[serde(default)]
    pub generation: i64,
    /// The `request_id` of the frame this stamp authorizes; must match, so a
    /// stamp cannot be lifted onto a different frame.
    pub request_id: String,
    /// The host this stamp was minted for (its `client_id`); must match the
    /// receiving host.
    pub audience: String,
    /// Absolute expiry as an RFC3339 UTC timestamp, or `None` for no expiry.
    /// Compared lexicographically (valid for `Z`-suffixed UTC RFC3339).
    pub expires_at: Option<String>,
}

impl RequestRemoteAuthz {
    /// Validate the stamp against the frame it wraps and the receiving host.
    /// Mirrors `AuthorizationBlock::validate`: version, request_id, audience, then
    /// expiry (lexicographic RFC3339 compare — correct for `Z`-suffixed UTC).
    pub fn validate(
        &self,
        frame_request_id: &str,
        expected_audience: &str,
        now_rfc3339: &str,
    ) -> Result<(), RequestRemoteAuthzError> {
        if self.version != REQUEST_REMOTE_AUTHZ_VERSION {
            return Err(RequestRemoteAuthzError::UnsupportedVersion);
        }
        if self.request_id != frame_request_id {
            return Err(RequestRemoteAuthzError::RequestIdMismatch);
        }
        if self.audience != expected_audience {
            return Err(RequestRemoteAuthzError::AudienceMismatch);
        }
        if let Some(expires_at) = &self.expires_at
            && expires_at.as_str() <= now_rfc3339
        {
            return Err(RequestRemoteAuthzError::Expired);
        }
        Ok(())
    }
}

/// A `RequestRemote` frame's data wrapped with its trusted stamp. The `inner`
/// value is the original frame data (a serialized [`super::signal::RequestRemoteModel`],
/// with any TURN ICE already injected) kept byte-for-byte so the host unwraps it
/// back unchanged after validating `authz`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthorizedRequestRemote {
    pub inner: serde_json::Value,
    pub authz: RequestRemoteAuthz,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authz() -> RequestRemoteAuthz {
        RequestRemoteAuthz {
            version: REQUEST_REMOTE_AUTHZ_VERSION,
            access_ceiling: None,
            grant_session_id: None,
            generation: 0,
            request_id: "req-1".to_string(),
            audience: "host-client-abc".to_string(),
            expires_at: Some("2999-01-01T00:00:00Z".to_string()),
        }
    }

    #[test]
    fn validate_accepts_a_matching_unexpired_stamp() {
        assert_eq!(
            authz().validate("req-1", "host-client-abc", "2026-01-01T00:00:00Z"),
            Ok(())
        );
    }

    #[test]
    fn validate_rejects_unsupported_version() {
        let mut a = authz();
        a.version = REQUEST_REMOTE_AUTHZ_VERSION + 1;
        assert_eq!(
            a.validate("req-1", "host-client-abc", "2026-01-01T00:00:00Z"),
            Err(RequestRemoteAuthzError::UnsupportedVersion)
        );
    }

    #[test]
    fn validate_rejects_request_id_mismatch() {
        assert_eq!(
            authz().validate("req-OTHER", "host-client-abc", "2026-01-01T00:00:00Z"),
            Err(RequestRemoteAuthzError::RequestIdMismatch)
        );
    }

    #[test]
    fn validate_rejects_audience_mismatch() {
        assert_eq!(
            authz().validate("req-1", "some-other-host", "2026-01-01T00:00:00Z"),
            Err(RequestRemoteAuthzError::AudienceMismatch)
        );
    }

    #[test]
    fn validate_rejects_expired_stamp() {
        let mut a = authz();
        a.expires_at = Some("2020-01-01T00:00:00Z".to_string());
        assert_eq!(
            a.validate("req-1", "host-client-abc", "2026-01-01T00:00:00Z"),
            Err(RequestRemoteAuthzError::Expired)
        );
    }

    #[test]
    fn validate_allows_no_expiry() {
        let mut a = authz();
        a.expires_at = None;
        assert_eq!(
            a.validate("req-1", "host-client-abc", "2026-01-01T00:00:00Z"),
            Ok(())
        );
    }

    #[test]
    fn stamp_without_generation_decodes_to_zero() {
        // A stamp serialized before `generation` existed (an older central during a
        // rolling deploy) must still decode — to a `0` placeholder — rather than
        // failing the whole wrapper.
        let json = r#"{
            "version": 1,
            "access_ceiling": null,
            "grant_session_id": null,
            "request_id": "req-1",
            "audience": "host-client-abc",
            "expires_at": null
        }"#;
        let decoded: RequestRemoteAuthz = serde_json::from_str(json).expect("decode");
        assert_eq!(decoded.generation, 0);
    }

    #[test]
    fn wrapper_round_trips_through_json() {
        let wrapped = AuthorizedRequestRemote {
            inner: serde_json::json!({ "ice_servers": [], "grant_session_id": "GS1" }),
            authz: authz(),
        };
        let json = serde_json::to_string(&wrapped).unwrap();
        let back: AuthorizedRequestRemote = serde_json::from_str(&json).unwrap();
        assert_eq!(back, wrapped);
    }
}
