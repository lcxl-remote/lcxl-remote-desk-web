//! Host control wire protocol shared by Local / Aggregator / Forwarder hubs and the
//! Tauri / forwarder clients that connect to them.
//!
//! A single `HostControlMessage` enum is used in both directions. Each role only
//! sends / receives a subset of variants; receivers should be tolerant of others
//! (log + ignore).

use serde::{Deserialize, Serialize};

use crate::model::security_approval::SecurityPermissionType;
use desk_signal_facade::model::request_remote_authz::ActorSummary;

pub const SESSION_SHELL_PROTOCOL_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct EnvironmentEntryBase64 {
    pub key_base64: String,
    pub value_base64: String,
}

/// Byte-safe Linux desktop-session context reported by the trusted Tauri shell.
///
/// Environment values are intentionally opaque base64 strings. Keep `Debug`
/// redacted so a rejected frame cannot copy session secrets into daemon logs.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct SessionShellInfo {
    pub app_version: String,
    pub protocol_version: u32,
    pub pid: u32,
    pub process_start_ticks: u64,
    pub reported_uid: u32,
    pub session_id: Option<String>,
    pub seat: Option<String>,
    pub session_type: Option<String>,
    pub cwd_base64: String,
    pub umask: u32,
    pub environment: Vec<EnvironmentEntryBase64>,
}

impl std::fmt::Debug for SessionShellInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionShellInfo")
            .field("app_version", &self.app_version)
            .field("protocol_version", &self.protocol_version)
            .field("pid", &self.pid)
            .field("process_start_ticks", &self.process_start_ticks)
            .field("reported_uid", &self.reported_uid)
            .field("session_id", &self.session_id)
            .field("seat", &self.seat)
            .field("session_type", &self.session_type)
            .field("cwd_bytes", &"<redacted>")
            .field("umask", &format_args!("{:04o}", self.umask))
            .field("environment_entries", &self.environment.len())
            .finish()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionShellRegistrationError {
    Unsupported,
    RoleRequired,
    InvalidPayload,
    IdentityMismatch,
    SessionConflict,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostFileTransferDirection {
    Upload,
    Download,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct HostFileTransferSummary {
    pub transfer_id: String,
    pub direction: HostFileTransferDirection,
    pub file_name: String,
    pub transferred_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct HostAccessSession {
    pub connection_id: String,
    pub actor: ActorSummary,
    pub started_at: String,
    pub desktop_view: bool,
    pub system_audio_capture: bool,
    pub remote_control: bool,
    pub terminal_count: u32,
    pub file_manager: bool,
    pub transfers: Vec<HostFileTransferSummary>,
}

impl HostAccessSession {
    pub fn is_active(&self) -> bool {
        self.desktop_view
            || self.system_audio_capture
            || self.remote_control
            || self.terminal_count > 0
            || self.file_manager
            || !self.transfers.is_empty()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostRemoteAccessMode {
    Unlocked,
    Locked,
    RecoveryLocked,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CentralSyncState {
    NotRequired,
    Pending,
    Synced,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct HostRemoteAccessStatus {
    pub mode: HostRemoteAccessMode,
    pub state_version: u64,
    pub locked_at: Option<String>,
    /// False means the running process is fail-closed but the latest transition
    /// could not be committed and may not survive a restart.
    pub durable: bool,
    pub central_sync: CentralSyncState,
}

impl Default for HostRemoteAccessStatus {
    fn default() -> Self {
        Self {
            mode: HostRemoteAccessMode::Unlocked,
            state_version: 0,
            locked_at: None,
            durable: true,
            central_sync: CentralSyncState::NotRequired,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct HostAccessSnapshot {
    pub epoch: String,
    pub revision: u64,
    pub indicator_enabled: bool,
    pub total_session_count: u32,
    pub sessions: Vec<HostAccessSession>,
    pub remote_access: HostRemoteAccessStatus,
}

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

    /// Per-WebSocket-session credential for the native locale REST bridge.
    /// This message is sent only through that session's direct channel.
    NativeBridgeReady {
        token: String,
        locale: String,
        locale_persisted: bool,
    },

    /// Daemon-issued identity for one validated Tauri WebSocket registration.
    SessionShellRegistered {
        registration_id: String,
        registration_generation: u64,
    },

    /// Stable rejection category. Details stay in redacted daemon diagnostics.
    SessionShellRegistrationRejected { code: SessionShellRegistrationError },

    /// Authoritative locale changed. Broadcast to every connected Tauri shell
    /// so multiple windows converge.
    GlobalLocaleChanged { locale: String },

    /// Show private-screen overlay.
    PrivateScreenShow {
        connection_id: String,
        request_id: String,
    },

    /// Hide private-screen overlay.
    PrivateScreenHide {
        connection_id: String,
        request_id: String,
    },

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
    /// `install_idd_driver` is only honoured when `op == Install`; the
    /// uninstall path always removes the driver as well.
    ServiceOp {
        op: ServiceOpKind,
        install_path: Option<String>,
        #[serde(default)]
        install_idd_driver: bool,
    },

    /// Complete daemon-authoritative remote-access state for the local shell.
    HostAccessSnapshot { snapshot: HostAccessSnapshot },

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
        request_id: Option<String>,
        visible: bool,
        is_supported: bool,
        error_msg: Option<String>,
    },

    // ========================= Client → Server =========================
    /// Handshake message announcing role + admin status.
    Ready {
        #[serde(default)]
        role: ClientRole,
        #[serde(default)]
        is_admin: Option<bool>,
    },

    /// Linux service shell reports the context from which a user worker should
    /// be launched. The daemon, not this payload, assigns registration identity.
    SessionShellInfo { info: SessionShellInfo },

    /// Tauri reports a private-screen visibility change.
    PrivateScreenStateChanged {
        connection_id: String,
        request_id: Option<String>,
        visible: bool,
        is_supported: bool,
        error_msg: Option<String>,
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
            HostControlMessage::NativeBridgeReady {
                token: "bridge-123".to_string(),
                locale: "en-US".to_string(),
                locale_persisted: true,
            },
            HostControlMessage::SessionShellRegistered {
                registration_id: "registration-1".to_string(),
                registration_generation: 9,
            },
            HostControlMessage::SessionShellRegistrationRejected {
                code: SessionShellRegistrationError::SessionConflict,
            },
            HostControlMessage::GlobalLocaleChanged {
                locale: "zh-CN".to_string(),
            },
            HostControlMessage::PrivateScreenShow {
                connection_id: "c1".to_string(),
                request_id: "r-show".to_string(),
            },
            HostControlMessage::PrivateScreenHide {
                connection_id: "c1".to_string(),
                request_id: "r-hide".to_string(),
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
                request_id: Some("r-state".to_string()),
                visible: true,
                is_supported: true,
                error_msg: None,
            },
            HostControlMessage::ServiceOp {
                op: ServiceOpKind::Install,
                install_path: Some("C:\\Program Files\\app".to_string()),
                install_idd_driver: true,
            },
            HostControlMessage::HostAccessSnapshot {
                snapshot: HostAccessSnapshot {
                    epoch: "epoch-1".to_string(),
                    revision: 7,
                    indicator_enabled: true,
                    total_session_count: 1,
                    sessions: vec![HostAccessSession {
                        connection_id: "c1".to_string(),
                        actor: ActorSummary::unknown(),
                        started_at: "2026-07-21T00:00:00Z".to_string(),
                        desktop_view: true,
                        remote_control: false,
                        terminal_count: 0,
                        file_manager: false,
                        system_audio_capture: false,
                        transfers: Vec::new(),
                    }],
                    remote_access: HostRemoteAccessStatus::default(),
                },
            },
            HostControlMessage::ServiceOp {
                op: ServiceOpKind::Install,
                install_path: Some("C:\\Program Files\\app".to_string()),
                install_idd_driver: false,
            },
            HostControlMessage::ServiceOp {
                op: ServiceOpKind::Uninstall,
                install_path: None,
                install_idd_driver: false,
            },
            HostControlMessage::Ready {
                role: ClientRole::Tauri,
                is_admin: Some(true),
            },
            HostControlMessage::Ready {
                role: ClientRole::Forwarder,
                is_admin: None,
            },
            HostControlMessage::SessionShellInfo {
                info: SessionShellInfo {
                    app_version: "1.0.0".to_string(),
                    protocol_version: SESSION_SHELL_PROTOCOL_VERSION,
                    pid: 42,
                    process_start_ticks: 123,
                    reported_uid: 1000,
                    session_id: Some("2".to_string()),
                    seat: Some("seat0".to_string()),
                    session_type: Some("wayland".to_string()),
                    cwd_base64: "L2hvbWUvdXNlcg==".to_string(),
                    umask: 0o022,
                    environment: vec![EnvironmentEntryBase64 {
                        key_base64: "UEFUSA==".to_string(),
                        value_base64: "L3Vzci9iaW4=".to_string(),
                    }],
                },
            },
            HostControlMessage::PrivateScreenStateChanged {
                connection_id: "c1".to_string(),
                request_id: None,
                visible: false,
                is_supported: true,
                error_msg: None,
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

    // ServiceOp without `install_idd_driver` (older client/wire) defaults
    // the field to false, never `true` — the `#[serde(default)]` guard
    // is the only thing preventing a missing field from being interpreted
    // as "user opted into IDD installation".
    #[test]
    fn service_op_install_missing_idd_field_defaults_false() {
        let json = r#"{"type":"ServiceOp","op":"install","install_path":"C:/foo"}"#;
        let msg: HostControlMessage = serde_json::from_str(json).expect("deserialise");
        match msg {
            HostControlMessage::ServiceOp {
                op,
                install_path,
                install_idd_driver,
            } => {
                assert!(matches!(op, ServiceOpKind::Install));
                assert_eq!(install_path.as_deref(), Some("C:/foo"));
                assert!(!install_idd_driver);
            }
            other => panic!("unexpected: {other:?}"),
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
    fn session_shell_debug_redacts_environment_and_cwd() {
        let info = SessionShellInfo {
            app_version: "1.0.0".to_string(),
            protocol_version: SESSION_SHELL_PROTOCOL_VERSION,
            pid: 42,
            process_start_ticks: 123,
            reported_uid: 1000,
            session_id: Some("2".to_string()),
            seat: Some("seat0".to_string()),
            session_type: Some("wayland".to_string()),
            cwd_base64: "c2VjcmV0L2N3ZA==".to_string(),
            umask: 0o077,
            environment: vec![EnvironmentEntryBase64 {
                key_base64: "U0VDUkVU".to_string(),
                value_base64: "ZG8tbm90LWxvZw==".to_string(),
            }],
        };

        let rendered = format!("{info:?}");
        assert!(!rendered.contains("c2VjcmV0L2N3ZA=="));
        assert!(!rendered.contains("ZG8tbm90LWxvZw=="));
        assert!(rendered.contains("environment_entries: 1"));
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
            request_id: "r-show".to_string(),
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
            request_id: "r-show".to_string(),
        };
        assert_eq!(msg.req_id(), None);
    }
}
