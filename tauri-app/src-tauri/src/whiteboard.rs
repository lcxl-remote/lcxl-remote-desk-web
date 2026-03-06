use lcxl_remote_desk_server::model::system_setting::WhiteboardCommand;
use std::sync::mpsc;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

const WHITEBOARD_WINDOW_LABEL: &str = "whiteboard";

pub struct WhiteboardManager {
    app_handle: AppHandle,
    frontend_url: String,
    controlled_by_session_id: Option<String>,
}

impl WhiteboardManager {
    pub fn new(app_handle: AppHandle, frontend_url: String) -> Self {
        Self {
            app_handle,
            frontend_url,
            controlled_by_session_id: None,
        }
    }

    pub fn start(mut self, cmd_receiver: mpsc::Receiver<WhiteboardCommand>) {
        let handle = self.app_handle.clone();
        std::thread::spawn(move || {
            loop {
                match cmd_receiver.recv() {
                    Ok(cmd) => match cmd {
                        WhiteboardCommand::Show(from_session_id) => {
                            if let Some(ref controlled) = self.controlled_by_session_id {
                                if controlled != &from_session_id {
                                    log::warn!(
                                        "Whiteboard is already controlled by another session"
                                    );
                                    continue;
                                }
                            }
                            if let Err(e) = self.show_window(&handle) {
                                log::error!("Failed to show whiteboard window: {}", e);
                                continue;
                            }
                            self.controlled_by_session_id = Some(from_session_id);
                        }
                        WhiteboardCommand::DrawMessage(json_msg) => {
                            // Forward drawing message to the webview via Tauri event
                            if let Err(e) = handle.emit("whiteboard-draw", &json_msg) {
                                log::error!("Failed to emit whiteboard draw event: {}", e);
                            }
                        }
                        WhiteboardCommand::Hide(from_session_id) => {
                            if let Some(ref controlled) = self.controlled_by_session_id {
                                if controlled != &from_session_id {
                                    log::warn!(
                                        "Whiteboard is already controlled by another session"
                                    );
                                    continue;
                                }
                            }
                            if let Err(e) = Self::hide_window(&handle) {
                                log::error!("Failed to hide whiteboard window: {}", e);
                            }
                            self.controlled_by_session_id = None;
                        }
                        WhiteboardCommand::Quit => {
                            let _ = Self::hide_window(&handle);
                            log::info!("Whiteboard quit");
                            break;
                        }
                    },
                    Err(_) => {
                        log::warn!("Whiteboard command channel closed");
                        break;
                    }
                }
            }
            log::info!("Whiteboard loop exited");
        });
    }

    fn show_window(&self, handle: &AppHandle) -> Result<(), String> {
        let window = if let Some(window) = handle.get_webview_window(WHITEBOARD_WINDOW_LABEL) {
            window
        } else {
            // Create new transparent overlay window
            WebviewWindowBuilder::new(
                handle,
                WHITEBOARD_WINDOW_LABEL,
                WebviewUrl::External(format!("{}/whiteboard", self.frontend_url).parse().unwrap()),
            )
            .title("Whiteboard")
            .transparent(true)
            .always_on_top(true)
            .decorations(false)
            .skip_taskbar(true)
            .resizable(false)
            .build()
            .map_err(|e| e.to_string())?
        };

        if let Ok(Some(monitor)) = window.primary_monitor() {
            let _ = window.set_size(monitor.size().clone());
            let _ = window.set_position(monitor.position().clone());
        }

        window.show().map_err(|e| e.to_string())?;
        window.set_fullscreen(true).map_err(|e| e.to_string())?;
        window.set_always_on_top(true).map_err(|e| e.to_string())?;
        // Mouse events pass through to the desktop beneath
        let _ = window.set_ignore_cursor_events(true);

        Ok(())
    }

    fn hide_window(handle: &AppHandle) -> Result<(), String> {
        if let Some(window) = handle.get_webview_window(WHITEBOARD_WINDOW_LABEL) {
            window.close().map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}
