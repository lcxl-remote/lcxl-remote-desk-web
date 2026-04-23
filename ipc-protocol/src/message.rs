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

    /// Worker reports an error
    Error(ErrorPayload),
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
    /// Authentication token for signaling
    pub auth_token: Option<String>,
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
pub struct DesktopSwitchPayload {
    /// Previous desktop name
    pub from_desktop: Option<String>,
    /// New desktop name
    pub to_desktop: Option<String>,
    /// Phase of the switch
    pub phase: DesktopSwitchPhase,
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
