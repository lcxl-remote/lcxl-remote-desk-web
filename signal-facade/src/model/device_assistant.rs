use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Device Assistant product features implemented by one server target.
///
/// Authentication, ownership, target readiness and grants remain request-time
/// checks. Clients gate every optional control independently and treat an absent
/// profile as unsupported.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, ToSchema)]
pub struct DeviceAssistantClientCapabilities {
    pub schema_version: u16,
    pub turn_stream: bool,
    pub capability_inventory: bool,
    pub full_session_snapshot: bool,
    pub permission_decision: bool,
    pub grant_revoke: bool,
    pub background_task_cancel: bool,
    pub unknown_outcome_disposition: bool,
    pub object_context: bool,
    /// This server exposes the dedicated one-shot exec-PTY carrier surface.
    /// Per-device/session readiness is still proven by a successful prepare.
    pub exec_pty: bool,
}

impl DeviceAssistantClientCapabilities {
    pub const SCHEMA_VERSION: u16 = 1;

    pub const fn oss() -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            turn_stream: true,
            capability_inventory: true,
            full_session_snapshot: true,
            permission_decision: true,
            grant_revoke: true,
            background_task_cancel: true,
            unknown_outcome_disposition: true,
            object_context: true,
            exec_pty: true,
        }
    }

    pub const fn manager_current() -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            turn_stream: true,
            capability_inventory: true,
            full_session_snapshot: true,
            permission_decision: true,
            grant_revoke: true,
            background_task_cancel: true,
            unknown_outcome_disposition: true,
            object_context: true,
            exec_pty: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_keep_one_shape_and_gate_controls_independently() {
        let oss = serde_json::to_value(DeviceAssistantClientCapabilities::oss()).unwrap();
        let manager =
            serde_json::to_value(DeviceAssistantClientCapabilities::manager_current()).unwrap();
        let mut oss_keys = oss.as_object().unwrap().keys().collect::<Vec<_>>();
        let mut manager_keys = manager.as_object().unwrap().keys().collect::<Vec<_>>();
        oss_keys.sort();
        manager_keys.sort();
        assert_eq!(oss_keys, manager_keys);
        assert_eq!(manager["turn_stream"], true);
        assert_eq!(manager["permission_decision"], true);
        assert_eq!(manager["grant_revoke"], true);
        assert_eq!(manager["background_task_cancel"], true);
        assert_eq!(manager["object_context"], true);
    }
}
