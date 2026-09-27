#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::process::ExitCode;

fn main() -> ExitCode {
    // Before anything else: the sandbox child's stdout carries only its frame.
    if let Some(code) = lcxl_remote_desk_server::typst_sandbox::run_child_if_requested() {
        return ExitCode::from(code as u8);
    }
    #[cfg(windows)]
    if let Some(code) = lcxl_remote_desk_server::windows_application_host::run_if_requested() {
        return ExitCode::from(code as u8);
    }
    let run_result = lcxl_remote_desk_tauri::run();
    match run_result {
        Ok(_) => {
            log::info!("Server exit normally");
            ExitCode::SUCCESS
        }
        Err(e) => {
            log::error!("Server exit with error: {}", e);
            // log may not be initialized, so print to stderr
            eprintln!("Server exit with error: {}", e);
            ExitCode::FAILURE
        }
    }
}
