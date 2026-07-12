//! Wire payload for grant-session revocation.
//!
//! The central brain (manager) sends the target host a [`RevokeAccessGrantData`]
//! to direct-close an already-established grant session it holds, cutting the peer
//! connection immediately rather than waiting for its next `RequestRemote` (which
//! the `authorize` generation check would reject anyway). Two granularities share
//! the frame:
//!
//! - **Generation-scoped** (`grant_session_id: None`): after a dial-code
//!   regeneration bumps the device generation, close every session whose recorded
//!   generation is `≤ revoked_generation`.
//! - **Session-scoped** (`grant_session_id: Some(..)`): when the owner ends a
//!   single temporary-support session, close only that one grant session (its other
//!   generation-mates stay up).
//!
//! A plain signal server issues no such teardown.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Manager → host: direct-close grant session(s). When `grant_session_id` is set,
/// close exactly that session; otherwise close every session for `target_device`
/// minted at a generation `≤ revoked_generation`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RevokeAccessGrantData {
    /// The device whose grants are being revoked (its external `public_id` /
    /// `client_id`, matching what the host is). Carried for auditing / sanity; the
    /// host serves a single device, so revocation matches on generation / session.
    pub target_device: String,
    /// The highest generation being revoked. The host closes every grant it holds
    /// whose recorded generation is `≤` this value. Ignored when `grant_session_id`
    /// is set.
    pub revoked_generation: i64,
    /// When set, revoke only this specific grant session (session-scoped teardown,
    /// e.g. one support session the owner ended) rather than a whole generation
    /// range. Absent for the generation-scoped regeneration teardown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_session_id: Option<String>,
    /// A short machine reason (e.g. `"dial_code_regenerated"`) for host-side logs.
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revoke_access_grant_round_trips() {
        let original = RevokeAccessGrantData {
            target_device: "device-public-1".to_string(),
            revoked_generation: 7,
            grant_session_id: None,
            reason: "dial_code_regenerated".to_string(),
        };
        let json = serde_json::to_string(&original).expect("encode");
        let back: RevokeAccessGrantData = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, original);
        // Generation-scoped frames omit the optional session id entirely, so an
        // older host that predates the field still deserializes them.
        assert!(!json.contains("grant_session_id"));
    }

    #[test]
    fn session_scoped_revoke_round_trips() {
        let original = RevokeAccessGrantData {
            target_device: "device-public-1".to_string(),
            revoked_generation: 0,
            grant_session_id: Some("GS-abc".to_string()),
            reason: "support_ended".to_string(),
        };
        let json = serde_json::to_string(&original).expect("encode");
        let back: RevokeAccessGrantData = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, original);
        assert_eq!(back.grant_session_id.as_deref(), Some("GS-abc"));
    }
}
