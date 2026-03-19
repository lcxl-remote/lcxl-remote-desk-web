#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::process::ExitCode;

fn main() -> ExitCode {
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
