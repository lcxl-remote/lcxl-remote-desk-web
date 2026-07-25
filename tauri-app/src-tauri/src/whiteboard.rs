use desk_input_injection::model::host_control::WhiteboardCommand;
use std::sync::mpsc;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

const WHITEBOARD_WINDOW_LABEL: &str = "whiteboard";

/// Returns whether a whiteboard command from `from_connection_id` may act on the
/// overlay, given the connection that currently owns it (if any). The first
/// controller to `Show` the board claims ownership; commands from any other
/// connection are rejected until the owner releases it via `Hide`.
fn command_allowed(current_owner: Option<&str>, from_connection_id: &str) -> bool {
    current_owner.is_none_or(|owner| owner == from_connection_id)
}

pub struct WhiteboardManager {
    app_handle: AppHandle,
    frontend_url: String,
    controlled_by_connection_id: Option<String>,
}

impl WhiteboardManager {
    pub fn new(app_handle: AppHandle, frontend_url: String) -> Self {
        Self {
            app_handle,
            frontend_url,
            controlled_by_connection_id: None,
        }
    }

    pub fn start(mut self, cmd_receiver: mpsc::Receiver<WhiteboardCommand>) {
        let handle = self.app_handle.clone();
        std::thread::spawn(move || {
            loop {
                match cmd_receiver.recv() {
                    Ok(cmd) => match cmd {
                        WhiteboardCommand::Show(from_connection_id) => {
                            if !command_allowed(
                                self.controlled_by_connection_id.as_deref(),
                                &from_connection_id,
                            ) {
                                log::warn!(
                                    "Whiteboard is already controlled by another connection"
                                );
                                continue;
                            }
                            if let Err(e) = self.show_window(&handle) {
                                log::error!("Failed to show whiteboard window: {}", e);
                                continue;
                            }
                            log::info!("Whiteboard window shown");
                            self.controlled_by_connection_id = Some(from_connection_id);
                        }
                        WhiteboardCommand::DrawMessage(json_msg) => {
                            // Forward drawing message to the webview via evaluate_script to avoid IPC cross-origin block
                            log::info!(
                                "Forwarding drawing message to whiteboard window: {}",
                                json_msg
                            );
                            if let Some(window) = handle.get_webview_window(WHITEBOARD_WINDOW_LABEL)
                            {
                                // Serialize the json message safely for JavaScript evaluation
                                let safe_json = serde_json::to_string(&json_msg)
                                    .unwrap_or_else(|_| "\"\"".to_string());
                                let script = format!(
                                    "window.dispatchEvent(new CustomEvent('whiteboard-draw', {{ detail: {} }}));",
                                    safe_json
                                );
                                if let Err(e) = window.eval(&script) {
                                    log::error!("Failed to eval whiteboard draw event: {}", e);
                                }
                            }
                        }
                        WhiteboardCommand::Hide(from_connection_id) => {
                            if !command_allowed(
                                self.controlled_by_connection_id.as_deref(),
                                &from_connection_id,
                            ) {
                                log::warn!(
                                    "Whiteboard is already controlled by another connection"
                                );
                                continue;
                            }
                            if let Err(e) = Self::hide_window(&handle) {
                                log::error!("Failed to hide whiteboard window: {}", e);
                            }
                            log::info!("Whiteboard window hidden");
                            self.controlled_by_connection_id = None;
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
                WebviewUrl::External(
                    format!("{}/whiteboard?tauri=1", self.frontend_url)
                        .parse()
                        .unwrap(),
                ),
            )
            .title(rust_i18n::t!("whiteboard_title"))
            .transparent(true)
            .always_on_top(true)
            .decorations(false)
            .skip_taskbar(true)
            .resizable(false)
            .on_page_load(|window, event| {
                if let tauri::webview::PageLoadEvent::Finished = event.event() {
                    crate::inject_native_bridge_state(&window);
                }
            })
            .build()
            .map_err(|e| e.to_string())?
        };

        if let Ok(Some(monitor)) = window.primary_monitor() {
            let _ = window.set_size(*monitor.size());
            let _ = window.set_position(*monitor.position());
        }

        window.show().map_err(|e| e.to_string())?;
        crate::overlay_window::enter_overlay_fullscreen(&window)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unowned_overlay_accepts_any_connection() {
        // Before anyone claims the board, the first Show from any connection is
        // allowed (and goes on to claim ownership).
        assert!(command_allowed(None, "conn-a"));
        assert!(command_allowed(None, "conn-b"));
    }

    #[test]
    fn owner_can_keep_driving_its_overlay() {
        assert!(command_allowed(Some("conn-a"), "conn-a"));
    }

    #[test]
    fn other_connection_is_rejected_while_owned() {
        // A second connection cannot Show/Hide the board while another owns it.
        assert!(!command_allowed(Some("conn-a"), "conn-b"));
    }
}
