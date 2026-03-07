use crate::error::DeskError;
use auto_launch::AutoLaunchBuilder;
use desk_utils::error::DeskErrorCode;
use log::info;
use std::env;

pub fn update_auto_start_status(enable: bool) -> Result<(), DeskError> {
    let current_exe = env::current_exe().map_err(|e| {
        DeskError::new_custom_error(
            DeskErrorCode::AUTO_START_ERROR,
            &format!("Failed to get current executable path: {}", e),
        )
    })?;

    let app_name = "lcxl-remote-desk";
    let app_path = current_exe.to_string_lossy().to_string();
    let hidden_arg = "--hidden";

    let auto = AutoLaunchBuilder::new()
        .set_app_name(app_name)
        .set_app_path(&app_path)
        .set_args(&[hidden_arg])
        .build()
        .map_err(|e| {
            DeskError::new_custom_error(
                DeskErrorCode::AUTO_START_ERROR,
                &format!("Failed to build AutoLaunch: {}", e),
            )
        })?;

    info!("Updating auto-start status, enable: {}", enable);

    if enable {
        auto.enable().map_err(|e| {
            DeskError::new_custom_error(
                DeskErrorCode::AUTO_START_ERROR,
                &format!("Failed to enable auto-start: {:?}", e),
            )
        })?;
        info!("Successfully enabled auto-start for {}", app_name);
    } else {
        auto.disable().map_err(|e| {
            DeskError::new_custom_error(
                DeskErrorCode::AUTO_START_ERROR,
                &format!("Failed to disable auto-start: {:?}", e),
            )
        })?;
        info!("Successfully disabled auto-start for {}", app_name);
    }

    Ok(())
}
