use serde::{Deserialize, Serialize};
use strum_macros::AsRefStr;
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

/// The form a server process runs as: which subsystems it started, and
/// therefore what a peer can expect of it. A server reports its own mode, and a
/// host reports its mode to control ends, which is how one tells what the
/// *target* machine is rather than what the server in between happens to be.
// This doc is published into the OpenAPI schema, so it stays short and says only
// what a client needs; the reason the enum lives here is for Rust readers.
//
// Both the manager and the desk server answer with this document, and they may
// not depend on each other, so a mode declared in either one would be a second
// list free to drift from the first. Declaring it in the model they share means
// a control end reads one set of spellings no matter which it is talking to, and
// [`SystemInfo`](super::system_info::SystemInfo) can carry a host's own mode
// across the same seam.
#[derive(
    Clone,
    Default,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    AsRefStr,
    ToSchema,
    SchemaWrite,
    SchemaRead,
)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum StartupMode {
    /// Default mode, includes both signaling server and desk server (Portable)
    #[default]
    Default,
    /// Signaling mode, include signaling server and turn server
    Signaling,
    /// Desk Server only
    DeskServer,
    /// System service daemon (SYSTEM / root) - manages Worker lifecycle
    ServiceDaemon,
    /// Session worker process - launched by ServiceDaemon in target desktop
    SessionWorker,
    /// Read-only MCP server over stdio (local AI assistant integration). stdin /
    /// stdout carry the MCP JSON-RPC framing, so this mode must never log to
    /// stdout (see `is_headless_startup_mode`).
    McpStdio,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every mode and the exact name it takes on the wire. Control ends compare
    /// against these spellings, so each one is a contract; the list is also the
    /// place a seventh mode has to be added, which is why it enumerates rather
    /// than deriving from the type.
    const WIRE_NAMES: &[(StartupMode, &str)] = &[
        (StartupMode::Default, "default"),
        (StartupMode::Signaling, "signaling"),
        (StartupMode::DeskServer, "desk-server"),
        (StartupMode::ServiceDaemon, "service-daemon"),
        (StartupMode::SessionWorker, "session-worker"),
        (StartupMode::McpStdio, "mcp-stdio"),
    ];

    #[test]
    fn every_mode_serializes_as_its_kebab_case_name() {
        for (mode, expected) in WIRE_NAMES {
            assert_eq!(
                serde_json::to_value(mode).unwrap(),
                serde_json::Value::String((*expected).to_string()),
                "{mode:?} must stay on the wire as {expected}",
            );
            // `as_ref` feeds log lines and CLI help; a spelling that disagreed
            // with the wire would make the two describe different modes.
            assert_eq!(mode.as_ref(), *expected);
        }
    }

    #[test]
    fn every_wire_name_deserializes_back_to_its_mode() {
        for (mode, name) in WIRE_NAMES {
            let json = serde_json::Value::String((*name).to_string());
            let back: StartupMode = serde_json::from_value(json).unwrap();
            assert_eq!(back, *mode);
        }
    }

    /// The mode rides the IPC framing inside `SystemInfo`, so it has to survive
    /// wincode as well as JSON. Round-trip each variant: a derive that collapsed
    /// them would still pass a single-variant check.
    #[test]
    fn every_mode_round_trips_wincode() {
        use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};

        let config: Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED> = Configuration::new();
        for (mode, _) in WIRE_NAMES {
            let bytes = wincode::config::serialize(mode, config).expect("encode");
            let back: StartupMode = wincode::config::deserialize(&bytes, config).expect("decode");
            assert_eq!(back, *mode);
        }
    }

    /// The default is the portable mode — the one a server started with no
    /// arguments actually runs in.
    #[test]
    fn the_default_mode_is_portable() {
        assert_eq!(StartupMode::default(), StartupMode::Default);
    }
}
