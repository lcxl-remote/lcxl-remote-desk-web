//! Manager credential proof carried by a successful application heartbeat.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Proof that the manager revalidated the API token bound to this WebSocket.
///
/// Correlation remains in the surrounding signaling envelope. The payload
/// intentionally carries no token, account, clock, or host-local lease data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ManagerCredentialHeartbeatProof {
    pub protocol: u8,
}

impl ManagerCredentialHeartbeatProof {
    pub const PROTOCOL_V1: u8 = 1;

    pub const fn v1() -> Self {
        Self {
            protocol: Self::PROTOCOL_V1,
        }
    }

    pub const fn is_supported(self) -> bool {
        self.protocol == Self::PROTOCOL_V1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_v1_has_a_minimal_stable_json_shape() {
        let encoded = serde_json::to_string(&ManagerCredentialHeartbeatProof::v1()).unwrap();
        assert_eq!(encoded, r#"{"protocol":1}"#);
        let decoded: ManagerCredentialHeartbeatProof =
            serde_json::from_str(&encoded).expect("decode proof");
        assert!(decoded.is_supported());
        assert!(!ManagerCredentialHeartbeatProof { protocol: 2 }.is_supported());
    }
}
