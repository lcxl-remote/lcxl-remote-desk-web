mod platform;
mod private_screen;
mod whiteboard;
mod error;

use std::sync::atomic::{AtomicBool, Ordering};

static IS_EXITING: AtomicBool = AtomicBool::new(false);

use clap::Parser as _;
use lcxl_remote_desk_server::model::settings::{Args, Settings, StartupMode};
use private_screen::PrivateScreenManager;
use whiteboard::WhiteboardManager;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

use crate::error::DeskTauriError;

const MAIN_WINDOW_LABEL: &str = "main";

pub fn run()->Result<(), DeskTauriError> {
    let args = Args::parse();
    let settings = Settings::new(&args)?;
    // Parse startup mode
    let startup_mode = settings.args.startup_mode.clone();

    match startup_mode {
        StartupMode::Signaling => {
            // Pure signaling mode, no Tauri window needed
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(async {
                let server = lcxl_remote_desk_server::run().await.unwrap();
                server.await.unwrap();
            });
        }
        _ => {
            // Default or DeskServer mode, start Tauri
            run_tauri_app(&settings)?;
        }
    };
    Ok(())
}


pub fn run_tauri_app(settings: &Settings)->Result<(), DeskTauriError> {
    let settings = settings.clone();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(move |app| {
            let handle = app.handle().clone();

            // Create server → tauri private screen command channel
            let (cmd_sender, cmd_receiver) = std::sync::mpsc::channel();
            // Create tauri → server private screen state channel
            let (state_sender, state_receiver) = tokio::sync::mpsc::unbounded_channel();

            // Read server port from settings before starting server thread
            let server_port = settings.system.port;
            let enable_ipv6 = settings.system.enable_ipv6;

            let host = if enable_ipv6 { "[::1]" } else { "127.0.0.1" };
            let mut frontend_host_port = format!("{}:{}", host, server_port);

            // Use Vite dev server (5173) automatically when running in debug mode (e.g. IDE Run/Debug)
            // Unless explicitly forced to use production build frontend via args
            if cfg!(debug_assertions) && !settings.args.prod_frontend {
                log::info!("Debug build detected, using vite dev server url for webview. (Use --prod-frontend to override)");
                frontend_host_port = "127.0.0.1:5173".to_string(); 
            } else if settings.args.dev_frontend {
                log::info!("--dev-frontend flag provided, using vite dev server url for webview.");
                frontend_host_port = "127.0.0.1:5173".to_string(); 
            }

            let frontend_url = format!("http://{}", frontend_host_port);

            // Start private screen manager (listen to commands from server)
            let ps_manager = PrivateScreenManager::new(handle.clone(), frontend_url.clone());

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

            // Create whiteboard command channel
            let (wb_cmd_sender, wb_cmd_receiver) = std::sync::mpsc::channel();

            // Start whiteboard manager
            let wb_manager = WhiteboardManager::new(handle.clone(), frontend_url.clone());
            wb_manager.start(wb_cmd_receiver);

            // Generate a one-time login token
            let tauri_token = uuid::Uuid::new_v4().to_string();
            let tauri_token_for_window = tauri_token.clone();

            // Start actix-web server (in a separate thread)
            let channels = lcxl_remote_desk_server::ExternalChannels {
                private_screen_cmd_sender: Some(cmd_sender),
                private_screen_state_receiver: Some(state_receiver),
                tauri_login_token: Some(tauri_token),
                whiteboard_cmd_sender: Some(wb_cmd_sender),
            };

            // Start actix-web server (in a separate thread)
            std::thread::spawn(move || {
                let system = actix_rt::System::new();
                system.block_on(async {
                    match lcxl_remote_desk_server::run_with_channels(&settings, channels).await {
                        Ok(server) => {
                            if let Err(e) = server.await {
                                log::error!("Server error: {}", e);
                            }
                        }
                        Err(e) => log::error!("Failed to start server: {}", e),
                    }
                });
            });

            // Setup Tray Icon & Menu
            use tauri::menu::{Menu, MenuItem};
            use tauri::tray::{TrayIconBuilder, TrayIconEvent};

            let quit_i = MenuItem::with_id(app, "quit", "Exit", true, None::<&str>).unwrap();
            let show_i = MenuItem::with_id(app, "show", "Open Window", true, None::<&str>).unwrap();
            let tray_menu = Menu::with_items(app, &[&show_i, &quit_i]).unwrap();
            let default_icon = app.default_window_icon().unwrap().clone();
            
            let _tray = TrayIconBuilder::new()
                .menu(&tray_menu)
                .icon(default_icon)
                .tooltip("LCXL Remote Desktop")
                .on_menu_event(|app, event| {
                    match event.id.as_ref() {
                        "quit" => {
                            IS_EXITING.store(true, Ordering::SeqCst);
                            app.exit(0);
                        },
                        "show" => {
                            if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::DoubleClick { .. } = event {
                        if let Some(window) = tray.app_handle().get_webview_window(MAIN_WINDOW_LABEL) {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                })
                .build(app)
                .expect("Failed to create tray icon");

            // Spawn a thread to wait for server readiness and open the main window
            let handle_for_window = handle.clone();
            std::thread::spawn(move || {
                let server_url = format!("http://{}:{}", host, server_port);
                let check_url = format!("{}/api/server_info", server_url);

                // Poll until server is ready (max 30 seconds)
                let start = std::time::Instant::now();
                let timeout = std::time::Duration::from_secs(30);

                // Keep track of initialization state from server_info response
                let mut system_initialized = true;

                log::info!("Checking server readiness at: {}", check_url);
                loop {
                    if start.elapsed() > timeout {
                        log::error!("Timeout waiting for server to become ready");
                        return;
                    }

                    match ureq::get(&check_url)
                        .timeout(std::time::Duration::from_secs(2))
                        .call()
                    {
                        Ok(response) => {
                            log::info!("Server is ready");
                            // Parse JSON response to check initialized status
                            if let Ok(json) = response.into_json::<serde_json::Value>() {
                                if let Some(init) = json
                                    .get("data")
                                    .and_then(|d| d.get("initialized"))
                                    .and_then(|i| i.as_bool())
                                {
                                    system_initialized = init;
                                }
                            }
                            break;
                        }
                        Err(e) => {
                            log::warn!("Check server ready error: {:?}", e);
                            std::thread::sleep(std::time::Duration::from_millis(500));
                        }
                    }
                }

                // If system is not initialized, go directly to /init to avoid losing the token in redirection
                // If it is initialized, load main app with token for auto login
                let window_url = if system_initialized {
                    format!(
                        "http://{}?token={}",
                        frontend_host_port, tauri_token_for_window
                    )
                } else {
                    format!("http://{}/init?tauri=1", frontend_host_port)
                };

                log::info!("Opening main window at: {}", window_url);

                let handle = handle_for_window.clone();
                let _ = handle_for_window.run_on_main_thread(move || {
                    if let Some(existing) = handle.get_webview_window(MAIN_WINDOW_LABEL) {
                        let _ = existing.set_focus();
                        return;
                    }

                    match WebviewWindowBuilder::new(
                        &handle,
                        MAIN_WINDOW_LABEL,
                        WebviewUrl::External(window_url.parse().unwrap()),
                    )
                    .title("LCXL Remote Desktop")
                    .inner_size(1200.0, 800.0)
                    .center()
                    .visible(false) // hide window first to avoid long white screen
                    .on_page_load(|window, event| {
                        if let tauri::webview::PageLoadEvent::Finished = event.event() {
                            // show window after page load
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    })
                    .build()
                    {
                        Ok(window) => {
                            log::info!("Window built successfully. Waiting for page load to finish...");
                        }
                        Err(e) => {
                            log::error!("Failed to create main window: {}", e);
                        }
                    }
                });
            });

            Ok(())
        })
        .build(tauri::generate_context!())?;

        app.run(|app, event| {
            // Intercept window close events to hide instead of quit
            match event {
                tauri::RunEvent::WindowEvent { label, event: window_event, .. } => {
                    if label == MAIN_WINDOW_LABEL {
                        if let tauri::WindowEvent::CloseRequested { api, .. } = window_event {
                            api.prevent_close();
                            if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
                                let _ = window.hide();
                            }
                        }
                    }
                },
                tauri::RunEvent::ExitRequested { api, .. } => {
                    if !IS_EXITING.load(Ordering::SeqCst) {
                        api.prevent_exit();
                    }
                },
                _ => {}
            }
        });
        Ok(())
}
