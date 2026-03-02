#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use lcxl_remote_desk_server::model::settings::StartupMode;

fn main() {
    // Parse startup mode
    let startup_mode = parse_startup_mode_from_args();

    match startup_mode {
        StartupMode::Signaling => {
            // Pure signaling mode, no Tauri window needed
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let server = lcxl_remote_desk_server::run().await.unwrap();
                server.await.unwrap();
            });
        }
        _ => {
            // Default or DeskServer mode, start Tauri
            lcxl_remote_desk_tauri::run_tauri_app();
        }
    }
}

fn parse_startup_mode_from_args() -> StartupMode {
    let args: Vec<String> = std::env::args().collect();
    for (i, arg) in args.iter().enumerate() {
        if (arg == "--startup-mode" || arg == "-s") && i + 1 < args.len() {
            return match args[i + 1].as_str() {
                "signaling" => StartupMode::Signaling,
                "desk-server" => StartupMode::DeskServer,
                _ => StartupMode::Default,
            };
        }
    }
    StartupMode::Default
}
