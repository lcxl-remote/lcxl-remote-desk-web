//! Whether this process runs a TURN service, and why.
//!
//! Two independent conditions decide it: the startup mode has to be one that
//! hosts signaling at all, and the operator has to have left the service
//! enabled. Keeping them apart matters for what the operator sees — a host that
//! never serves TURN is not the same as one whose TURN was switched off, and a
//! switched-off service is not a failure to report.

use desk_turn::model::TurnSettings;

use crate::model::settings::StartupMode;

/// The decision, with the reason attached so callers can log it at the right
/// level and, later, tell the two "not running" cases apart in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStartup {
    /// Start the service.
    Start,
    /// The operator turned the TURN service off in settings.
    DisabledBySettings,
    /// This startup mode never hosts a TURN service, whatever the settings say.
    UnsupportedMode,
}

/// Decide whether to run a TURN service in `mode` under `settings`.
///
/// `UnsupportedMode` wins over `DisabledBySettings`: a desk server or a worker
/// would not have served TURN even with the switch on, so reporting it as
/// "switched off" would invite an operator to look for a switch that changes
/// nothing.
pub fn turn_startup(mode: &StartupMode, settings: &TurnSettings) -> TurnStartup {
    if !mode_hosts_turn(mode) {
        return TurnStartup::UnsupportedMode;
    }
    if !settings.enable_turn {
        return TurnStartup::DisabledBySettings;
    }
    TurnStartup::Start
}

/// Whether a startup mode runs an embedded signaling server, which is what the
/// TURN service belongs to.
fn mode_hosts_turn(mode: &StartupMode) -> bool {
    match mode {
        StartupMode::Default | StartupMode::Signaling => true,
        StartupMode::DeskServer
        | StartupMode::ServiceDaemon
        | StartupMode::SessionWorker
        | StartupMode::McpStdio => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(enable_turn: bool) -> TurnSettings {
        TurnSettings {
            enable_turn,
            ..TurnSettings::default()
        }
    }

    /// The switch only has an answer to give where TURN could run at all, and
    /// where it can, it decides.
    #[test]
    fn every_mode_and_switch_combination_has_a_stable_answer() {
        for mode in [
            StartupMode::Default,
            StartupMode::Signaling,
            StartupMode::DeskServer,
            StartupMode::ServiceDaemon,
            StartupMode::SessionWorker,
            StartupMode::McpStdio,
        ] {
            let hosts = matches!(mode, StartupMode::Default | StartupMode::Signaling);
            let expected_on = if hosts {
                TurnStartup::Start
            } else {
                TurnStartup::UnsupportedMode
            };
            let expected_off = if hosts {
                TurnStartup::DisabledBySettings
            } else {
                TurnStartup::UnsupportedMode
            };
            assert_eq!(
                turn_startup(&mode, &settings(true)),
                expected_on,
                "{mode:?}"
            );
            assert_eq!(
                turn_startup(&mode, &settings(false)),
                expected_off,
                "{mode:?}",
            );
        }
    }

    /// The default settings must run TURN: this is the switch's first release
    /// where it is read at all, and a host that configured interfaces expects
    /// them to be served without hunting for a toggle.
    #[test]
    fn a_default_configuration_runs_turn() {
        assert_eq!(
            turn_startup(&StartupMode::Default, &TurnSettings::default()),
            TurnStartup::Start
        );
    }
}
