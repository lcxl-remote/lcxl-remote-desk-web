mod error;
mod external_link;
mod host_access_status;
mod ipc_client;
#[cfg(target_os = "macos")]
mod macos_relocate;
mod overlay_window;
mod platform;
mod private_screen;
mod security_approval;
mod webview_webrtc;
mod whiteboard;

use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};

static IS_EXITING: AtomicBool = AtomicBool::new(false);
pub(crate) static HOST_ACCESS_BLOCKS_EXIT: AtomicBool = AtomicBool::new(false);

use clap::Parser as _;
use lcxl_remote_desk_server::model::settings::{Args, Settings};
// StartupMode only gates the (non-macOS) elevate tray item.
#[cfg(not(target_os = "macos"))]
use lcxl_remote_desk_server::model::settings::StartupMode;
use private_screen::PrivateScreenManager;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
use whiteboard::WhiteboardManager;

use crate::error::DeskTauriError;

rust_i18n::i18n!("locales");

pub(crate) const MAIN_WINDOW_LABEL: &str = "main";
pub(crate) const MAIN_TRAY_ID: &str = "main-tray";
static NATIVE_BRIDGE_STATE: OnceLock<Mutex<Option<(String, String, bool)>>> = OnceLock::new();

fn native_bridge_state() -> &'static Mutex<Option<(String, String, bool)>> {
    NATIVE_BRIDGE_STATE.get_or_init(|| Mutex::new(None))
}

fn native_bridge_script(token: &str, locale: &str, locale_persisted: bool) -> String {
    let detail = serde_json::json!({
        "token": token,
        "locale": locale,
        "localePersisted": locale_persisted,
    })
    .to_string();
    format!(
        "sessionStorage.setItem('lcxl.tauriShell','1');\
         sessionStorage.setItem('lcxl.nativeBridgeToken',{});\
         window.dispatchEvent(new CustomEvent('lcxl-native-bridge-ready',{{detail:{detail}}}));",
        serde_json::to_string(token).expect("serialize bridge token")
    )
}

pub(crate) fn inject_native_bridge_state(window: &tauri::WebviewWindow) {
    if let Some((token, locale, locale_persisted)) = native_bridge_state().lock().unwrap().clone() {
        let _ = window.eval(native_bridge_script(&token, &locale, locale_persisted));
    }
}

fn refresh_native_ui(app: &tauri::AppHandle, include_elevate: bool) {
    use tauri::menu::{Menu, MenuItem};

    let show = MenuItem::with_id(app, "show", rust_i18n::t!("tray.open"), true, None::<&str>)
        .expect("create localized show menu item");
    let status = MenuItem::with_id(
        app,
        "host_access_status",
        rust_i18n::t!("tray.remote_access_status"),
        true,
        None::<&str>,
    )
    .expect("create localized status menu item");
    let quit = MenuItem::with_id(app, "quit", rust_i18n::t!("tray.exit"), true, None::<&str>)
        .expect("create localized quit menu item");
    let elevate = MenuItem::with_id(
        app,
        "elevate",
        rust_i18n::t!("tray.elevate"),
        true,
        None::<&str>,
    )
    .expect("create localized elevate menu item");
    let mut items: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = vec![&show, &status];
    if include_elevate {
        items.push(&elevate);
    }
    items.push(&quit);
    if let Some(tray) = app.tray_by_id(MAIN_TRAY_ID)
        && let Ok(menu) = Menu::with_items(app, &items)
    {
        let _ = tray.set_menu(Some(menu));
    }
    host_access_status::refresh_tray_locale(app);
    for (label, window) in app.webview_windows() {
        let title = if label == MAIN_WINDOW_LABEL {
            Some(rust_i18n::t!("app_title"))
        } else if label.starts_with("host-access-status") {
            Some(rust_i18n::t!("remote_access_status_title"))
        } else if label == "private-screen" {
            Some(rust_i18n::t!("private_screen_title"))
        } else if label.starts_with("security-approval") {
            Some(rust_i18n::t!("security_approval_title"))
        } else if label == "whiteboard" {
            Some(rust_i18n::t!("whiteboard_title"))
        } else {
            None
        };
        if let Some(title) = title {
            let _ = window.set_title(title.as_ref());
        }
    }
}

fn start_native_bridge_event_loop(
    app: tauri::AppHandle,
    rx: std::sync::mpsc::Receiver<ipc_client::NativeBridgeEvent>,
    include_elevate: bool,
) {
    std::thread::spawn(move || {
        while let Ok(event) = rx.recv() {
            let (token, locale, locale_persisted, ready) = match event {
                ipc_client::NativeBridgeEvent::Ready {
                    token,
                    locale,
                    locale_persisted,
                } => (Some(token), locale, locale_persisted, true),
                ipc_client::NativeBridgeEvent::LocaleChanged { locale } => {
                    (None, locale, true, false)
                }
            };
            let Some(locale) = lcxl_remote_desk_server::locale::canonicalize(&locale) else {
                log::warn!("[NativeI18n] ignored unsupported locale {locale:?}");
                continue;
            };
            let _ = lcxl_remote_desk_server::locale::set_global_locale(locale);
            {
                let mut state = native_bridge_state().lock().unwrap();
                match (token, state.as_mut()) {
                    (Some(token), _) => {
                        *state = Some((token, locale.to_string(), locale_persisted))
                    }
                    (None, Some((_, current_locale, persisted))) => {
                        *current_locale = locale.to_string();
                        *persisted = true;
                    }
                    (None, None) => {}
                }
            }

            let app_for_main = app.clone();
            let locale = locale.to_string();
            let _ = app.run_on_main_thread(move || {
                refresh_native_ui(&app_for_main, include_elevate);
                for (_, window) in app_for_main.webview_windows() {
                    if ready {
                        inject_native_bridge_state(&window);
                    } else {
                        let detail = serde_json::json!({ "locale": locale }).to_string();
                        let _ = window.eval(format!(
                            "window.dispatchEvent(new CustomEvent('lcxl-global-locale-changed',\
                             {{detail:{detail}}}));"
                        ));
                    }
                }
            });
        }
    });
}

/// Platform service name registered by the install flow.
#[cfg(target_os = "windows")]
const SERVICE_NAME: &str = "LcxlDeskService";
#[cfg(target_os = "linux")]
const SERVICE_NAME: &str = "lcxl-remote-desk.service";
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
const SERVICE_NAME: &str = "LcxlDeskService";

/// Build the Tauri context. With the `macos-private-api` feature enabled,
/// `tauri::generate_context!()` embeds the macOS Info.plist as a static with a
/// fixed link symbol (`_EMBED_INFO_PLIST`), so expanding the macro at more than
/// one builder call site would emit that symbol twice and fail to link. Both
/// the service-shell and portable run paths funnel through this single
/// expansion to keep the symbol unique.
fn build_tauri_context() -> tauri::Context {
    tauri::generate_context!()
}

pub fn run() -> Result<(), DeskTauriError> {
    platform::prepare_tauri_window_backend();
    let args = Args::parse();

    // Before anything that depends on a stable bundle path (TCC prompts,
    // auto-start), offer to move into /Applications on a foreground launch. A
    // hidden (auto-start) launch is already guarded to /Applications, so skip it.
    #[cfg(target_os = "macos")]
    if !args.hidden {
        macos_relocate::maybe_offer_relocate();
    }

    if desk_utils::permission::is_service_running(SERVICE_NAME) {
        log::info!("ServiceDaemon is running — launching as service shell (no embedded server)");
        let daemon_settings = Settings::new(&args)?;
        run_tauri_service_shell(&daemon_settings)?;
    } else {
        let settings = Settings::new(&args)?;
        run_tauri_app(&settings)?;
    }

    Ok(())
}

/// Service-shell mode: the daemon owns the HTTP server; Tauri is a pure UI shell
/// that communicates with the daemon over a WebSocket IPC link.
///
/// Logging note: portable mode (`run_tauri_app`) launches the embedded server
/// in-process, which calls `init_telemetry` and installs a global tracing
/// subscriber that already captures every `log::*` macro from this crate.
/// Service-shell mode does NOT launch that server, so we install a slim
/// tracing subscriber here via [`telemetry::init_tauri_shell_telemetry`]
/// instead. The two paths are mutually exclusive at runtime, so neither can
/// shadow the other.
fn run_tauri_service_shell(settings: &Settings) -> Result<(), DeskTauriError> {
    // Hold the WorkerGuard alive for the full process lifetime — dropping it
    // early would close the non-blocking writer thread mid-run.
    let _telemetry_guard = match lcxl_remote_desk_server::telemetry::init_tauri_shell_telemetry(
        &settings.log.log_level,
        settings.paths().log_dir(),
    ) {
        Ok(g) => Some(g),
        Err(e) => {
            // No subscriber installed: fall through to a silent run. Surface
            // the reason on stderr so a debug build still shows it.
            eprintln!("[ServiceShell] telemetry init failed: {e}");
            None
        }
    };

    log::info!("[ServiceShell] starting; daemon ws endpoint = ws://127.0.0.1:8082/ws/tauri_ipc");
    let ipc_token = settings.system.tauri_ipc_token.clone().unwrap_or_default();
    let remote_access_paths = settings.paths().clone();
    log::info!(
        "[ServiceShell] ipc_token len={} (empty={})",
        ipc_token.len(),
        ipc_token.is_empty()
    );

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
    let (host_access_tx, host_access_rx) =
        std::sync::mpsc::channel::<host_access_status::HostAccessStatusCommand>();
    let (native_bridge_tx, native_bridge_rx) =
        std::sync::mpsc::channel::<ipc_client::NativeBridgeEvent>();

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
                host_access_tx,
                Some(state_rx),
                token_holder_ipc,
                native_bridge_tx,
                true,
            )
            .await;
        });
    });

    // Service-op handler (ShellExecute runas — does not need Tauri handle).
    #[cfg(target_os = "linux")]
    let service_config_override = Some(remote_access_paths.config_file().to_path_buf());
    #[cfg(not(target_os = "linux"))]
    let service_config_override = remote_access_paths
        .explicit_config_file()
        .map(std::path::Path::to_path_buf);
    std::thread::spawn(move || {
        while let Ok(op) = svc_op_rx.recv() {
            handle_service_op(op, service_config_override.as_deref());
        }
    });

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![])
        .setup(move |app| {
            let handle = app.handle().clone();
            let daemon_url = "http://127.0.0.1:8082".to_string();

            // Start GUI managers (reuse existing implementations).
            let ps_manager = PrivateScreenManager::new(handle.clone(), daemon_url.clone());
            ps_manager.start(ps_cmd_rx, state_tx);

            let wb_manager = WhiteboardManager::new(handle.clone(), daemon_url.clone());
            wb_manager.start(wb_cmd_rx);

            let sa_manager = crate::security_approval::SecurityApprovalManager::new(
                handle.clone(),
                daemon_url.clone(),
            );
            sa_manager.start(sa_rx);
            host_access_status::HostAccessStatusManager::new(
                handle.clone(),
                daemon_url.clone(),
                &remote_access_paths,
            )
            .start(host_access_rx);

            // Tray: show + quit only (no elevate in service-shell mode).
            {
                use tauri::menu::{Menu, MenuItem};
                use tauri::tray::{TrayIconBuilder, TrayIconEvent};

                let quit_i =
                    MenuItem::with_id(app, "quit", rust_i18n::t!("tray.exit"), true, None::<&str>)
                        .unwrap();
                let show_i =
                    MenuItem::with_id(app, "show", rust_i18n::t!("tray.open"), true, None::<&str>)
                        .unwrap();
                let status_i = MenuItem::with_id(
                    app,
                    "host_access_status",
                    rust_i18n::t!("tray.remote_access_status"),
                    true,
                    None::<&str>,
                )
                .unwrap();
                let tray_menu = Menu::with_items(app, &[&show_i, &status_i, &quit_i]).unwrap();
                let default_icon = app.default_window_icon().unwrap().clone();
                let _tray = TrayIconBuilder::with_id(MAIN_TRAY_ID)
                    .menu(&tray_menu)
                    .icon(default_icon)
                    .tooltip(rust_i18n::t!("app_title"))
                    .on_menu_event(|app, event| match event.id.as_ref() {
                        "quit" => {
                            if HOST_ACCESS_BLOCKS_EXIT.load(Ordering::SeqCst)
                                && !confirm_exit_with_remote_access()
                            {
                                return;
                            }
                            IS_EXITING.store(true, Ordering::SeqCst);
                            app.exit(0);
                        }
                        "host_access_status" => {
                            host_access_status::show_status_windows(app);
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
            start_native_bridge_event_loop(handle.clone(), native_bridge_rx, false);

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
                        let window_url = format!("http://127.0.0.1:8082?token={token}&tauri=1");
                        log::info!("[ServiceShell] Opening window at: {}", window_url);

                        let handle_inner = handle_for_window.clone();
                        let _ = handle_for_window.run_on_main_thread(move || {
                            if handle_inner.get_webview_window(MAIN_WINDOW_LABEL).is_some() {
                                return;
                            }
                            let frontend_origin = window_url.parse().unwrap();
                            match WebviewWindowBuilder::new(
                                &handle_inner,
                                MAIN_WINDOW_LABEL,
                                WebviewUrl::External(window_url.parse().unwrap()),
                            )
                            .on_navigation(external_link::external_link_navigation_handler(
                                handle_inner.clone(),
                                frontend_origin,
                            ))
                            .title(rust_i18n::t!("app_title"))
                            .inner_size(1200.0, 800.0)
                            .center()
                            .visible(false)
                            .on_page_load(|window, event| {
                                if let tauri::webview::PageLoadEvent::Finished = event.event() {
                                    inject_native_bridge_state(&window);
                                    let _ = window.show();
                                    let _ = window.set_focus();
                                }
                            })
                            .build()
                            {
                                Ok(window) => {
                                    webview_webrtc::enable_webrtc_if_needed(
                                        &window,
                                        MAIN_WINDOW_LABEL,
                                    );
                                }
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
        .build(build_tauri_context())?;

    app.run(|app, event| match event {
        tauri::RunEvent::WindowEvent {
            label,
            event: window_event,
            ..
        } => {
            if let tauri::WindowEvent::CloseRequested { api, .. } = &window_event {
                if label == MAIN_WINDOW_LABEL {
                    api.prevent_close();
                    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
                        let _ = window.hide();
                    }
                } else if label.starts_with(security_approval::APPROVAL_LABEL_PREFIX) {
                    // A user closed an approval window: keep it alive long enough
                    // to submit a Deny via the page's hook, then let the backend
                    // tear it down. The webview can't detect a native close itself.
                    api.prevent_close();
                    security_approval::on_approval_window_close(app, &label);
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
fn handle_service_op(
    op: lcxl_remote_desk_server::ServiceOp,
    config_override: Option<&std::path::Path>,
) {
    let sidecar = find_server_binary();

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOW;
        use windows::core::PCWSTR;

        let params_str = match &op {
            lcxl_remote_desk_server::ServiceOp::Install {
                install_path,
                install_idd_driver,
            } => {
                let mut s = format!(
                    "--install-service --install-path {}",
                    quote_cmd_arg(install_path)
                );
                if *install_idd_driver {
                    s.push_str(" --install-idd-driver");
                }
                if let Some(path) = config_override {
                    s.push_str(" --config-file-path ");
                    s.push_str(&quote_cmd_arg(&path.to_string_lossy()));
                }
                s
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

    #[cfg(target_os = "linux")]
    {
        let mut cmd = std::process::Command::new("pkexec");
        cmd.arg(&sidecar);
        match &op {
            lcxl_remote_desk_server::ServiceOp::Install {
                install_path,
                install_idd_driver,
            } => {
                cmd.arg("--install-service")
                    .arg("--install-path")
                    .arg(install_path);
                // pkexec intentionally replaces the caller environment with a
                // minimal safe set, so explicitly carry the already-checked
                // development opt-in as a sidecar argument. The Tauri process
                // must itself have been launched with the flag; normal users
                // still cannot enter the experimental install path by default.
                if std::env::var(
                    lcxl_remote_desk_server::daemon::linux_service::EXPERIMENTAL_INSTALL_ENV,
                )
                .as_deref()
                    == Ok("1")
                {
                    cmd.arg("--experimental-linux-service-daemon");
                }
                if *install_idd_driver {
                    cmd.arg("--install-idd-driver");
                }
                if let Some(path) = config_override {
                    cmd.arg("--config-file-path").arg(path);
                }
            }
            lcxl_remote_desk_server::ServiceOp::Uninstall => {
                cmd.arg("--uninstall-service");
            }
        }
        if let Err(e) = cmd.status() {
            log::error!("Service op failed: {e}");
        }
    }

    #[cfg(target_os = "macos")]
    {
        // macOS does not use the OS-service install path. Unattended auto-start
        // is a per-user LaunchAgent managed entirely through this node's
        // /settings `auto_start` endpoint (server `macos_agent`); there is no
        // privileged service to install here. Keeping this a no-op preserves a
        // single management entry and avoids two code paths racing on the same
        // plist. The frontend also hides the service install/uninstall UI on
        // macOS, so this is not expected to be reached.
        let _ = (&sidecar, &op);
        log::warn!(
            "Service op requested on macOS; ignored (auto-start is managed via the LaunchAgent)"
        );
    }
}

/// Quote a single argument for an `lpParameters`-style command line so it
/// will round-trip back through `CommandLineToArgvW` to the exact original
/// string. The backend controller already rejects `"` and ASCII control
/// chars in `install_path`, but defence-in-depth: we never want a path
/// containing spaces or backslashes to corrupt the elevated sidecar's
/// argv. Implements the algorithm described in
/// "Everyone quotes command line arguments the wrong way"
/// (learn.microsoft.com/archive/blogs/twistylittlepassagesallalike).
#[cfg(target_os = "windows")]
fn quote_cmd_arg(arg: &str) -> String {
    // Empty arg must still produce a single empty token.
    if !arg.is_empty() && !arg.chars().any(|c| c.is_whitespace() || c == '"') {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        if ch == '\\' {
            backslashes += 1;
            continue;
        }
        if ch == '"' {
            // Each backslash that precedes a quote must be escaped, and
            // the quote itself escaped.
            for _ in 0..(2 * backslashes + 1) {
                out.push('\\');
            }
            out.push('"');
        } else {
            for _ in 0..backslashes {
                out.push('\\');
            }
            out.push(ch);
        }
        backslashes = 0;
    }
    // Trailing backslashes immediately before the closing quote must
    // also be doubled, otherwise they would escape the closing quote.
    for _ in 0..(2 * backslashes) {
        out.push('\\');
    }
    out.push('"');
    out
}

#[cfg(all(test, target_os = "windows"))]
mod quote_cmd_arg_tests {
    use super::quote_cmd_arg;
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::Shell::CommandLineToArgvW;
    use windows::core::PCWSTR;

    /// Feeds `quoted` (with a dummy program prefix) through
    /// CommandLineToArgvW and returns the parsed argv as UTF-8 strings.
    fn parse_via_winapi(quoted: &str) -> Vec<String> {
        let cmdline = format!("dummy.exe {quoted}");
        let wide: Vec<u16> = std::ffi::OsStr::new(&cmdline)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut argc: i32 = 0;
        unsafe {
            // CommandLineToArgvW returns *mut PWSTR (an array of
            // wide-string pointers) that the caller must LocalFree
            // when done.
            let argv = CommandLineToArgvW(PCWSTR(wide.as_ptr()), &mut argc);
            assert!(!argv.is_null());
            let mut out = Vec::with_capacity(argc as usize);
            for i in 0..(argc as usize) {
                let ptr = *argv.add(i);
                let mut len = 0;
                while *ptr.0.add(len) != 0 {
                    len += 1;
                }
                let slice = std::slice::from_raw_parts(ptr.0, len);
                out.push(String::from_utf16_lossy(slice));
            }
            let _ = windows::Win32::Foundation::LocalFree(Some(
                windows::Win32::Foundation::HLOCAL(argv as *mut _),
            ));
            out
        }
    }

    fn round_trip(arg: &str) {
        let quoted = quote_cmd_arg(arg);
        let argv = parse_via_winapi(&quoted);
        assert_eq!(argv.len(), 2, "for {arg:?} -> {quoted:?} got {argv:?}");
        assert_eq!(argv[1], arg, "for {arg:?} -> {quoted:?}");
    }

    #[test]
    fn simple_unquoted_path() {
        round_trip("C:\\foo\\bar");
    }

    #[test]
    fn path_with_spaces() {
        round_trip("C:\\Program Files\\LCXL Remote Desktop");
    }

    #[test]
    fn path_with_trailing_backslash() {
        round_trip("C:\\foo\\");
    }

    #[test]
    fn path_with_multiple_trailing_backslashes() {
        round_trip("C:\\foo\\\\\\");
    }

    #[test]
    fn arg_with_embedded_quote() {
        // The REST controller forbids `"` in install_path, but the
        // helper itself must still round-trip the character so we can
        // safely reuse it for other args in the future.
        round_trip(r#"C:\with"quote"#);
    }

    #[test]
    fn empty_arg_produces_empty_quoted_token() {
        let quoted = quote_cmd_arg("");
        let argv = parse_via_winapi(&quoted);
        assert_eq!(argv.len(), 2);
        assert_eq!(argv[1], "");
    }
}

pub fn run_tauri_app(settings: &Settings) -> Result<(), DeskTauriError> {
    let settings = settings.clone();
    let hidden_mode = settings.args.hidden;

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![])
        .setup(move |app| {
            let handle = app.handle().clone();

            // Channels connect the embedded server's host-control stream to the
            // GUI managers through the loopback IPC client.
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
            let (host_access_tx, host_access_rx) =
                std::sync::mpsc::channel::<host_access_status::HostAccessStatusCommand>();
            let (native_bridge_tx, native_bridge_rx) =
                std::sync::mpsc::channel::<ipc_client::NativeBridgeEvent>();

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

            let sa_manager = crate::security_approval::SecurityApprovalManager::new(
                handle.clone(),
                frontend_url.clone(),
            );
            sa_manager.start(sa_rx);
            host_access_status::HostAccessStatusManager::new(
                handle.clone(),
                frontend_url.clone(),
                settings.paths(),
            )
            .start(host_access_rx);

            // Spawn handler for Install / Uninstall operations.
            #[cfg(target_os = "linux")]
            let service_config_override = Some(settings.paths().config_file().to_path_buf());
            #[cfg(not(target_os = "linux"))]
            let service_config_override = settings
                .paths()
                .explicit_config_file()
                .map(std::path::Path::to_path_buf);
            std::thread::spawn(move || {
                while let Ok(op) = svc_op_rx.recv() {
                    handle_service_op(op, service_config_override.as_deref());
                }
            });

            // The hub Local owns the broadcast channels for `/ws/tauri_ipc`.
            // All overlay / approval / service-op traffic now flows through the
            // hub: the embedded server is the producer, ipc_client below is the
            // consumer that fans out into the GUI managers' mpsc channels.
            let host_control_hub =
                std::sync::Arc::new(lcxl_remote_desk_server::host_control::HostControlHub::new_local());

            #[cfg(not(target_os = "macos"))]
            let startup_mode = settings.args.startup_mode.clone();
            let server_settings = settings.clone();
            let hub_for_server = host_control_hub.clone();
            std::thread::spawn(move || {
                let system = actix_rt::System::new();
                system.block_on(async {
                    match lcxl_remote_desk_server::run_with_hub(
                        &server_settings,
                        Some(hub_for_server),
                    )
                    .await
                    {
                        Ok((server, _telemetry_guard)) => {
                            // Hold _telemetry_guard until server.await
                            // completes; dropping earlier closes the
                            // non-blocking log writer thread and silently
                            // discards all subsequent lines.
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
            // loopback and routes business commands to the GUI managers.
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
                        host_access_tx,
                        // The hub is the single source of truth for private-screen
                        // state, so GUI events are forwarded back over the socket.
                        Some(state_rx),
                        token_holder_ipc,
                        native_bridge_tx,
                        false,
                    )
                    .await;
                });
            });

            // Setup Tray Icon & Menu
            use tauri::menu::{Menu, MenuItem};
            use tauri::tray::{TrayIconBuilder, TrayIconEvent};

            let quit_i = MenuItem::with_id(
                app,
                "quit",
                rust_i18n::t!("tray.exit"),
                true,
                None::<&str>,
            )
            .unwrap();
            let show_i = MenuItem::with_id(
                app,
                "show",
                rust_i18n::t!("tray.open"),
                true,
                None::<&str>,
            )
            .unwrap();
            let status_i = MenuItem::with_id(
                app,
                "host_access_status",
                rust_i18n::t!("tray.remote_access_status"),
                true,
                None::<&str>,
            )
            .unwrap();

            let mut menu_items: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> =
                vec![&show_i, &status_i];

            // macOS has no UAC / integrity-level model: capabilities are gated by
            // TCC (per-app/per-user, orthogonal to uid — even root can't bypass
            // it), and capture / injection / user file-management need no root.
            // Relaunching as root via osascript would actually DROP the app's TCC
            // grants. So there is no "Elevate Privileges" item on macOS.
            #[cfg(not(target_os = "macos"))]
            let elevate_i = MenuItem::with_id(
                app,
                "elevate",
                rust_i18n::t!("tray.elevate"),
                true,
                None::<&str>,
            )
            .unwrap();
            #[cfg(not(target_os = "macos"))]
            let include_elevate = {
                let is_admin = desk_utils::permission::is_admin();
                let is_signaling = startup_mode == StartupMode::Signaling;
                !is_admin && !is_signaling
            };
            #[cfg(target_os = "macos")]
            let include_elevate = false;
            #[cfg(not(target_os = "macos"))]
            {
                if include_elevate {
                    menu_items.push(&elevate_i);
                }
            }

            menu_items.push(&quit_i);
            let tray_menu = Menu::with_items(app, &menu_items).unwrap();
            let default_icon = app.default_window_icon().unwrap().clone();

            let _tray = TrayIconBuilder::with_id(MAIN_TRAY_ID)
                .menu(&tray_menu)
                .icon(default_icon)
                .tooltip(rust_i18n::t!("app_title"))
                .on_menu_event(|app, event| {
                    match event.id.as_ref() {
                        "quit" => {
                            if HOST_ACCESS_BLOCKS_EXIT.load(Ordering::SeqCst)
                                && !confirm_exit_with_remote_access()
                            {
                                return;
                            }
                            IS_EXITING.store(true, Ordering::SeqCst);
                            app.exit(0);
                        },
                        "host_access_status" => {
                            host_access_status::show_status_windows(app);
                        }
                        "show" => {
                            if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                        "elevate" => {
                            // No elevate item exists on macOS (TCC, not root,
                            // gates capability), so this arm is a no-op there.
                            // Windows uses ShellExecute runas; Linux uses pkexec.
                            #[cfg(not(target_os = "macos"))]
                            {
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

                                #[cfg(target_os = "linux")]
                                {
                                    let cmd = format!("pkexec \"{}\"", curr_exe.display());
                                    std::process::Command::new("sh")
                                        .arg("-c")
                                        .arg(cmd)
                                        .spawn()
                                        .ok();
                                    std::process::exit(0);
                                }
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
            start_native_bridge_event_loop(
                handle.clone(),
                native_bridge_rx,
                include_elevate,
            );

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
                    format!("http://{}?token={}&tauri=1", frontend_host_port, token)
                } else {
                    format!("http://{}?tauri=1", frontend_host_port)
                };

                log::info!("Opening main window at: {}", window_url);

                let handle = handle_for_window.clone();
                let _ = handle_for_window.run_on_main_thread(move || {
                    if let Some(existing) = handle.get_webview_window(MAIN_WINDOW_LABEL) {
                        let _ = existing.set_focus();
                        return;
                    }

                    let frontend_origin = window_url.parse().unwrap();
                    match WebviewWindowBuilder::new(
                        &handle,
                        MAIN_WINDOW_LABEL,
                        WebviewUrl::External(window_url.parse().unwrap()),
                    )
                    .on_navigation(external_link::external_link_navigation_handler(
                        handle.clone(),
                        frontend_origin,
                    ))
                    .title(rust_i18n::t!("app_title"))
                    .inner_size(1200.0, 800.0)
                    .center()
                    .visible(false) // hide window first to avoid long white screen
                    .on_page_load(move |window, event| {
                        if let tauri::webview::PageLoadEvent::Finished = event.event() {
                            inject_native_bridge_state(&window);
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
                            webview_webrtc::enable_webrtc_if_needed(&window, MAIN_WINDOW_LABEL);
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
        .build(build_tauri_context())?;

    app.run(|app, event| {
        // Intercept window close events to hide instead of quit
        match event {
            tauri::RunEvent::WindowEvent {
                label,
                event: window_event,
                ..
            } => {
                if let tauri::WindowEvent::CloseRequested { api, .. } = &window_event {
                    if label == MAIN_WINDOW_LABEL {
                        api.prevent_close();
                        if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
                            let _ = window.hide();
                        }
                    } else if label.starts_with(security_approval::APPROVAL_LABEL_PREFIX) {
                        // A user closed an approval window: keep it alive long
                        // enough to submit a Deny via the page's hook, then let
                        // the backend tear it down. The webview can't detect a
                        // native close itself.
                        api.prevent_close();
                        security_approval::on_approval_window_close(app, &label);
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

/// Exiting the UI must never imply unlocking the daemon. This confirmation is
/// intentionally native because the status window itself may be the last
/// remaining webview and must not be trusted to reinterpret the choice.
#[cfg(target_os = "windows")]
fn confirm_exit_with_remote_access() -> bool {
    use std::os::windows::ffi::OsStrExt as _;
    use windows::Win32::UI::WindowsAndMessaging::{
        IDYES, MB_ICONWARNING, MB_SETFOREGROUND, MB_YESNO, MessageBoxW,
    };
    use windows::core::PCWSTR;

    let text_value = rust_i18n::t!("host_access_exit_confirm");
    let text: Vec<u16> = std::ffi::OsStr::new(text_value.as_ref())
        .encode_wide()
        .chain(Some(0))
        .collect();
    let title_value = rust_i18n::t!("host_access_dialog_title");
    let title: Vec<u16> = std::ffi::OsStr::new(title_value.as_ref())
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_YESNO | MB_ICONWARNING | MB_SETFOREGROUND,
        ) == IDYES
    }
}

#[cfg(target_os = "linux")]
fn confirm_exit_with_remote_access() -> bool {
    std::process::Command::new("zenity")
        .arg("--question")
        .arg(format!(
            "--title={}",
            rust_i18n::t!("host_access_dialog_title")
        ))
        .arg(format!(
            "--text={}",
            rust_i18n::t!("host_access_exit_confirm")
        ))
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(target_os = "macos")]
fn confirm_exit_with_remote_access() -> bool {
    let message = serde_json::to_string(rust_i18n::t!("host_access_exit_confirm").as_ref())
        .unwrap_or_else(|_| "\"Exit?\"".to_string());
    let title = serde_json::to_string(rust_i18n::t!("host_access_dialog_title").as_ref())
        .unwrap_or_else(|_| "\"LCXL Remote Desktop\"".to_string());
    let cancel = serde_json::to_string(rust_i18n::t!("button_cancel").as_ref()).unwrap();
    let exit = serde_json::to_string(rust_i18n::t!("button_exit").as_ref()).unwrap();
    let script = format!(
        "display dialog {message} with title {title} buttons {{{cancel}, {exit}}} default button {cancel} with icon caution"
    );
    let check_exit = format!("if button returned of result is {exit} then return \"yes\"");
    std::process::Command::new("osascript")
        .args(["-e", &script, "-e", &check_exit])
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("yes")
        })
}
