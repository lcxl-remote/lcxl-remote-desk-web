use std::io::{Read, Write};

use actix_web::web;
use actix_ws::{AggregatedMessage, AggregatedMessageStream, Session};
use desk_server_user::model::CurrentUser;
use desk_utils::error::DeskErrorCode;
use futures::StreamExt;
use portable_pty::{MasterPty, Child, PtySize};
use regex::Regex;
use tokio::sync::mpsc;
use serde::Deserialize;

use crate::model::terminal::TerminalList;
use crate::{error::DeskError, model::settings::SharedSettings};

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum TerminalMessage {
    Data { content: String },
    Resize { rows: u16, cols: u16 },
}

/// Inner function to fetch terminal list based on provided shell names and regex patterns
pub async fn inner_fetch_terminal_list(
    settings: web::Data<SharedSettings>,
    shell_list: &[&str],
    shell_regexe_list: &[&str],
) -> Result<TerminalList, DeskError> {
    let mut terminal_list = Vec::<Vec<String>>::new();

    for shell in shell_list {
        if let Ok(path) = which::which(*shell) {
            terminal_list.push(vec![path.to_string_lossy().into_owned()]);
        }
    }

    for regex in shell_regexe_list {
        if let Ok(paths) = which::which_re(Regex::new(*regex)?) {
            for path in paths {
                terminal_list.push(vec![path.to_string_lossy().into_owned()]);
            }
        }
    }

    let mut current = 0;
    let settings = &settings.read().await.terminal;
    if let Some(ref current_terminal) = settings.current_terminal {
        log::info!(
            "Default terminal command from settings: {:?}",
            current_terminal
        );
        // find the index of the default command in the terminal list
        for index in 0..terminal_list.len() {
            if terminal_list[index] == *current_terminal {
                log::info!(
                    "Found default terminal command: {:?} at index {}",
                    current_terminal,
                    index
                );
                current = index;
                break;
            }
        }
    }

    return Ok(TerminalList {
        commands: terminal_list,
        current,
    });
}

/// Fetches the list of available terminals on a Windows
/// see alse: https://github.com/microsoft/vscode/blob/main/src/vs/platform/terminal/node/windowsShellHelper.ts
#[cfg(target_os = "windows")]
pub async fn fetch_terminal_list(
    settings: web::Data<SharedSettings>,
) -> Result<TerminalList, DeskError> {
    let shell_list = [
        "cmd",
        "powershell",
        "bash",
        "wsl",
        "WindowsTerminal",
        "node",
        "julia",
    ];
    let shell_regexe_list = [r"python(\d(\.\d{0,2})?)?\.exe"];
    inner_fetch_terminal_list(settings, &shell_list, &shell_regexe_list).await
}

#[cfg(not(target_os = "windows"))]
pub async fn fetch_terminal_list(
    settings: web::Data<SharedSettings>,
) -> Result<TerminalList, DeskError> {
    let shell_list = ["bash", "csh", "fish", "ksh", "sh", "zsh"];
    let shell_regexe_list = [r"python(\d(\.\d{0,2})?)?"];
    inner_fetch_terminal_list(settings, &shell_list, &shell_regexe_list).await
}

pub async fn handle_terminal(
    _settings: web::Data<SharedSettings>,
    mut stream: AggregatedMessageStream,
    mut session: Session,
    _user: CurrentUser,
    master_pty: Box<dyn MasterPty + Send>,
    mut child: Box<dyn Child + Send + Sync>,
) -> Result<(), DeskError> {
    log::info!("Handling terminal session");

    // Since portable-pty reads are blocking, we need to spawn a blocking task to read from pty
    // and send to a channel that the main async loop can read from.
    let mut reader = master_pty.try_clone_reader().map_err(|e| {
        let err = DeskError::custom_error::<()>(DeskErrorCode::SYSTEM_ERROR, format!("Failed to clone pty reader: {}", e));
        match err {
            Err(e) => e,
            Ok(_) => unreachable!(),
        }
    })?;
    
    let mut writer = master_pty.take_writer().map_err(|e| {
         let err = DeskError::custom_error::<()>(DeskErrorCode::SYSTEM_ERROR, format!("Failed to take pty writer: {}", e));
         match err {
             Err(e) => e,
             Ok(_) => unreachable!(),
         }
    })?;


    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(32);

    // Spawn a blocking thread for reading from PTY
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(n) => {
                    if n == 0 {
                        break;
                    }
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    log::error!("Failed to read from pty: {}", e);
                    break;
                }
            }
        }
    });

    loop {
        tokio::select! {
            // Receive data from PTY (via channel) and send to WebSocket
            Some(data) = rx.recv() => {
                let text = String::from_utf8_lossy(&data);
                // Convert Cow to String to satisfy Into<ByteString>
                if let Err(e) = session.text(text.to_string()).await {
                     log::error!("Failed to send to websocket: {}", e);
                     break;
                }
            },
            
            // Receive data from WebSocket and write to PTY
            result = stream.next() => {
                let msg = match result {
                    Some(Ok(msg)) => msg,
                    Some(Err(e)) => {
                        log::error!("WS error: {}", e);
                        break;
                    }
                    None => {
                        log::info!("Stream closed");
                        break;
                    }
                };

                match msg {
                    AggregatedMessage::Text(text) => {
                        log::debug!("Received text content from websocket: {:?}", text);
                        
                        // Try to parse as JSON protocol
                        match serde_json::from_str::<TerminalMessage>(&text) {
                            Ok(TerminalMessage::Data { content }) => {
                                if let Err(e) = writer.write_all(content.as_bytes()) {
                                     log::error!("Failed to write to pty: {}", e);
                                     break;
                                }
                            },
                            Ok(TerminalMessage::Resize { rows, cols }) => {
                                if let Err(e) = master_pty.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }) {
                                    log::error!("Failed to resize pty: {}", e);
                                    // Non-fatal error
                                } else {
                                    log::debug!("Resized pty to {}x{}", rows, cols);
                                }
                            },
                            Err(_) => {
                                // Fallback: Treat as raw input if not valid JSON (compatibility)
                                log::warn!("Received non-JSON message, treating as raw input");
                                if let Err(e) = writer.write_all(text.as_bytes()) {
                                     log::error!("Failed to write to pty: {}", e);
                                     break;
                                }
                            }
                        }
                    }
                    AggregatedMessage::Binary(bin) => {
                        log::debug!("Received binary content from websocket: {:?}", bin);
                         if let Err(e) = writer.write_all(&bin) {
                             log::error!("Failed to write to pty: {}", e);
                             break;
                        }
                    }
                    AggregatedMessage::Ping(msg) => {
                        if let Err(e) = session.pong(&msg).await {
                             log::error!("Failed to send pong: {}", e);
                             break;
                        }
                    }
                    AggregatedMessage::Pong(_) => {}
                    AggregatedMessage::Close(reason) => {
                        log::warn!("WS close frame received: {:?}", reason);
                        break;
                    }
                }
            }
        }
    }
    
    // cleanup
    let _ = child.kill();
    
    Ok(())
}

#[cfg(test)]
mod tests {

    use crate::model::settings::Settings;

    use super::*;

    #[tokio::test]
    async fn test_fetch_terminal_list() -> Result<(), DeskError> {
        let settings = web::Data::new(SharedSettings::from(Settings::default()));
        let result = fetch_terminal_list(settings).await?;
        println!("Terminal list: {:?}", result);
        assert!(!result.commands.is_empty()); // Ensure that the terminal list is not empty
        Ok(())
    }
}