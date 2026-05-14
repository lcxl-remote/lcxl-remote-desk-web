use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

#[derive(
    Debug, Clone, Serialize, Deserialize, IntoParams, wincode::SchemaWrite, wincode::SchemaRead,
)]
pub struct StartTerminalSession {
    /// The command to start the terminal session. with the format of "path/to/executable,arg1,arg2"
    pub command: String,
}

/// Terminal list
#[derive(
    Debug, Clone, Serialize, Deserialize, ToSchema, wincode::SchemaWrite, wincode::SchemaRead,
)]
pub struct TerminalList {
    /// terminal command list
    pub commands: Vec<Vec<String>>,

    /// current terminal index
    pub current: usize,
}

/// Terminal settings
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
#[derive(Default)]
pub struct TerminalSettings {
    pub current_terminal: Option<Vec<String>>,
}

/// List terminal query path
#[derive(Clone, Debug, Deserialize, Serialize, IntoParams, ToSchema)]
pub struct ListTerminalPath {
    /// connection id
    pub connection_id: String,
}

/// Start terminal query path
#[derive(Clone, Debug, Deserialize, Serialize, IntoParams, ToSchema)]
pub struct StartTerminalPath {
    /// connection id
    pub connection_id: String,
}

/// SignalingType::SendDataToTerminal
#[derive(
    Debug, Clone, Serialize, Deserialize, ToSchema, wincode::SchemaWrite, wincode::SchemaRead,
)]
pub struct TerminalInputData {
    pub content: String,
}

#[derive(
    Debug, Clone, Serialize, Deserialize, ToSchema, wincode::SchemaWrite, wincode::SchemaRead,
)]
pub struct TerminalOutputData {
    pub content: String,
}

#[derive(
    Debug, Clone, Serialize, Deserialize, ToSchema, wincode::SchemaWrite, wincode::SchemaRead,
)]
pub struct TerminalResizeData {
    pub rows: u16,
    pub cols: u16,
}

#[cfg(test)]
mod wincode_tests {
    use super::*;
    use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};

    fn unbounded_config() -> Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED> {
        Configuration::new()
    }

    #[test]
    fn start_terminal_session_round_trips_wincode() {
        let original = StartTerminalSession {
            command: r"C:\Windows\System32\cmd.exe,/k,echo,hello".to_string(),
        };
        let config = unbounded_config();
        let bytes = wincode::config::serialize(&original, config).expect("encode");
        let back: StartTerminalSession =
            wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(back.command, original.command);
    }

    #[test]
    fn terminal_list_round_trips_wincode() {
        let original = TerminalList {
            commands: vec![
                vec![r"C:\Windows\System32\cmd.exe".to_string()],
                vec![r"C:\Program Files\PowerShell\7\pwsh.exe".to_string()],
            ],
            current: 1,
        };
        let config = unbounded_config();
        let bytes = wincode::config::serialize(&original, config).expect("encode");
        let back: TerminalList = wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(back.commands.len(), 2);
        assert_eq!(back.commands[0][0], r"C:\Windows\System32\cmd.exe");
        assert_eq!(back.current, 1);
    }

    /// PTY content frequently carries escape codes — verify they
    /// survive wincode round-trip verbatim. A regression here would
    /// silently corrupt terminal output in PR-2's `ReplyFromTerminal`
    /// IPC payload.
    #[test]
    fn terminal_input_data_preserves_escape_sequences() {
        let original = TerminalInputData {
            content: "ls -la\n\x1b[1;31mred\x1b[0m\n".to_string(),
        };
        let config = unbounded_config();
        let bytes = wincode::config::serialize(&original, config).expect("encode");
        let back: TerminalInputData = wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(back.content, original.content);
    }

    #[test]
    fn terminal_resize_data_preserves_field_order() {
        // 50 rows × 200 cols — picked so that swapped fields would
        // surface as obviously incorrect dimensions.
        let original = TerminalResizeData {
            rows: 50,
            cols: 200,
        };
        let config = unbounded_config();
        let bytes = wincode::config::serialize(&original, config).expect("encode");
        let back: TerminalResizeData =
            wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(back.rows, 50);
        assert_eq!(back.cols, 200);
    }
}
