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

/// Static enumeration of every `#[utoipa::path]` the desk-server binary can
/// expose at runtime — including conditional routes that only register in
/// specific `StartupMode`s (e.g. device-code CRUD, signaling, TURN). Used by
/// the `openapi_tags` integration test to assert every operation carries a
/// non-empty `tags` field; the production swagger is still assembled inline
/// in `run_with_hub` via `utoipa_actix_web::scope`, but adding a new handler
/// without also listing it here will cause the test to under-report — keep
/// the two registrations in sync, matching the convention already in place
/// for [`configure_api_routes`] vs. `run_with_hub`.
#[derive(OpenApi)]
#[openapi(paths(
    // web/server/src/controller/
    crate::controller::init::init_system,
    crate::controller::info::query_sysinfo,
    crate::controller::info::query_server_info,
    crate::controller::info::query_backend_info,
    crate::controller::user::get_current_user,
    crate::controller::login::login_account,
    crate::controller::login::get_captcha,
    crate::controller::login::logout_account,
    crate::controller::login::change_password,
    crate::controller::login::login_tauri,
    crate::controller::service_mgmt::install_service,
    crate::controller::service_mgmt::uninstall_service,
    crate::controller::turn::get_turn_info,
    crate::controller::turn::get_turn_session,
    crate::controller::turn::get_turn_session_statistics,
    crate::controller::turn::delete_turn_session,
    crate::controller::turn::get_turn_metrics,
    crate::controller::virtual_display::query_driver_status,
    crate::controller::virtual_display::install_driver,
    crate::controller::virtual_display::uninstall_driver,
    crate::controller::virtual_display::query_virtual_display_settings,
    crate::controller::virtual_display::update_virtual_display_settings,
    crate::controller::settings::query_settings,
    crate::controller::settings::update_settings,
    crate::controller::settings::query_turn_settings,
    crate::controller::settings::update_turn_settings,
    crate::controller::settings::query_log_settings,
    crate::controller::settings::update_log_settings,
    crate::controller::settings::query_telemetry_status,
    crate::controller::settings::update_telemetry_consent,
    crate::controller::settings::regenerate_turn_secret,
    crate::controller::settings::query_security_settings,
    crate::controller::settings::update_security_settings,
    crate::controller::settings::submit_security_approval,
    crate::controller::settings::ack_security_approval,
    crate::controller::settings::query_turn_client_settings,
    crate::controller::settings::update_turn_client_settings,
    // web/signal-facade/src/controller/ (paths shared with manager via desk_proxy)
    desk_signal_facade::controller::connection::list_connections,
    desk_signal_facade::controller::files::list_files,
    desk_signal_facade::controller::files::delete_file,
    desk_signal_facade::controller::sysinfo::query_sysinfo,
    desk_signal_facade::controller::terminal::list_terminal,
    // web/signal/src/controller/ (registered by desk-server when applicable)
    desk_signal::controller::signaling::open_signaling_handle,
    desk_signal::controller::terminal::open_terminal_session,
    desk_signal::controller::device_code::list_device_codes,
    desk_signal::controller::device_code::create_device_code,
    desk_signal::controller::device_code::update_device_code,
    desk_signal::controller::device_code::delete_device_code,
    desk_signal::controller::device_code::batch_delete_device_codes,
))]
pub struct AllPathsDoc;
