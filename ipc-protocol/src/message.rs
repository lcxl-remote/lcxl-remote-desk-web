use serde::{Deserialize, Serialize};

/// Messages sent from Service Core to Worker process
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum ServiceToWorker {
    /// Initialize the worker with session and configuration info
    Init(WorkerInitPayload),

    /// Forward a signaling message (SDP offer/answer, ICE candidate) to the worker
    SignalingMessage(SignalingPayload),

    /// Notify the worker that a desktop switch is happening
    /// The worker should prepare to shut down
    DesktopSwitching,

    /// Force the worker to shut down immediately
    Shutdown,
}

/// Messages sent from Worker process to Service Core
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum WorkerToService {
    /// Worker has started and is ready to accept connections
    Ready,

    /// Worker is forwarding a signaling message back to the Service
    SignalingMessage(SignalingPayload),

    /// Worker reports its health status
    Heartbeat(HeartbeatPayload),

    /// Worker reports a desktop switch is complete and it's ready to resume
    DesktopReady,

    /// Worker detected the user-input desktop changed (e.g. user invoked UAC
    /// → secure desktop "Winlogon", switched to lock screen "ScreenSaver",
    /// etc.). The daemon's session-monitor running in session 0 cannot see
    /// across-window-station desktop changes — only a process living in
    /// the user's WinSta0 (i.e. the worker) can. The daemon reacts by
    /// shutting down this worker and launching a fresh one bound to the
    /// new desktop.
    DesktopChanged(DesktopChangedPayload),

    /// Worker reports an error
    Error(ErrorPayload),

    /// Worker reports the authoritative `ConnectionAcceptState` for one of
    /// its peer connections has changed. The daemon caches this map keyed
    /// by `connection_id` and ships it as `preapproved_connections` into
    /// the next worker's `WorkerInitPayload`, so that after worker restart
    /// (UAC, lock screen, OS-session change, crash recovery) the new worker
    /// can pre-populate `SignalingState` without re-prompting the user via
    /// Tauri (whose dialog is invisible on the secure desktop during UAC).
    ///
    /// The worker is the source of truth — the daemon must not infer this
    /// state by parsing signaling traffic.
    ConnectionAcceptStateChanged {
        connection_id: String,
        state: ConnectionAcceptState,
    },

    /// Worker reports a peer connection is gone (browser closed the tab,
    /// WebRTC ICE failed, etc.). The daemon drops the entry from its
    /// per-connection cache so a long-running daemon does not accumulate
    /// stale state across many connect/disconnect cycles.
    ConnectionClosed { connection_id: String },
}

/// Messages sent from Service Core to Tauri UI
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum ServiceToUI {
    /// Service status update
    StatusUpdate(ServiceStatus),

    /// Connection state changed
    ConnectionState(ConnectionStatePayload),

    /// Desktop switch event
    DesktopSwitchEvent(DesktopSwitchPayload),
}

/// Messages sent from Tauri UI to Service Core
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum UIToService {
    /// Request service status
    GetStatus,

    /// Start/stop service
    SetServiceState { enabled: bool },

    /// Update configuration
    UpdateConfig(String), // JSON config string
}

// ==================== Payload Types ====================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerInitPayload {
    /// Session ID for this worker instance
    pub session_id: String,
    /// OS session ID
    pub os_session_id: u32,
    /// Desktop name being served
    pub desktop_name: Option<String>,
    /// Configuration JSON (DeskSettings serialized)
    pub config_json: String,
    /// Signaling server URL to connect to (or proxy through service)
    pub signaling_url: Option<String>,
    /// Authentication token. In daemon-spawned workers this is the
    /// `tauri_ipc_token` used to authenticate the worker's host-control
    /// upstream ws connection back to the daemon.
    pub auth_token: Option<String>,
    /// URL of the daemon's `/ws/host_upstream` endpoint. When `Some`, the
    /// worker constructs a Forwarder-mode `HostControlHub` that bridges
    /// approval / private-screen / whiteboard traffic through the daemon to
    /// the connected Tauri shell. When `None`, the worker falls back to a
    /// Local hub (used by tests / standalone runs).
    #[serde(default)]
    pub host_upstream_url: Option<String>,

    /// Per-connection accept state the daemon cached before this worker was
    /// (re)spawned. The worker uses this list to pre-populate
    /// `SignalingState` at PC-creation time so the user is not re-prompted
    /// across desktop / session switches. Empty on the first worker launch
    /// and on standalone (non-daemon) runs.
    #[serde(default)]
    pub preapproved_connections: Vec<(String, ConnectionAcceptState)>,
}

/// Per-peer-connection acceptance state. The daemon caches this map keyed by
/// `connection_id` and ships it across worker restarts (see
/// `WorkerToService::ConnectionAcceptStateChanged` and
/// `WorkerInitPayload::preapproved_connections`).
///
/// Each `bool` corresponds 1:1 to a field on the worker's
/// `desk_signal_facade::SignalingState`. Both default to `false`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectionAcceptState {
    /// Remote peer was granted mouse / keyboard input.
    pub accept_control: bool,
    /// Remote peer was granted bidirectional clipboard sync. Independent of
    /// `accept_control` — clipboard can be denied even when control was
    /// granted, so the daemon must never infer this from control alone.
    pub accept_clipboard_sync: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalingPayload {
    /// The raw signaling message (SDP, ICE, etc.) as JSON
    pub message: String,
    /// Connection ID this message is associated with
    pub connection_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatPayload {
    /// Current timestamp
    pub timestamp_ms: u64,
    /// Number of active WebRTC connections
    pub active_connections: u32,
    /// CPU usage percentage
    pub cpu_usage: Option<f32>,
    /// Memory usage in bytes
    pub memory_usage: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorPayload {
    /// Error code
    pub code: i32,
    /// Error message
    pub message: String,
    /// Whether the worker can continue operating
    pub recoverable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStatus {
    /// Whether the service is running as a Windows service
    pub is_service_mode: bool,
    /// Whether a worker is currently active
    pub worker_active: bool,
    /// Current OS session ID
    pub current_session_id: Option<u32>,
    /// Current desktop name
    pub current_desktop: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionStatePayload {
    /// Connection ID
    pub connection_id: String,
    /// Connection state
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopChangedPayload {
    /// New input desktop name as returned by `OpenInputDesktop` +
    /// `GetUserObjectInformationW(UOI_NAME)`. Examples: "Default", "Winlogon",
    /// "Screen-saver". The daemon launches the next worker with this name as
    /// the `lpDesktop` argument to `CreateProcessAsUserW`.
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopSwitchPayload {
    /// Previous desktop name
    pub from_desktop: Option<String>,
    /// New desktop name
    pub to_desktop: Option<String>,
    /// Phase of the switch
    pub phase: DesktopSwitchPhase,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// New `host_upstream_url` + repurposed `auth_token` fields round-trip cleanly.
    #[test]
    fn worker_init_payload_round_trip_with_host_upstream_fields() {
        let original = WorkerInitPayload {
            session_id: "session-1".to_string(),
            os_session_id: 7,
            desktop_name: Some("Default".to_string()),
            config_json: "{}".to_string(),
            signaling_url: None,
            auth_token: Some("ipc-token".to_string()),
            host_upstream_url: Some("ws://127.0.0.1:8082/ws/host_upstream".to_string()),
            preapproved_connections: Vec::new(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: WorkerInitPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.session_id, original.session_id);
        assert_eq!(decoded.os_session_id, original.os_session_id);
        assert_eq!(decoded.auth_token, original.auth_token);
        assert_eq!(decoded.host_upstream_url, original.host_upstream_url);
    }

    /// `DesktopChanged` round-trips with the same JSON shape the IPC reader
    /// expects (tag = "type", content = "payload").
    #[test]
    fn desktop_changed_round_trips() {
        let msg = WorkerToService::DesktopChanged(DesktopChangedPayload {
            name: "Winlogon".to_string(),
        });
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: WorkerToService = serde_json::from_str(&json).unwrap();
        match decoded {
            WorkerToService::DesktopChanged(payload) => assert_eq!(payload.name, "Winlogon"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// Older daemons that don't yet emit `host_upstream_url` must still be
    /// accepted by newer workers (the field carries `#[serde(default)]`).
    #[test]
    fn worker_init_payload_accepts_missing_host_upstream_url() {
        let legacy = serde_json::json!({
            "session_id": "session-1",
            "os_session_id": 7,
            "desktop_name": null,
            "config_json": "{}",
            "signaling_url": null,
            "auth_token": null,
        });
        let decoded: WorkerInitPayload = serde_json::from_value(legacy).unwrap();
        assert!(decoded.host_upstream_url.is_none());
        assert!(decoded.auth_token.is_none());
        assert!(decoded.preapproved_connections.is_empty());
    }

    /// `preapproved_connections` round-trips and carries the per-connection
    /// `ConnectionAcceptState` faithfully.
    #[test]
    fn worker_init_payload_preapproved_round_trip() {
        let original = WorkerInitPayload {
            session_id: "session-1".to_string(),
            os_session_id: 1,
            desktop_name: None,
            config_json: "{}".to_string(),
            signaling_url: None,
            auth_token: None,
            host_upstream_url: None,
            preapproved_connections: vec![
                (
                    "conn-a".to_string(),
                    ConnectionAcceptState {
                        accept_control: true,
                        accept_clipboard_sync: false,
                    },
                ),
                (
                    "conn-b".to_string(),
                    ConnectionAcceptState {
                        accept_control: true,
                        accept_clipboard_sync: true,
                    },
                ),
            ],
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: WorkerInitPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.preapproved_connections, original.preapproved_connections);
    }

    /// `ConnectionAcceptState` defaults to all-false (the safe initial state
    /// for any new `connection_id` the daemon has not yet seen approved).
    #[test]
    fn connection_accept_state_default_is_all_false() {
        let s = ConnectionAcceptState::default();
        assert!(!s.accept_control);
        assert!(!s.accept_clipboard_sync);
    }

    /// `WorkerToService::ConnectionAcceptStateChanged` round-trips with the
    /// shared tag/content shape (`type` / `payload`) used by the IPC reader.
    #[test]
    fn connection_accept_state_changed_round_trips() {
        let msg = WorkerToService::ConnectionAcceptStateChanged {
            connection_id: "conn-42".to_string(),
            state: ConnectionAcceptState {
                accept_control: true,
                accept_clipboard_sync: true,
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: WorkerToService = serde_json::from_str(&json).unwrap();
        match decoded {
            WorkerToService::ConnectionAcceptStateChanged {
                connection_id,
                state,
            } => {
                assert_eq!(connection_id, "conn-42");
                assert!(state.accept_control);
                assert!(state.accept_clipboard_sync);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `WorkerToService::ConnectionClosed` round-trips and carries only the
    /// `connection_id`.
    #[test]
    fn connection_closed_round_trips() {
        let msg = WorkerToService::ConnectionClosed {
            connection_id: "conn-x".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: WorkerToService = serde_json::from_str(&json).unwrap();
        match decoded {
            WorkerToService::ConnectionClosed { connection_id } => {
                assert_eq!(connection_id, "conn-x");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DesktopSwitchPhase {
    /// Switch is starting, worker may disconnect
    Starting,
    /// New worker is initializing
    WorkerInitializing,
    /// Switch complete, connections are being re-established
    Reconnecting,
    /// Switch complete, all connections restored
    Complete,
    /// Switch failed
    Failed(String),
}
