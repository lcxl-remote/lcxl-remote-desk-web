use crate::worker::agent::LocalDeviceAgent;
use crate::{
    host_control::{HostControlHub, UpstreamForwarder, upstream::spawn_upstream_ws_task},
    model::policy_access::PolicyAccess,
    model::settings::{Args, Settings, SharedSettings, StartupMode},
    service::signaling::{DeskSession, DeskSessionMessage, DeskSessionSender},
    worker::{
        clipboard_dispatcher::ClipboardDispatcher,
        desktop_monitor,
        file_transfer_dispatcher::FileTransferDispatcher,
        input_dispatcher::InputDispatcher,
        media_producer::{MediaProducer, StartMediaResult},
        policy_mirror::PolicyMirror,
        shared_capture::CaptureKey,
        virtual_display::{
            RestartStep, VirtualDisplayState, resolve_attach_with_backoff, run_set_mode,
        },
        whiteboard_dispatcher::WhiteboardDispatcher,
    },
};
use actix_web::web;
use desk_agent_protocol::computer_use::{
    ComputerActionCompleted, ComputerActionKind, ComputerActionPhase, ComputerActionResultClass,
    ComputerActionStartDisposition, ComputerActionStarted, ComputerActionStateReport,
    ComputerActionStepFact, FilePatchAction,
};
use desk_agent_protocol::{AgentOutcome, DeviceAgent};
use desk_input_injection::display_watcher;
use desk_ipc_protocol::{
    dual_transport::{EventReceiver, EventSender, MediaSender, framed},
    message::{
        AgentResponsePayload, ComputerActionCompletedPayload, ComputerActionStartedPayload,
        ComputerActionStateReportedPayload, ComputerUseReadinessPayload, DesktopChangedPayload,
        ExecCancelPayload, ExecHeartbeatPayload, ExecResultIpcPayload, ExecSpawnReportPayload,
        FileTransferPayload, FilesListedPayload, HeartbeatPayload, LocaleAppliedPayload,
        ManagerResponseRefPayload, PrivateScreenStateChangedPayload,
        RemoteAccessStateAppliedPayload, SecurityPolicyAppliedPayload, ServiceToWorker,
        SignalingErrorPayload, StopMediaPayload, SystemInfoRetrievedPayload, TerminalClosedPayload,
        TerminalCommandsListedPayload, TerminalOutputProducedPayload, TerminalStartedPayload,
        VirtualDisplayAttachOutcome, VirtualDisplayAttachResultPayload, WorkerInitPayload,
        WorkerToService,
    },
    transport::{read_message, write_message},
};
use desk_server_user::model::CurrentUser;
use desk_signal_facade::model::files::FileListResponse;
use desk_signal_facade::model::policy_snapshot::PolicySnapshot;
use desk_signal_facade::model::private_screen::{
    PrivateScreenStateChangedData, SetPrivateScreenVisibilityData,
};
use desk_signal_facade::model::signal::PeerSignalingSender;
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};
use desk_signal_facade::model::system_info::SystemInfo;
use desk_signal_facade::model::terminal::{TerminalList, TerminalOutputData};
#[cfg(target_os = "linux")]
use desk_utils::linux_display::{LinuxDisplayServer, detect_linux_display_environment};
use desk_virtual_display::VirtualDisplayController;

/// Whether a daemon command still applies while remote access is locked.
///
/// A locked host refuses remote work, but not the instructions that describe
/// the host itself. The locale is already persisted by the time it arrives, so
/// dropping it would leave the worker rendering in a language the host has
/// stopped using — permanently, because nothing re-sends it on unlock.
///
/// The security policy is absent from this decision on purpose: it is applied
/// on the transport reader task, ahead of the loop this guards, so an operator
/// revoking a capability during a lock is never subject to it.
pub(crate) fn survives_remote_access_lock(msg: &ServiceToWorker) -> bool {
    matches!(
        msg,
        ServiceToWorker::Shutdown
            | ServiceToWorker::Init(_)
            | ServiceToWorker::SetRemoteAccessState(_)
            | ServiceToWorker::SetLocale(_)
    )
}

/// How often a running command reports that it is still running.
///
/// Long enough that a fleet of busy hosts does not flood the link, short enough
/// that an operator watching a long command sees it move. Losing a beat costs
/// nothing: the authoritative answer is a state query against the ledger.
const EXEC_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
use log::{error, info, warn};
use std::collections::HashSet;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
};

mod connection;
mod outbound;
mod runtime;
mod tasks;

use outbound::*;
use tasks::*;

#[derive(Debug)]
struct CaptureGeometryReady {
    connection_id: String,
    generation: u64,
    rect: (i32, i32, i32, i32),
}

/// Worker-side session. Stateless wrapper — all mutable state lives in the
/// dispatchers / `DeskSession` instances built per-session inside
/// [`Self::run_with_transports`]. The struct exists so the named-pipe
/// entry point ([`Self::run`]) and the in-process portable entry
/// ([`Self::run_with_transports`]) share an inherent-method namespace.
pub struct WorkerSession;

impl Default for WorkerSession {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkerSession {
    pub fn new() -> Self {
        WorkerSession
    }

    pub async fn run(_args: Args, pipe_name: &str) -> Result<(), Box<dyn std::error::Error>> {
        let session = WorkerSession;
        session.connect_and_serve(pipe_name).await
    }
}

#[cfg(test)]
mod tests;
