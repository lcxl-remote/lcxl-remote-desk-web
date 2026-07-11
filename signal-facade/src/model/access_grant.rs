//! Wire payload for grant-session revocation.
//!
//! When a device's dial code is regenerated its generation is bumped, superseding
//! every access grant minted at an earlier generation. The central brain (manager)
//! sends the target host a [`RevokeAccessGrantData`] so it direct-closes every
//! in-flight session it holds whose recorded generation is at or below the revoked
//! one — cutting an already-established peer connection immediately rather than
//! waiting for its next `RequestRemote` (which the `authorize` generation check
//! would reject anyway). A plain signal server issues no such teardown.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Manager → host: close every grant session for `target_device` minted at a
/// generation `≤ revoked_generation`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RevokeAccessGrantData {
    /// The device whose grants are being revoked (its external `public_id` /
    /// `client_id`, matching what the host is). Carried for auditing / sanity; the
    /// host serves a single device, so revocation matches on generation.
    pub target_device: String,
    /// The highest generation being revoked. The host closes every grant it holds
    /// whose recorded generation is `≤` this value.
    pub revoked_generation: i64,
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
            reason: "dial_code_regenerated".to_string(),
        };
        let json = serde_json::to_string(&original).expect("encode");
        let back: RevokeAccessGrantData = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, original);
    }
}
