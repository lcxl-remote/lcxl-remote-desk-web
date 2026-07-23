use crate::worker::agent::LocalDeviceAgent;
use crate::{
    host_control::{HostControlHub, UpstreamForwarder, upstream::spawn_upstream_ws_task},
    model::settings::{Args, Settings, SharedSettings, StartupMode},
    service::signaling::{DeskSession, DeskSessionMessage, DeskSessionSender},
    worker::{
        clipboard_dispatcher::ClipboardDispatcher,
        desktop_monitor,
        file_transfer_dispatcher::FileTransferDispatcher,
        input_dispatcher::InputDispatcher,
        media_producer::MediaProducer,
        shared_capture::CaptureKey,
        virtual_display::{
            RestartStep, VirtualDisplayState, resolve_attach_with_backoff, run_set_mode,
        },
        whiteboard_dispatcher::WhiteboardDispatcher,
    },
};
use actix_web::web;
use desk_agent_protocol::{AgentOutcome, DeviceAgent};
use desk_input_injection::display_watcher;
use desk_ipc_protocol::{
    dual_transport::{EventReceiver, EventSender, MediaSender, framed},
    message::{
        AgentResponsePayload, DesktopChangedPayload, ExecCancelPayload, ExecHeartbeatPayload,
        ExecResultIpcPayload, ExecSpawnReportPayload, FileTransferPayload, HeartbeatPayload,
        ListTerminalResponsePayload, LocaleAppliedPayload, ManagerFileListResponsePayload,
        ManagerQuerySettingsResponsePayload, ManagerResponseRefPayload,
        ManagerSystemInfoResponsePayload, PrivateScreenStateChangedPayload,
        RemoteAccessStateAppliedPayload, ReplyFromTerminalPayload, ServiceToWorker,
        SignalingErrorPayload, StopMediaPayload, TerminalClosedPayload, TerminalStartedPayload,
        VirtualDisplayAttachOutcome, VirtualDisplayAttachResultPayload, WorkerInitPayload,
        WorkerToService,
    },
    transport::{read_message, write_message},
};
use desk_server_user::model::CurrentUser;
use desk_signal_facade::model::files::FileListResponse;
use desk_signal_facade::model::private_screen::{
    EnablePrivateScreenData, PrivateScreenStateChangedData,
};
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};
use desk_signal_facade::model::system_info::SystemInfo;
use desk_signal_facade::model::system_settings::RemoteSystemSettings;
use desk_signal_facade::model::terminal::{TerminalList, TerminalOutputData};
use desk_virtual_display::VirtualDisplayController;

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
    time::{SystemTime, UNIX_EPOCH},
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
