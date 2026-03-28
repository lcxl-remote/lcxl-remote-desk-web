use desk_signal::controller::device_code::{
    DeviceCodeBatchDeleteParams, DeviceCodeItem, DeviceCodeListResult,
};
use desk_signal_facade::model::{
    connection::ConnectionList,
    desk_settings::DeskSettings,
    signal::{InitSignalingData, RequestRemoteModel, SignalingModel},
    terminal::{TerminalInputData, TerminalOutputData, TerminalResizeData},
};
use desk_turn::model::{TurnInterface, TurnSettings};
use utoipa::OpenApi;

use crate::model::{
    data_channel::{KeyboardEventData, MouseEventData, SignalRequestControlData},
    file_transfer::FileTransferMessage,
    info::BackendInfo,
    security_approval::SecurityApprovalEventPayload,
    settings::{TurnClientSettings, TraversalMode},
};

/// API version
#[derive(OpenApi)]
#[openapi(components(schemas(
    SignalingModel,
    RequestRemoteModel,
    InitSignalingData,
    DeskSettings,
    KeyboardEventData,
    MouseEventData,
    SignalRequestControlData,
    ConnectionList,
    TerminalInputData,
    TerminalOutputData,
    TerminalResizeData,
    FileTransferMessage,
    DeviceCodeItem,
    DeviceCodeListResult,
    DeviceCodeBatchDeleteParams,
    BackendInfo,
    SecurityApprovalEventPayload,
    TurnSettings,
    TurnInterface,
    TurnClientSettings,
    TraversalMode,
)))]
pub struct ExtraSchemas;
