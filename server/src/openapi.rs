use desk_signal::controller::device_code::{
    DeviceCodeBatchDeleteParams, DeviceCodeItem, DeviceCodeListResult,
};
use desk_signal_facade::model::{
    connection::ConnectionsFetchedData,
    desk_settings::DeskSettings,
    media_pipeline::MediaPipelineStateData,
    signal::{RemoteAccessInitializedData, RequestRemoteModel, SignalingModel},
    terminal::{TerminalInputData, TerminalOutputData, TerminalResizeData},
};
use desk_turn::model::{TurnInterface, TurnSettings};
use desk_utils::error::DeskErrorCode;
use utoipa::OpenApi;

use crate::controller::virtual_display::VirtualDisplayDriverStatusResponse;
use crate::model::{
    data_channel::SignalRequestControlData,
    file_transfer::FileTransferMessage,
    info::{BackendDiagnosticItem, BackendDiagnosticSection, BackendDiagnosticStatus, BackendInfo},
    security_approval::SecurityApprovalEventPayload,
    settings::{TraversalMode, TurnClientSettings, VirtualDisplaySettings},
};
use desk_input_injection::model::data_channel::{KeyboardEventData, MouseEventData};

/// API version
#[derive(OpenApi)]
#[openapi(components(schemas(
    SignalingModel,
    RequestRemoteModel,
    RemoteAccessInitializedData,
    DeskSettings,
    KeyboardEventData,
    MouseEventData,
    SignalRequestControlData,
    ConnectionsFetchedData,
    TerminalInputData,
    TerminalOutputData,
    TerminalResizeData,
    FileTransferMessage,
    DeviceCodeItem,
    DeviceCodeListResult,
    DeviceCodeBatchDeleteParams,
    BackendInfo,
    BackendDiagnosticSection,
    BackendDiagnosticItem,
    BackendDiagnosticStatus,
    SecurityApprovalEventPayload,
    TurnSettings,
    TurnInterface,
    TurnClientSettings,
    TraversalMode,
    VirtualDisplaySettings,
    VirtualDisplayDriverStatusResponse,
    // Not referenced by any body: `RestResponse.code` is a bare `i32` on the
    // wire. Publishing the enum anyway is what lets the generated client expose
    // named constants instead of the numbers being mirrored by hand.
    DeskErrorCode,
    MediaPipelineStateData,
)))]
pub struct ExtraSchemas;
