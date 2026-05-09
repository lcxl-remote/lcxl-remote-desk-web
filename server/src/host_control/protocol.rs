//! Host control wire protocol shared by Local / Aggregator / Forwarder hubs and the
//! Tauri / forwarder clients that connect to them.
//!
//! A single `HostControlMessage` enum is used in both directions. Each role only
//! sends / receives a subset of variants; receivers should be tolerant of others
//! (log + ignore).

use serde::{Deserialize, Serialize};

use crate::model::security_approval::SecurityPermissionType;

/// Identifies the role of a client connecting to the hub's WS endpoint.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClientRole {
    /// Tauri UI shell (consumer of commands, producer of state events).
    Tauri,
    /// Worker forwarder (producer of business-side commands and approval requests;
    /// only valid on the aggregator's `/ws/host_upstream` endpoint).
    Forwarder,
}

impl Default for ClientRole {
    fn default() -> Self {
        Self::Tauri
    }
}

/// Service installation operation kind.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ServiceOpKind {
    Install,
    Uninstall,
}

/// Approval request payload pushed to the Tauri shell.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    pub req_id: String,
    pub permission_type: SecurityPermissionType,
    pub from_connection_id: Option<String>,
}

/// User's response to an approval request.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApprovalResponse {
    pub approved: bool,
    pub remember: bool,
}

impl ApprovalResponse {
    pub fn deny() -> Self {
        Self {
            approved: false,
            remember: false,
        }
    }
}

/// Unified host control wire message.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type")]
pub enum HostControlMessage {
    // ========================= Server → Tauri =========================
    /// One-time auto-login token, pushed immediately after the handshake.
    TauriToken { token: String },

    /// Show private-screen overlay.
    PrivateScreenShow { connection_id: String },

    /// Hide private-screen overlay.
    PrivateScreenHide { connection_id: String },

    /// Show whiteboard overlay.
    WhiteboardShow { connection_id: String },

    /// Forward a whiteboard drawing message.
    WhiteboardDraw {
        #[serde(default)]
        connection_id: String,
        message: serde_json::Value,
    },

    /// Hide whiteboard overlay (data channel closed).
    WhiteboardHide { connection_id: String },

    /// Request the user to approve a security-sensitive operation.
    SecurityApprovalRequest {
        req_id: String,
        permission_type: SecurityPermissionType,
        from_connection_id: Option<String>,
    },

    /// Notify Tauri that a previously requested approval has finished
    /// (resolved by the user or cancelled by the server). The Tauri shell
    /// uses this to release UI affordances (e.g. always-on-top) once the
    /// last pending dialog is gone.
    SecurityApprovalFinished { req_id: String },

    /// Service install / uninstall (UAC elevation needed on the Tauri side).
    ServiceOp {
        op: ServiceOpKind,
        install_path: Option<String>,
    },

    // ====================== Aggregator → Forwarder ======================
    /// Deliver the user's approval response back to the worker.
    SecurityApprovalSubmit {
        req_id: String,
        approved: bool,
        remember: bool,
    },

    /// Cancel an in-flight approval (e.g. Tauri shell offline).
    SecurityApprovalCancel { req_id: String },

    /// Forward Tauri-reported state changes to the worker.
    PrivateScreenStateChangedToWorker {
        connection_id: String,
        visible: bool,
    },

    // ========================= Client → Server =========================
    /// Handshake message announcing role + admin status.
    Ready {
        #[serde(default)]
        role: ClientRole,
        #[serde(default)]
        is_admin: Option<bool>,
    },

    /// Tauri reports a private-screen visibility change.
    PrivateScreenStateChanged {
        connection_id: String,
        visible: bool,
    },

    // ====================== Forwarder → Aggregator ======================
    /// Worker business side has resolved an approval (user response or local cancel) —
    /// the aggregator should clean up its routing/replay tables.
    SecurityApprovalResolved { req_id: String },
}

impl HostControlMessage {
    /// True if this variant is a command / request that should be cached for replay
    /// when a Tauri client connects after the message was originally produced.
    ///
    /// Currently only approval requests are replayed — transient overlay commands
    /// (PrivateScreenShow, WhiteboardDraw, …) are not, because their state will be
    /// re-derived from the underlying business flow on reconnect.
    pub fn is_replayable_for_tauri(&self) -> bool {
        matches!(self, Self::SecurityApprovalRequest { .. })
    }

    /// Extract the `req_id` if this message carries one.
    pub fn req_id(&self) -> Option<&str> {
        match self {
            Self::SecurityApprovalRequest { req_id, .. }
            | Self::SecurityApprovalFinished { req_id }
            | Self::SecurityApprovalSubmit { req_id, .. }
            | Self::SecurityApprovalCancel { req_id }
            | Self::SecurityApprovalResolved { req_id } => Some(req_id.as_str()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(msg: &HostControlMessage) -> HostControlMessage {
        let json = serde_json::to_string(msg).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    // U-1: HostControlMessage serde round-trip for every variant.
    #[test]
    fn u1_round_trip_all_variants() {
        let cases = vec![
            HostControlMessage::TauriToken {
                token: "tok-123".to_string(),
            },
            HostControlMessage::PrivateScreenShow {
                connection_id: "c1".to_string(),
            },
            HostControlMessage::PrivateScreenHide {
                connection_id: "c1".to_string(),
            },
            HostControlMessage::WhiteboardShow {
                connection_id: "c1".to_string(),
            },
            HostControlMessage::WhiteboardDraw {
                connection_id: "c1".to_string(),
                message: serde_json::json!({"action": "stroke", "points": [1, 2, 3]}),
            },
            HostControlMessage::WhiteboardHide {
                connection_id: "c1".to_string(),
            },
            HostControlMessage::SecurityApprovalRequest {
                req_id: "r1".to_string(),
                permission_type: SecurityPermissionType::RemoteControl,
                from_connection_id: Some("c1".to_string()),
            },
            HostControlMessage::SecurityApprovalFinished {
                req_id: "r1".to_string(),
            },
            HostControlMessage::SecurityApprovalSubmit {
                req_id: "r1".to_string(),
                approved: true,
                remember: false,
            },
            HostControlMessage::SecurityApprovalCancel {
                req_id: "r1".to_string(),
            },
            HostControlMessage::PrivateScreenStateChangedToWorker {
                connection_id: "c1".to_string(),
                visible: true,
            },
            HostControlMessage::ServiceOp {
                op: ServiceOpKind::Install,
                install_path: Some("C:\\Program Files\\app".to_string()),
            },
            HostControlMessage::ServiceOp {
                op: ServiceOpKind::Uninstall,
                install_path: None,
            },
            HostControlMessage::Ready {
                role: ClientRole::Tauri,
                is_admin: Some(true),
            },
            HostControlMessage::Ready {
                role: ClientRole::Forwarder,
                is_admin: None,
            },
            HostControlMessage::PrivateScreenStateChanged {
                connection_id: "c1".to_string(),
                visible: false,
            },
            HostControlMessage::SecurityApprovalResolved {
                req_id: "r1".to_string(),
            },
        ];
        for case in &cases {
            let restored = round_trip(case);
            assert_eq!(case, &restored, "round-trip mismatch for {case:?}");
        }
    }

    // U-1 (extra): unknown fields are tolerated (forward compatibility).
    #[test]
    fn u1_unknown_fields_tolerated() {
        let json = r#"{"type":"TauriToken","token":"abc","unknown_field":42}"#;
        let msg: HostControlMessage = serde_json::from_str(json).expect("should ignore unknown");
        assert!(matches!(msg, HostControlMessage::TauriToken { .. }));
    }

    // U-2: ApprovalRequest missing required fields fails to deserialize.
    #[test]
    fn u2_missing_required_field_errors() {
        let json = r#"{"req_id":"r1"}"#;
        let parsed: Result<ApprovalRequest, _> = serde_json::from_str(json);
        assert!(parsed.is_err(), "should reject missing permission_type");
    }

    // U-2 (extra): ApprovalRequest with extra fields succeeds (extra ignored).
    #[test]
    fn u2_extra_fields_tolerated() {
        let json = r#"{
            "req_id":"r1",
            "permission_type":"RemoteControl",
            "from_connection_id":null,
            "extra_field":"junk"
        }"#;
        let req: ApprovalRequest = serde_json::from_str(json).expect("should accept extras");
        assert_eq!(req.req_id, "r1");
    }

    // Wire compatibility: SecurityPermissionType serializes to bare string variants
    // matching the old format!("{:?}", v) wire format used by daemon/tauri_ipc.rs.
    #[test]
    fn wire_compat_permission_type_strings() {
        let json = serde_json::to_string(&SecurityPermissionType::RemoteControl).unwrap();
        assert_eq!(json, "\"RemoteControl\"");
        let json = serde_json::to_string(&SecurityPermissionType::FileTransfer).unwrap();
        assert_eq!(json, "\"FileTransfer\"");
    }

    // Ready handshake without role field defaults to Tauri (backward compat with
    // legacy Tauri shells that pre-date the role field).
    #[test]
    fn ready_default_role_is_tauri() {
        let json = r#"{"type":"Ready","is_admin":true}"#;
        let msg: HostControlMessage = serde_json::from_str(json).unwrap();
        match msg {
            HostControlMessage::Ready { role, is_admin } => {
                assert_eq!(role, ClientRole::Tauri);
                assert_eq!(is_admin, Some(true));
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn replay_filter_only_approval_requests() {
        let approval = HostControlMessage::SecurityApprovalRequest {
            req_id: "r1".to_string(),
            permission_type: SecurityPermissionType::RemoteControl,
            from_connection_id: None,
        };
        assert!(approval.is_replayable_for_tauri());

        let show = HostControlMessage::PrivateScreenShow {
            connection_id: "c1".to_string(),
        };
        assert!(!show.is_replayable_for_tauri());
    }

    #[test]
    fn req_id_extraction() {
        let msg = HostControlMessage::SecurityApprovalCancel {
            req_id: "r1".to_string(),
        };
        assert_eq!(msg.req_id(), Some("r1"));

        let msg = HostControlMessage::SecurityApprovalFinished {
            req_id: "rf".to_string(),
        };
        assert_eq!(msg.req_id(), Some("rf"));

        let msg = HostControlMessage::PrivateScreenShow {
            connection_id: "c1".to_string(),
        };
        assert_eq!(msg.req_id(), None);
    }
}
