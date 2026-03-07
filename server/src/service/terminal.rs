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
