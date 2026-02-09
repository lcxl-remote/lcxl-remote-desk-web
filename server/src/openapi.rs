use desk_signal_facade::model::session::SessionList;
use utoipa::OpenApi;

use crate::model::data_channel::{KeyboardEventData, MouseEventData, SignalRequestControlData};

/// API version
#[derive(OpenApi)]
#[openapi(components(schemas(KeyboardEventData, MouseEventData, SignalRequestControlData, SessionList)))]
pub struct ExtraSchemas;
