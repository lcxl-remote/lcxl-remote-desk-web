mod error;
mod ipc_client;
mod platform;
mod private_screen;
mod security_approval;
mod whiteboard;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

static IS_EXITING: AtomicBool = AtomicBool::new(false);

use clap::Parser as _;
use lcxl_remote_desk_server::model::settings::{Args, Settings, StartupMode};
use private_screen::PrivateScreenManager;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
use whiteboard::WhiteboardManager;

use crate::error::DeskTauriError;

rust_i18n::i18n!("locales");

const MAIN_WINDOW_LABEL: &str = "main";

/// Windows service name registered by the install flow.
const SERVICE_NAME: &str = "LcxlDeskService";

pub fn run() -> Result<(), DeskTauriError> {
    let args = Args::parse();

    if desk_utils::permission::is_service_running(SERVICE_NAME) {
        log::info!("ServiceDaemon is running — launching as service shell (no embedded server)");
        // Load settings from the daemon's config file (absolute path stored in SCM).
        // This ensures the IPC token matches the one the daemon generated and persisted.
        let daemon_settings =
            lcxl_remote_desk_server::daemon::windows_service::get_service_config_path()
                .and_then(|p| {
                    let mut a = args.clone();
                    a.config_file_path = p.to_string_lossy().into_owned();
                    Settings::new(&a).ok()
                })
                .unwrap_or_else(|| Settings::new(&args).unwrap_or_default());
        run_tauri_service_shell(&daemon_settings)?;
    } else {
        let settings = Settings::new(&args)?;
        run_tauri_app(&settings)?;
    }

    Ok(())
}

/// Service-shell mode: the daemon owns the HTTP server; Tauri is a pure UI shell
/// that communicates with the daemon over a WebSocket IPC link.
fn run_tauri_service_shell(settings: &Settings) -> Result<(), DeskTauriError> {
    let ipc_token = settings.system.tauri_ipc_token.clone().unwrap_or_default();

    // Channels for GUI managers (same types as portable mode)
    let (ps_cmd_tx, ps_cmd_rx) = std::sync::mpsc::channel::<
        desk_input_injection::model::host_control::PrivateScreenCommand,
    >();
    let (state_tx, state_rx) = tokio::sync::mpsc::unbounded_channel::<
        desk_input_injection::model::host_control::HostControlEventType,
    >();
    let (wb_cmd_tx, wb_cmd_rx) =
        std::sync::mpsc::channel::<desk_input_injection::model::host_control::WhiteboardCommand>();
    let (sa_tx, sa_rx) = std::sync::mpsc::channel::<
        lcxl_remote_desk_server::model::security_approval::SecurityApprovalCommand,
    >();
    let (svc_op_tx, svc_op_rx) =
        std::sync::mpsc::sync_channel::<lcxl_remote_desk_server::ServiceOp>(8);

    // Shared token holder: IPC client writes the first token; window thread reads it.
    let token_holder: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Start IPC client in an actix runtime thread.
    let token_holder_ipc = Arc::clone(&token_holder);
    let ipc_token_clone = ipc_token.clone();
    let daemon_ws_url = "ws://127.0.0.1:8082/ws/tauri_ipc".to_string();
    std::thread::spawn(move || {
        let system = actix_rt::System::new();
        system.block_on(async move {
            ipc_client::run_ipc_loop(
                daemon_ws_url,
                ipc_token_clone,
                ps_cmd_tx,
                wb_cmd_tx,
                sa_tx,
                svc_op_tx,
                Some(state_rx),
                token_holder_ipc,
            )
            .await;
        });
    });

    // Service-op handler (ShellExecute runas — does not need Tauri handle).
    std::thread::spawn(move || {
        while let Ok(op) = svc_op_rx.recv() {
            handle_service_op(op);
        }
    });

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![])
        .setup(move |app| {
            let handle = app.handle().clone();
            let daemon_url = "http://127.0.0.1:8082".to_string();

            // Start GUI managers (reuse existing implementations).
            let ps_manager = PrivateScreenManager::new(handle.clone(), daemon_url.clone());
            ps_manager.start(ps_cmd_rx, state_tx);

            let wb_manager = WhiteboardManager::new(handle.clone(), daemon_url.clone());
            wb_manager.start(wb_cmd_rx);

            let sa_manager = crate::security_approval::SecurityApprovalManager::new(handle.clone());
            sa_manager.start(sa_rx);

            // Tray: show + quit only (no elevate in service-shell mode).
            {
                use tauri::menu::{Menu, MenuItem};
                use tauri::tray::{TrayIconBuilder, TrayIconEvent};

                let quit_i = MenuItem::with_id(app, "quit", "Exit", true, None::<&str>).unwrap();
                let show_i =
                    MenuItem::with_id(app, "show", "Open Window", true, None::<&str>).unwrap();
                let tray_menu = Menu::with_items(app, &[&show_i, &quit_i]).unwrap();
                let default_icon = app.default_window_icon().unwrap().clone();
                let _tray = TrayIconBuilder::new()
                    .menu(&tray_menu)
                    .icon(default_icon)
                    .tooltip("LCXL Remote Desktop")
                    .on_menu_event(|app, event| match event.id.as_ref() {
                        "quit" => {
                            IS_EXITING.store(true, Ordering::SeqCst);
                            app.exit(0);
                        }
                        "show" => {
                            if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                        _ => {}
                    })
                    .on_tray_icon_event(|tray, event| {
                        if let TrayIconEvent::DoubleClick { .. } = event
                            && let Some(window) =
                                tray.app_handle().get_webview_window(MAIN_WINDOW_LABEL)
                        {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    })
                    .build(app)
                    .expect("Failed to create tray icon");
            }

            // Spawn a thread to wait for the IPC token then open the webview.
            let handle_for_window = handle.clone();
            let token_holder_win = Arc::clone(&token_holder);
            std::thread::spawn(move || {
                let start = std::time::Instant::now();
                let timeout = std::time::Duration::from_secs(60);

                loop {
                    if start.elapsed() > timeout {
                        log::error!("[ServiceShell] Timeout waiting for IPC token from daemon");
                        return;
                    }

                    let token = token_holder_win.lock().unwrap().clone();
                    if let Some(token) = token {
                        let window_url = format!("http://127.0.0.1:8082?token={}", token);
                        log::info!("[ServiceShell] Opening window at: {}", window_url);

                        let handle_inner = handle_for_window.clone();
                        let _ = handle_for_window.run_on_main_thread(move || {
                            if handle_inner.get_webview_window(MAIN_WINDOW_LABEL).is_some() {
                                return;
                            }
                            match WebviewWindowBuilder::new(
                                &handle_inner,
                                MAIN_WINDOW_LABEL,
                                WebviewUrl::External(window_url.parse().unwrap()),
                            )
                            .title("LCXL Remote Desktop")
                            .inner_size(1200.0, 800.0)
                            .center()
                            .visible(false)
                            .on_page_load(|window, event| {
                                if let tauri::webview::PageLoadEvent::Finished = event.event() {
                                    let _ = window.show();
                                    let _ = window.set_focus();
                                }
                            })
                            .build()
                            {
                                Ok(_) => {}
                                Err(e) => log::error!("[ServiceShell] Window build error: {e}"),
                            }
                        });
                        return;
                    }

                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            });

            Ok(())
        })
        .build(tauri::generate_context!())?;

    app.run(|app, event| match event {
        tauri::RunEvent::WindowEvent {
            label,
            event: window_event,
            ..
        } => {
            if label == MAIN_WINDOW_LABEL
                && let tauri::WindowEvent::CloseRequested { api, .. } = window_event
            {
                api.prevent_close();
                if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
                    let _ = window.hide();
                }
            }
        }
        tauri::RunEvent::ExitRequested { api, .. } => {
            if !IS_EXITING.load(Ordering::SeqCst) {
                api.prevent_exit();
            }
        }
        _ => {}
    });

    Ok(())
}

/// Find the lcxl-remote-desk-server sidecar executable next to the Tauri app binary.
fn find_server_binary() -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        #[cfg(target_os = "windows")]
        let name = "lcxl-remote-desk-server.exe";
        #[cfg(not(target_os = "windows"))]
        let name = "lcxl-remote-desk-server";

        let candidate = dir.join(name);
        if candidate.exists() {
            return candidate;
        }
    }
    #[cfg(target_os = "windows")]
    return std::path::PathBuf::from("lcxl-remote-desk-server.exe");
    #[cfg(not(target_os = "windows"))]
    return std::path::PathBuf::from("lcxl-remote-desk-server");
}

/// Elevate and run `lcxl-remote-desk-server <args>` to install or uninstall the OS service.
fn handle_service_op(op: lcxl_remote_desk_server::ServiceOp) {
    let sidecar = find_server_binary();

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOW;
        use windows::core::PCWSTR;

        let params_str = match &op {
            lcxl_remote_desk_server::ServiceOp::Install { install_path } => {
                format!("--install-service --install-path \"{}\"", install_path)
            }
            lcxl_remote_desk_server::ServiceOp::Uninstall => "--uninstall-service".to_string(),
        };

        log::info!("Service op: running {} {}", sidecar.display(), params_str);

        let path: Vec<u16> = sidecar
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let operation: Vec<u16> = "runas\0".encode_utf16().collect();
        let params: Vec<u16> = params_str
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        unsafe {
            ShellExecuteW(
                None,
                PCWSTR(operation.as_ptr()),
                PCWSTR(path.as_ptr()),
                PCWSTR(params.as_ptr()),
                None,
                SW_SHOW,
            );
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let mut cmd = std::process::Command::new("pkexec");
        cmd.arg(&sidecar);
        match &op {
            lcxl_remote_desk_server::ServiceOp::Install { install_path } => {
                cmd.arg("--install-service")
                    .arg("--install-path")
                    .arg(install_path);
            }
            lcxl_remote_desk_server::ServiceOp::Uninstall => {
                cmd.arg("--uninstall-service");
            }
        }
        if let Err(e) = cmd.status() {
            log::error!("Service op failed: {e}");
        }
    }
}

pub fn run_tauri_app(settings: &Settings) -> Result<(), DeskTauriError> {
    let settings = settings.clone();
    let hidden_mode = settings.args.hidden;

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![])
        .setup(move |app| {
            let handle = app.handle().clone();

            // Channels for GUI managers — same pattern as service-shell mode.
            // Senders are cloned: one set for the embedded server (legacy direct
            // mpsc path; will be removed in Step 6) and one set for ipc_client
            // which receives broadcast commands from the host control hub.
            let (ps_cmd_tx, ps_cmd_rx) = std::sync::mpsc::channel::<
                desk_input_injection::model::host_control::PrivateScreenCommand,
            >();
            let (state_tx, state_rx) = tokio::sync::mpsc::unbounded_channel::<
                desk_input_injection::model::host_control::HostControlEventType,
            >();
            let (wb_cmd_tx, wb_cmd_rx) = std::sync::mpsc::channel::<
                desk_input_injection::model::host_control::WhiteboardCommand,
            >();
            let (sa_tx, sa_rx) = std::sync::mpsc::channel::<
                lcxl_remote_desk_server::model::security_approval::SecurityApprovalCommand,
            >();
            let (svc_op_tx, svc_op_rx) =
                std::sync::mpsc::sync_channel::<lcxl_remote_desk_server::ServiceOp>(8);

            // Read server port from settings before starting server thread
            let server_port = settings.system.port;
            let enable_ipv6 = settings.system.enable_ipv6;

            let host = if enable_ipv6 { "[::1]" } else { "127.0.0.1" };
            let mut frontend_host_port = format!("{}:{}", host, server_port);

            // Use Vite dev server (5174) automatically when running in debug mode (e.g. IDE Run/Debug)
            // Unless explicitly forced to use production build frontend via args
            if cfg!(debug_assertions) && !settings.args.prod_frontend {
                log::info!("Debug build detected, using vite dev server url for webview. (Use --prod-frontend to override)");
                frontend_host_port = "127.0.0.1:5174".to_string();
            } else if settings.args.dev_frontend {
                log::info!("--dev-frontend flag provided, using vite dev server url for webview.");
                frontend_host_port = "127.0.0.1:5174".to_string();
            }

            let frontend_url = format!("http://{}", frontend_host_port);

            // Start GUI managers (own the receivers).
            let ps_manager = PrivateScreenManager::new(handle.clone(), frontend_url.clone());
            ps_manager.start(ps_cmd_rx, state_tx);

            let wb_manager = WhiteboardManager::new(handle.clone(), frontend_url.clone());
            wb_manager.start(wb_cmd_rx);

            let sa_manager = crate::security_approval::SecurityApprovalManager::new(handle.clone());
            sa_manager.start(sa_rx);

            // Spawn handler for Install / Uninstall operations.
            std::thread::spawn(move || {
                while let Ok(op) = svc_op_rx.recv() {
                    handle_service_op(op);
                }
            });

            // The auto-login token is now delivered via the ws Ready first frame
            // so the embedded server doesn't need a pre-generated one.
            // The hub Local owns the broadcast channels for `/ws/tauri_ipc`.
            let host_control_hub =
                std::sync::Arc::new(lcxl_remote_desk_server::host_control::HostControlHub::new_local());

            // Direct-mpsc path remains so legacy business code (Step 3 will
            // migrate it to call the hub) keeps working unchanged.
            let channels = lcxl_remote_desk_server::ExternalChannels {
                private_screen_cmd_sender: Some(ps_cmd_tx.clone()),
                private_screen_state_receiver: Some(state_rx),
                tauri_login_token: None,
                whiteboard_cmd_sender: Some(wb_cmd_tx.clone()),
                security_approval_sender: Some(sa_tx.clone()),
                service_op_sender: Some(svc_op_tx.clone()),
            };
            let startup_mode = settings.args.startup_mode.clone();
            let server_settings = settings.clone();
            let hub_for_server = host_control_hub.clone();
            std::thread::spawn(move || {
                let system = actix_rt::System::new();
                system.block_on(async {
                    match lcxl_remote_desk_server::run_with_channels(
                        &server_settings,
                        channels,
                        Some(hub_for_server),
                    )
                    .await
                    {
                        Ok(server) => {
                            if let Err(e) = server.await {
                                log::error!("Server error: {}", e);
                            }
                        }
                        Err(e) => log::error!("Failed to start server: {}", e),
                    }
                });
            });

            // Token holder: ipc_client writes the token after ws Ready, the
            // window-spawn thread reads it to construct the auto-login URL.
            let token_holder: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

            // ipc_client connects to the embedded server's `/ws/tauri_ipc` over
            // loopback. In Step 2 the hub only emits the TauriToken first frame;
            // Step 3 will route business commands here as well.
            let ipc_token = settings.system.tauri_ipc_token.clone().unwrap_or_default();
            let ipc_host = if enable_ipv6 { "[::1]" } else { "127.0.0.1" };
            let daemon_ws_url = format!("ws://{}:{}/ws/tauri_ipc", ipc_host, server_port);
            let token_holder_ipc = Arc::clone(&token_holder);
            std::thread::spawn(move || {
                let system = actix_rt::System::new();
                system.block_on(async move {
                    ipc_client::run_ipc_loop(
                        daemon_ws_url,
                        ipc_token,
                        ps_cmd_tx,
                        wb_cmd_tx,
                        sa_tx,
                        svc_op_tx,
                        // state_rx is owned by the embedded server in Step 2.
                        None,
                        token_holder_ipc,
                    )
                    .await;
                });
            });

            // Setup Tray Icon & Menu
            use tauri::menu::{Menu, MenuItem};
            use tauri::tray::{TrayIconBuilder, TrayIconEvent};

            let quit_i = MenuItem::with_id(app, "quit", "Exit", true, None::<&str>).unwrap();
            let show_i = MenuItem::with_id(app, "show", "Open Window", true, None::<&str>).unwrap();

            let mut menu_items: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = vec![&show_i];

            let is_admin = desk_utils::permission::is_admin();
            let is_signaling = startup_mode == StartupMode::Signaling;
            let elevate_i = MenuItem::with_id(app, "elevate", "Elevate Privileges (提升权限)", true, None::<&str>).unwrap();
            if !is_admin && !is_signaling {
                menu_items.push(&elevate_i);
            }

            menu_items.push(&quit_i);
            let tray_menu = Menu::with_items(app, &menu_items).unwrap();
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
                        "elevate" => {
                            let curr_exe = std::env::current_exe().unwrap();
                            #[cfg(target_os = "windows")]
                            {
                                use windows::Win32::UI::Shell::ShellExecuteW;
                                use windows::Win32::UI::WindowsAndMessaging::SW_SHOW;
                                use windows::core::PCWSTR;
                                use std::os::windows::ffi::OsStrExt;

                                let mut path: Vec<u16> = curr_exe.as_os_str().encode_wide().collect();
                                path.push(0);
                                let mut operation: Vec<u16> = "runas".encode_utf16().collect();
                                operation.push(0);

                                unsafe {
                                    ShellExecuteW(
                                        None,
                                        PCWSTR(operation.as_ptr()),
                                        PCWSTR(path.as_ptr()),
                                        None,
                                        None,
                                        SW_SHOW,
                                    );
                                }
                                std::process::exit(0);
                            }

                            #[cfg(any(target_os = "linux", target_os = "macos"))]
                            {
                                let cmd = if cfg!(target_os = "macos") {
                                    format!("osascript -e 'do shell script \"{}\" with administrator privileges'", curr_exe.display())
                                } else {
                                    format!("pkexec \"{}\"", curr_exe.display())
                                };

                                std::process::Command::new("sh")
                                    .arg("-c")
                                    .arg(cmd)
                                    .spawn()
                                    .ok();
                                std::process::exit(0);
                            }
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::DoubleClick { .. } = event
                        && let Some(window) = tray.app_handle().get_webview_window(MAIN_WINDOW_LABEL) {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                })
                .build(app)
                .expect("Failed to create tray icon");

            // Spawn a thread to wait for server readiness + an auto-login token
            // (delivered via the ws Ready first frame), then open the main window.
            let handle_for_window = handle.clone();
            let token_holder_win = Arc::clone(&token_holder);
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
                            if let Ok(json) = response.into_json::<serde_json::Value>()
                                && let Some(init) = json
                                    .get("data")
                                    .and_then(|d| d.get("initialized"))
                                    .and_then(|i| i.as_bool())
                                {
                                    system_initialized = init;
                                }
                            break;
                        }
                        Err(e) => {
                            log::warn!("Check server ready error: {:?}", e);
                            std::thread::sleep(std::time::Duration::from_millis(500));
                        }
                    }
                }

                // Wait for the auto-login token from ipc_client (60s budget after
                // server readiness). Service-shell mode uses the same pattern.
                let token_deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
                let auto_token: Option<String> = loop {
                    if std::time::Instant::now() > token_deadline {
                        log::error!(
                            "Timeout waiting for auto-login token from /ws/tauri_ipc; opening window without auto-login"
                        );
                        break None;
                    }
                    if let Some(t) = token_holder_win.lock().unwrap().clone() {
                        break Some(t);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                };

                // If system is not initialized, go directly to /init to avoid losing the token in redirection
                // If it is initialized, load main app with token for auto login
                let window_url = if !system_initialized {
                    format!("http://{}/init?tauri=1", frontend_host_port)
                } else if let Some(token) = auto_token {
                    format!("http://{}?token={}", frontend_host_port, token)
                } else {
                    format!("http://{}", frontend_host_port)
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
                    .on_page_load(move |window, event| {
                        if let tauri::webview::PageLoadEvent::Finished = event.event() {
                            if !hidden_mode {
                                // show window after page load
                                let _ = window.show();
                                let _ = window.set_focus();
                            } else {
                                log::info!("Hidden mode enabled, main window remains hidden on page load.");
                            }
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
            tauri::RunEvent::WindowEvent {
                label,
                event: window_event,
                ..
            } => {
                if label == MAIN_WINDOW_LABEL
                    && let tauri::WindowEvent::CloseRequested { api, .. } = window_event
                {
                    api.prevent_close();
                    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
                        let _ = window.hide();
                    }
                }
            }
            tauri::RunEvent::ExitRequested { api, .. } => {
                if !IS_EXITING.load(Ordering::SeqCst) {
                    api.prevent_exit();
                }
            }
            _ => {}
        }
    });
    Ok(())
}
