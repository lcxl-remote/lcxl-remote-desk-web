use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::error::DeskErrorCode;

#[derive(Serialize, Deserialize, ToSchema)]
pub struct RestResponse<T: ToSchema> {
    pub success: bool,
    pub code: i32,
    pub message: Option<String>,
    pub data: Option<T>,
}
impl RestResponse<()> {
    pub fn succeed() -> Self {
        RestResponse {
            success: true,
            code: DeskErrorCode::SUCCESS.code(),
            message: None,
            data: None,
        }
    }

    pub fn succeed_with_message(message: String) -> Self {
        RestResponse {
            success: true,
            code: DeskErrorCode::SUCCESS.code(),
            message: Some(message),
            data: None,
        }
    }

    pub fn failed(error_code: DeskErrorCode, message: String) -> Self {
        RestResponse {
            success: false,
            code: error_code.code(),
            message: Some(message),
            data: None,
        }
    }
}

impl<T: ToSchema> RestResponse<T> {
    pub fn succeed_with_data(data: T) -> Self {
        RestResponse {
            success: true,
            code: DeskErrorCode::SUCCESS.code(),
            message: None,
            data: Some(data),
        }
    }

    pub fn failed_with_data(
        error_code: DeskErrorCode,
        message: Option<String>,
        data: Option<T>,
    ) -> Self {
        RestResponse {
            success: false,
            code: error_code.code(),
            message,
            data,
        }
    }
}
