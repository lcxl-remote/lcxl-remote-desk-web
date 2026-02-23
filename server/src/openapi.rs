use desk_signal_facade::model::{
    desk_settings::DeskSettings,
    session::SessionList,
    signal::{InitSignalingData, SignalingModel},
    terminal::{TerminalInputData, TerminalOutputData, TerminalResizeData},
};
use utoipa::OpenApi;

use crate::model::{
    data_channel::{KeyboardEventData, MouseEventData, SignalRequestControlData},
    file_transfer::FileTransferMessage,
};

/// API version
#[derive(OpenApi)]
#[openapi(components(schemas(
    SignalingModel,
    InitSignalingData,
    DeskSettings,
    KeyboardEventData,
    MouseEventData,
    SignalRequestControlData,
    SessionList,
    TerminalInputData,
    TerminalOutputData,
    TerminalResizeData,
    FileTransferMessage,
)))]
pub struct ExtraSchemas;
