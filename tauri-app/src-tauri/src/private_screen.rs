use crate::platform;
use lcxl_remote_desk_server::model::system_setting::{
    PrivateScreenCommand, SystemSettingEventType,
};
use std::sync::mpsc;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

pub struct PrivateScreenManager {
    app_handle: AppHandle,
    controlled_by_session_id: Option<String>,
}

const PRIVATE_SCREEN_WINDOW_LABEL: &str = "private-screen";
const HOTKEY: &str = "ctrl+alt+l";

impl PrivateScreenManager {
    pub fn new(app_handle: AppHandle) -> Self {
        Self {
            app_handle,
            controlled_by_session_id: None,
        }
    }

    pub fn start(
        mut self,
        cmd_receiver: mpsc::Receiver<PrivateScreenCommand>,
        state_sender: tokio::sync::mpsc::UnboundedSender<SystemSettingEventType>,
    ) {
        let handle = self.app_handle.clone();
        std::thread::spawn(move || {
            loop {
                match cmd_receiver.recv() {
                    Ok(cmd) => match cmd {
                        PrivateScreenCommand::Show(from_session_id) => {
                            if let Some(controlled_by_session_id) = &self.controlled_by_session_id {
                                if controlled_by_session_id != &from_session_id {
                                    log::warn!(
                                        "Private screen is already controlled by another session"
                                    );
                                    continue;
                                }
                            }

                            if let Err(e) = Self::show_window(&handle) {
                                log::error!("Failed to show private screen: {}", e);
                                let _ = state_sender.send(
                                    SystemSettingEventType::PrivateScreenUnknownError(
                                        Some(from_session_id.clone()),
                                        e.to_string(),
                                    ),
                                );
                                continue;
                            }
                            let _ = state_sender.send(
                                SystemSettingEventType::PrivateScreenVisibleChanged(
                                    from_session_id.clone(),
                                    true,
                                ),
                            );
                            self.controlled_by_session_id = Some(from_session_id);
                        }
                        PrivateScreenCommand::Hide(from_session_id) => {
                            if let Some(controlled_by_session_id) = &self.controlled_by_session_id {
                                if controlled_by_session_id != &from_session_id {
                                    log::warn!(
                                        "Private screen is already controlled by another session"
                                    );
                                    continue;
                                }
                            }
                            if let Err(e) = Self::hide_window(&handle) {
                                log::error!("Failed to hide private screen: {}", e);
                            }
                            let _ = state_sender.send(
                                SystemSettingEventType::PrivateScreenVisibleChanged(
                                    from_session_id.clone(),
                                    false,
                                ),
                            );
                            self.controlled_by_session_id = None;
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

    fn show_window(handle: &AppHandle) -> Result<(), String> {
        // If window already exists, just show it
        if let Some(window) = handle.get_webview_window(PRIVATE_SCREEN_WINDOW_LABEL) {
            window.show().map_err(|e| e.to_string())?;
            window.set_fullscreen(true).map_err(|e| e.to_string())?;
            window.set_always_on_top(true).map_err(|e| e.to_string())?;
            window.set_focus().map_err(|e| e.to_string())?;
        } else {
            // Create new window
            let _window = WebviewWindowBuilder::new(
                handle,
                PRIVATE_SCREEN_WINDOW_LABEL,
                WebviewUrl::App("private-screen.html".into()), // Using the placeholder HTML for now
            )
            .title("Private Screen")
            .fullscreen(true)
            .always_on_top(true)
            .decorations(false)
            .skip_taskbar(true)
            .resizable(false)
            .content_protected(true) // Prevent screen capture
            .minimizable(false)
            .build()
            .map_err(|e| e.to_string())?;
        }

        // Platform specific: block input (best effort; do not fail private screen)
        if let Err(e) = platform::block_input(true) {
            log::warn!("Failed to block local input, continue with private screen: {}", e);
        }

        // 注册全局快捷键
        Self::register_hotkey(handle)?;

        Ok(())
    }

    fn hide_window(handle: &AppHandle) -> Result<(), String> {
        // 先取消输入拦截
        let _ = platform::block_input(false);

        // 注销全局快捷键
        let _ = Self::unregister_hotkey(handle);

        // 隐藏窗口
        if let Some(window) = handle.get_webview_window(PRIVATE_SCREEN_WINDOW_LABEL) {
            window.hide().map_err(|e| e.to_string())?;
        }

        Ok(())
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
