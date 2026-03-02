mod platform;
mod private_screen;

use private_screen::PrivateScreenManager;

pub fn run_tauri_app() {
    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(|app| {
            let handle = app.handle().clone();

            // Create server → tauri private screen command channel
            let (cmd_sender, cmd_receiver) = std::sync::mpsc::channel();
            // Create tauri → server private screen state channel
            let (state_sender, state_receiver) = tokio::sync::mpsc::unbounded_channel();

            // Start private screen manager (listen to commands from server)
            let ps_manager = PrivateScreenManager::new(handle.clone());

            // Send command to tauri which from server
            let (tauri_cmd_sender, tauri_cmd_receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                loop {
                    match cmd_receiver.recv() {
                        Ok(server_cmd) => {
                            if tauri_cmd_sender.send(server_cmd).is_err() {
                                log::warn!("Tauri private screen command channel closed");
                                break;
                            }
                        }
                        Err(_) => {
                            log::warn!("Server private screen command channel closed");
                            break;
                        }
                    }
                }
            });

            // Start private screen manager
            ps_manager.start(tauri_cmd_receiver, state_sender);

            // Start actix-web server (in a separate thread)
            let channels = lcxl_remote_desk_server::ExternalChannels {
                private_screen_cmd_sender: Some(cmd_sender),
                private_screen_state_receiver: Some(state_receiver),
            };

            std::thread::spawn(move || {
                let system = actix_rt::System::new();
                system.block_on(async {
                    match lcxl_remote_desk_server::run_with_channels(channels).await {
                        Ok(server) => {
                            if let Err(e) = server.await {
                                log::error!("Server error: {}", e);
                            }
                        }
                        Err(e) => log::error!("Failed to start server: {}", e),
                    }
                });
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| {
            // Prevent Tauri from exiting when all windows are closed/hidden.
            // This app is primarily a headless server; windows are created dynamically
            // (e.g. privacy screen) and may be hidden/destroyed at any time.
            if let tauri::RunEvent::ExitRequested { api, .. } = event {
                api.prevent_exit();
            }
        });
}
