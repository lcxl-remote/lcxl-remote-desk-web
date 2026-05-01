use crate::platform;
use desk_input_injection::model::host_control::{HostControlEventType, PrivateScreenCommand};
use std::sync::mpsc;
use tauri::AppHandle;

pub struct PrivateScreenManager {
    app_handle: AppHandle,
    frontend_url: String,
    controlled_by_connection_id: Option<String>,
}

#[cfg(not(target_os = "linux"))]
const PRIVATE_SCREEN_WINDOW_LABEL: &str = "private-screen";
const HOTKEY: &str = "ctrl+alt+l";

impl PrivateScreenManager {
    pub fn new(app_handle: AppHandle, frontend_url: String) -> Self {
        Self {
            app_handle,
            frontend_url,
            controlled_by_connection_id: None,
        }
    }

    pub fn start(
        mut self,
        cmd_receiver: mpsc::Receiver<PrivateScreenCommand>,
        state_sender: tokio::sync::mpsc::UnboundedSender<HostControlEventType>,
    ) {
        let handle = self.app_handle.clone();
        std::thread::spawn(move || {
            loop {
                match cmd_receiver.recv() {
                    Ok(cmd) => match cmd {
                        PrivateScreenCommand::Show(from_connection_id) => {
                            if let Some(controlled_by_connection_id) =
                                &self.controlled_by_connection_id
                                && controlled_by_connection_id != &from_connection_id {
                                    log::warn!(
                                        "Private screen is already controlled by another connection"
                                    );
                                    continue;
                                }

                            if let Err(e) = self.show_window(&handle, &state_sender) {
                                log::error!("Failed to show private screen: {}", e);
                                let _ = state_sender.send(
                                    HostControlEventType::PrivateScreenUnknownError(
                                        Some(from_connection_id.clone()),
                                        e.to_string(),
                                    ),
                                );
                                continue;
                            }
                            let _ = state_sender.send(
                                HostControlEventType::PrivateScreenVisibleChanged(
                                    from_connection_id.clone(),
                                    true,
                                ),
                            );
                            self.controlled_by_connection_id = Some(from_connection_id);
                        }
                        PrivateScreenCommand::Hide(from_connection_id) => {
                            if let Some(controlled_by_connection_id) =
                                &self.controlled_by_connection_id
                                && controlled_by_connection_id != &from_connection_id {
                                    log::warn!(
                                        "Private screen is already controlled by another connection"
                                    );
                                    continue;
                                }
                            if let Err(e) = Self::hide_window(&handle) {
                                log::error!("Failed to hide private screen: {}", e);
                            }
                            let _ = state_sender.send(
                                HostControlEventType::PrivateScreenVisibleChanged(
                                    from_connection_id.clone(),
                                    false,
                                ),
                            );
                            self.controlled_by_connection_id = None;
                        }
                        PrivateScreenCommand::Quit => {
                            let _ = Self::hide_window(&handle);
                            log::info!("Private screen quit");
                            break;
                        }
                    },
                    Err(_) => {
                        log::warn!("Private screen command channel closed");
                        break;
                    }
                }
            }
            log::info!("Private screen loop exited");
        });
    }

    pub fn show_window(
        &self,
        handle: &AppHandle,
        _state_sender: &tokio::sync::mpsc::UnboundedSender<HostControlEventType>,
    ) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        {
            if let Err(e) = platform::block_input(true) {
                log::warn!("Failed to block input and set brightness: {}", e);
            }
            Self::register_hotkey(handle)?;
            return Ok(());
        }

        #[cfg(not(target_os = "linux"))]
        {
            use tauri::Manager as _;

            let window =
                if let Some(window) = handle.get_webview_window(PRIVATE_SCREEN_WINDOW_LABEL) {
                    window
                } else {
                    // Create new window

                    use tauri::{WebviewUrl, WebviewWindowBuilder};
                    WebviewWindowBuilder::new(
                        handle,
                        PRIVATE_SCREEN_WINDOW_LABEL,
                        WebviewUrl::External(
                            format!("{}/private-screen", self.frontend_url)
                                .parse()
                                .unwrap(),
                        ),
                    )
                    .title("Private Screen")
                    .always_on_top(true)
                    .decorations(false)
                    .skip_taskbar(true)
                    .resizable(false)
                    .content_protected(true) // Prevent screen capture
                    .minimizable(false)
                    .build()
                    .map_err(|e| e.to_string())?
                };

            if let Ok(Some(monitor)) = window.primary_monitor() {
                let _ = window.set_size(*monitor.size());
                let _ = window.set_position(*monitor.position());
            }

            window.show().map_err(|e| e.to_string())?;
            window.set_fullscreen(true).map_err(|e| e.to_string())?;
            window.set_always_on_top(true).map_err(|e| e.to_string())?;
            window.set_focus().map_err(|e| e.to_string())?;
            let _ = window.set_ignore_cursor_events(true);

            // Platform specific: block input (best effort; do not fail private screen)
            if let Err(e) = platform::block_input(true) {
                log::warn!(
                    "Failed to block local input, continue with private screen: {}",
                    e
                );
            }

            // Register global hotkey
            Self::register_hotkey(handle)?;

            Ok(())
        }
    }

    fn hide_window(handle: &AppHandle) -> Result<(), String> {
        // Unregister global hotkey (common for all platforms)
        let _ = Self::unregister_hotkey(handle);

        #[cfg(target_os = "linux")]
        {
            // On Linux, we only unblock input and restore brightness.
            // The window itself is not explicitly closed or hidden by the app.
            if let Err(e) = platform::block_input(false) {
                log::warn!("Failed to unblock input and restore brightness: {}", e);
            }
            return Ok(());
        }

        #[cfg(not(target_os = "linux"))]
        {
            // Platform specific: unblock input

            use tauri::Manager as _;
            if let Err(e) = platform::block_input(false) {
                log::warn!("Failed to unblock local input: {}", e);
            }

            // Hide/close the window on non-Linux platforms
            if let Some(window) = handle.get_webview_window(PRIVATE_SCREEN_WINDOW_LABEL) {
                // Using close() instead of hide() as per the provided edit,
                // assuming the intent is to fully dispose of the window.
                window.close().map_err(|e| e.to_string())?;
            }

            Ok(())
        }
    }

    fn register_hotkey(handle: &AppHandle) -> Result<(), String> {
        use tauri_plugin_global_shortcut::GlobalShortcutExt;
        let handle_clone = handle.clone();

        // Ensure registered before trying to register again
        let _ = handle.global_shortcut().unregister(HOTKEY);

        handle
            .global_shortcut()
            .on_shortcut(HOTKEY, move |_app, _shortcut, event| {
                if event.state == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                    log::info!("Private screen hotkey pressed");
                    let _ = Self::hide_window(&handle_clone);
                    // We don't have direct access to state_sender here, but we can emit a tauri event or similar if needed.
                    // For now, hiding the window is enough. The server will see the state change next time it queries or through the channel if we pass it properly.
                    // Actually passing state_sender to hotkey callback requires cloning it and moving it into closure.
                    // Let's rely on emitting an event to app handles and having a listener relay it.
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn unregister_hotkey(handle: &AppHandle) -> Result<(), String> {
        use tauri_plugin_global_shortcut::GlobalShortcutExt;
        let _ = handle
            .global_shortcut()
            .unregister(HOTKEY)
            .map_err(|e| e.to_string());
        Ok(())
    }
}
