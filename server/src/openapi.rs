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

use crate::controller::virtual_display::VirtualDisplayDriverStatusResponse;
use crate::model::{
    data_channel::SignalRequestControlData,
    file_transfer::FileTransferMessage,
    info::BackendInfo,
    security_approval::SecurityApprovalEventPayload,
    settings::{TraversalMode, TurnClientSettings, VirtualDisplaySettings},
};
use desk_input_injection::model::data_channel::{KeyboardEventData, MouseEventData};

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
    VirtualDisplaySettings,
    VirtualDisplayDriverStatusResponse,
)))]
pub struct ExtraSchemas;
