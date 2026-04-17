use crate::model::security_approval::{SecurityPermissionType, check_security_permission};
use crate::service::signaling::{DeskSession, DeskSessionMessage};
use bytestring::ByteString;
use desk_signal_facade::model::signal::{PeerSignalingSender, SignalingModel, SignalingType};
use desk_signal_facade::model::terminal::{
    StartTerminalSession, TerminalInputData, TerminalOutputData, TerminalResizeData,
};
use desk_utils::error::DeskErrorCode;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use actix_web::web;
use desk_signal_facade::model::terminal::TerminalList;
use regex::Regex;

use crate::{error::DeskError, model::settings::SharedSettings};

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
        "pwsh",
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
    let shell_list = ["bash", "csh", "fish", "ksh", "sh", "zsh", "pwsh"];
    let shell_regexe_list = [r"python(\d(\.\d{0,2})?)?"];
    inner_fetch_terminal_list(settings, &shell_list, &shell_regexe_list).await
}

/// Running terminal model
pub struct RunningTerminal {
    pub master: Box<dyn MasterPty + Send>,
    pub child: Arc<std::sync::Mutex<Box<dyn Child + Send + Sync>>>,
    pub writer: Box<dyn Write + Send>,
}

/// Helper function to kill a terminal process and all its descendants grouped by OS session ID.
pub fn force_kill_terminal_process(root_pid: u32) {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let target_sid = sysinfo::Pid::from_u32(root_pid);
    for (pid, proc) in sys.processes() {
        if proc.session_id() == Some(target_sid) {
            let result = proc.kill();
            log::info!(
                "Try to kill process, os_session_id={}, pid={}, result={}",
                target_sid.as_u32(),
                pid.as_u32(),
                result
            );
        }
    }
}

pub async fn handle_manager_terminal_start(
    desk_session: &mut DeskSession,
    signaling_model: &SignalingModel,
) -> Result<(), DeskError> {
    let from_connection_id = signaling_model.check_and_get_from_connection_id()?;

    let allow_terminal = { desk_session.settings.read().await.security.allow_terminal };
    let approved = check_security_permission(
        &desk_session.settings,
        desk_session.security_approval_sender.as_ref(),
        allow_terminal,
        SecurityPermissionType::Terminal,
        Some(from_connection_id.to_string()),
    )
    .await;

    if !approved {
        desk_session
            .session
            .send_error(
                &signaling_model.request_id,
                signaling_model.signaling_type.into(),
                Some(from_connection_id.to_string()),
                DeskErrorCode::PERMISSION_ERROR,
                "Terminal access denied by security settings or user",
            )
            .await?;
        return Ok(());
    }

    // The from_connection_id IS the terminal_connection_id generated by the controller.
    let start_terminal_session = signaling_model.get_data::<StartTerminalSession>()?;
    let command = start_terminal_session.command;
    if command.is_empty() {
        return DeskError::custom_error(DeskErrorCode::INVALID_PARAMS, "Missing command");
    }

    let terminal_command_list: Vec<&str> = command.split(",").collect();
    let execute_file_path = terminal_command_list[0];
    let args_list = &terminal_command_list[1..];

    // PTY setup
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(execute_file_path);
    cmd.args(args_list);
    if cfg!(unix) {
        cmd.env("TERM", "xterm-256color");
    }

    let child = pair.slave.spawn_command(cmd)?;

    // Wrap child in Arc<Mutex> for shared access
    let child = Arc::new(std::sync::Mutex::new(child));
    let child_clone = child.clone();

    // Spawn reader
    let mut reader = pair.master.try_clone_reader()?;
    let session_sender = desk_session.session.clone();
    let terminal_connection_id = from_connection_id.to_owned();

    // Monitor task for process exit (using tokio::spawn for coroutine)
    let monitor_sender = desk_session.session.clone();
    let monitor_connection_id = from_connection_id.to_owned();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let exited = {
                if let Ok(mut child) = child_clone.lock() {
                    match child.try_wait() {
                        Ok(Some(_)) => true,
                        Ok(None) => false,
                        Err(e) => {
                            log::warn!("Failed to wait child: {}", e);
                            true // Assume exited on error
                        }
                    }
                } else {
                    true // Poisioned mutex
                }
            };

            if exited {
                log::info!(
                    "Process exited, sending TerminalClosed to {}",
                    monitor_connection_id
                );
                let model = SignalingModel::new_request::<()>(
                    SignalingType::TerminalClosed,
                    Some(monitor_connection_id.to_string()),
                    None,
                );
                if let Ok(model) = model {
                    if let Ok(text) = serde_json::to_string(&model) {
                        if let Err(e) = monitor_sender
                            .sender
                            .send(DeskSessionMessage::Text(ByteString::from(text)))
                        {
                            log::warn!(
                                "Failed to send TerminalClosed to {}: {}",
                                monitor_connection_id,
                                e
                            );
                        }
                    }
                }
                break;
            }
        }
    });

    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(n) => {
                    if n == 0 {
                        break;
                    }
                    let content = String::from_utf8_lossy(&buf[..n]).to_string();
                    let data = TerminalOutputData { content };
                    let model = SignalingModel::new_request(
                        SignalingType::ReplyFromTerminal,
                        Some(terminal_connection_id.to_owned()),
                        Some(&data),
                    );
                    if let Ok(model) = model {
                        if let Ok(text) = serde_json::to_string(&model) {
                            let _ = session_sender
                                .sender
                                .send(DeskSessionMessage::Text(ByteString::from(text)));
                        }
                    }
                }
                Err(e) => {
                    log::warn!(
                        "Failed to read from terminal, connection id: {}, error: {}",
                        terminal_connection_id,
                        e
                    );
                    break;
                }
            }
        }
        // Send close message
        let data = TerminalOutputData {
            content: "\r\n\x1b[33m[Process exited]\x1b[0m\r\n".to_string(),
        };

        let model = SignalingModel::new_request(
            SignalingType::ReplyFromTerminal,
            Some(terminal_connection_id.to_owned()),
            Some(&data),
        );

        if let Ok(model) = model {
            if let Ok(text) = serde_json::to_string(&model) {
                let _ = session_sender
                    .sender
                    .send(DeskSessionMessage::Text(ByteString::from(text)));
            }
        }
    });

    let writer = pair.master.take_writer()?;

    desk_session.terminal_map.insert(
        from_connection_id.to_owned(),
        RunningTerminal {
            master: pair.master,
            child,
            writer,
        },
    );

    // send terminal started signal
    let model = SignalingModel::success_response::<()>(
        &signaling_model.request_id,
        SignalingType::TerminalStarted,
        None,
        Some(from_connection_id.to_owned()),
        None,
    );
    if let Ok(model) = model {
        if let Ok(text) = serde_json::to_string(&model) {
            let _ = desk_session
                .session
                .sender
                .send(DeskSessionMessage::Text(ByteString::from(text)));
        }
    }
    Ok(())
}

pub async fn handle_manager_terminal_data(
    desk_session: &mut DeskSession,
    signaling_model: &SignalingModel,
) -> Result<(), DeskError> {
    let from_connection_id = signaling_model.check_and_get_from_connection_id()?;
    let data_value = if let Some(v) = signaling_model.get_data_with_type::<TerminalInputData>()? {
        v
    } else {
        return Ok(()); // Ignore empty
    };

    if let Some(terminal) = desk_session.terminal_map.get_mut(from_connection_id) {
        let writer = &mut terminal.writer;
        if let Err(e) = writer.write_all(data_value.content.as_bytes()) {
            log::warn!("Failed to write to pty: {}", e);
        }
    }
    Ok(())
}

pub async fn handle_manager_terminal_resize(
    desk_session: &mut DeskSession,
    signaling_model: &SignalingModel,
) -> Result<(), DeskError> {
    let from_connection_id = signaling_model.check_and_get_from_connection_id()?;
    let data_value = if let Some(v) = signaling_model.get_data_with_type::<TerminalResizeData>()? {
        v
    } else {
        return Ok(());
    };

    if let Some(terminal) = desk_session.terminal_map.get_mut(from_connection_id) {
        let rows = data_value.rows;
        let cols = data_value.cols;
        if let Err(e) = terminal.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            log::warn!("Failed to resize pty: {}", e);
        }
    }
    Ok(())
}

pub async fn handle_manager_terminal_close(
    desk_session: &mut DeskSession,
    signaling_model: &SignalingModel,
) -> Result<(), DeskError> {
    log::info!(
        "handle_manager_terminal_close, signaling_model: {:?}",
        signaling_model
    );
    let from_connection_id = signaling_model.check_and_get_from_connection_id()?;
    if let Some(terminal) = desk_session.terminal_map.remove(from_connection_id) {
        let child_arc = terminal.child.clone();
        drop(terminal);
        if let Ok(mut child) = child_arc.lock() {
            if let Some(pid) = child.process_id() {
                force_kill_terminal_process(pid);
            }
            let _ = child.kill();
        }
    }
    Ok(())
}

pub async fn handle_list_terminals(
    desk_session: &mut DeskSession,
    signaling_model: &SignalingModel,
) -> Result<(), DeskError> {
    let from_connection_id = signaling_model.from_connection_id.clone();
    let terminals = fetch_terminal_list(desk_session.settings.clone()).await?;
    desk_session
        .session
        .send_response(
            &signaling_model.request_id,
            signaling_model.signaling_type.into(),
            from_connection_id,
            &terminals,
        )
        .await?;
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
