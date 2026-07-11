//! Temporary-support signaling payloads.
//!
//! Carried in a [`crate::model::signal::SignalingModel`]'s `signaling_data` for the
//! support flow: a desk server asks for a code over its regular `Server` upstream
//! (`RequestSupportCode`), the manager issues a short-lived code bound to that
//! connection's device and pushes it back with [`SupportCodeIssuedData`], and the
//! host asks to revoke it when the local user ends support
//! ([`RevokeSupportCodeData`]). Only a central brain (the manager) mints or revokes;
//! a plain signal never issues codes.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Manager → host: the issued temporary support code and when it expires.
///
/// The host displays `code` to the local user (who reads it out to a supporter)
/// and uses `expires_at` to render a countdown. `expires_at` is a Unix timestamp in
/// **seconds** (server clock), matching the TTL the manager stored in Redis.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SupportCodeIssuedData {
    /// The human-readable support code (uppercase, unambiguous alphabet).
    pub code: String,
    /// Unix seconds at which the code (and the support session) expires.
    pub expires_at: i64,
}

/// Host → manager: revoke the support code the host currently holds, because the
/// local user ended support. The manager verifies the requesting connection owns
/// the code's device before revoking, so it stops being redeemable at once instead
/// of only ageing out on its TTL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RevokeSupportCodeData {
    /// The support code to revoke.
    pub code: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn support_code_issued_round_trips() {
        let original = SupportCodeIssuedData {
            code: "K7P2M9QX4R".to_string(),
            expires_at: 1_800_000_123,
        };
        let json = serde_json::to_string(&original).expect("encode");
        let back: SupportCodeIssuedData = serde_json::from_str(&json).expect("decode");
        assert_eq!(original, back);
    }

    #[test]
    fn revoke_support_code_round_trips() {
        let original = RevokeSupportCodeData {
            code: "K7P2M9QX4R".to_string(),
        };
        let json = serde_json::to_string(&original).expect("encode");
        let back: RevokeSupportCodeData = serde_json::from_str(&json).expect("decode");
        assert_eq!(original, back);
    }
}
