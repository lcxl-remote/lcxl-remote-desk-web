// DeskError is the crate's standard (large) error type; returning it in Result
// is consistent with the rest of the server crate.
#![allow(clippy::result_large_err)]

use crate::error::DeskError;
use std::path::Path;

/// Enable or disable launch-on-login auto-start.
///
/// On macOS this delegates to [`crate::macos_agent`], which owns the single
/// LaunchAgent plist. Auto-start re-launches the Tauri `.app` (the one resident
/// process, which embeds the server) with `--hidden` and an absolute
/// `--config-file-path`. Enable/disable use a symmetric "next-login boundary"
/// semantic: writing/removing the plist (plus the disable override) never kills
/// or kickstarts the currently running instance.
///
/// On Windows/Linux this keeps the existing `auto-launch` behavior;
/// `config_file_path` is unused there (those binaries run from a fixed install
/// dir, so the relative default config path still resolves).
#[cfg(target_os = "macos")]
pub fn update_auto_start_status(
    enable: bool,
    config_override: Option<&Path>,
) -> Result<(), DeskError> {
    use log::info;

    info!("Updating auto-start status (macOS LaunchAgent), enable: {enable}");
    if enable {
        let spec = crate::macos_agent::current_spec(config_override)?;
        crate::macos_agent::enable(&spec)?;
        info!("Successfully enabled auto-start LaunchAgent");
    } else {
        crate::macos_agent::disable()?;
        info!("Successfully disabled auto-start LaunchAgent");
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn update_auto_start_status(
    enable: bool,
    _config_override: Option<&Path>,
) -> Result<(), DeskError> {
    use auto_launch::AutoLaunchBuilder;
    use desk_utils::error::DeskErrorCode;
    use log::info;
    use std::env;

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
