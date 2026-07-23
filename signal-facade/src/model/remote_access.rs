use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HostRemoteAccessLockRequest {
    pub request_id: String,
    pub lock_id: Option<String>,
    pub state_version: u64,
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HostRemoteAccessLockAck {
    pub request_id: String,
    pub lock_id: Option<String>,
    pub state_version: u64,
    pub locked: bool,
    pub generation: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TerminateRemotePeerRequest {
    pub operation_id: String,
    pub target_connection_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PeerEvictionOutcome {
    Delivered,
    Scheduled,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TerminateRemotePeerAck {
    pub operation_id: String,
    pub target_connection_id: String,
    pub outcome: PeerEvictionOutcome,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_request_and_ack_round_trip() {
        let request = HostRemoteAccessLockRequest {
            request_id: "req-1".into(),
            lock_id: Some("lock-1".into()),
            state_version: 7,
            locked: true,
        };
        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(
            serde_json::from_str::<HostRemoteAccessLockRequest>(&encoded).unwrap(),
            request
        );

        let ack = HostRemoteAccessLockAck {
            request_id: "req-1".into(),
            lock_id: Some("lock-1".into()),
            state_version: 7,
            locked: true,
            generation: 3,
        };
        let encoded = serde_json::to_string(&ack).unwrap();
        assert_eq!(
            serde_json::from_str::<HostRemoteAccessLockAck>(&encoded).unwrap(),
            ack
        );
    }
}
