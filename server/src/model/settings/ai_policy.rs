//! Edge-local AI execution policy.
//!
//! AI model provider configuration (credentials, base URL, model name, output
//! format) lives on the **central signaling brain**, not on this host. The edge
//! keeps only [`AiExecutionPolicy`]: the local ceiling on how far a centrally
//! authorized AI action may go on this device. It is a top-level field of
//! [`crate::model::settings::Settings`] and carries no secret, so there is no
//! redaction boundary to maintain here (unlike the credentials it replaced).
//!
//! The mode is an upper bound: a central authorization's mode is narrowed by the
//! local one via `restrict_to`, never widened (see the confirm-execution router).

use desk_agent_protocol::ExecutionMode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Whether an [`ExecutionMode`] is one of the three the confirm-execute flow
/// supports. `SessionApproved` / `Automated` are frozen in the protocol enum but
/// not selectable yet (they need a future policy engine); persisting them is
/// rejected so the stored mode stays in the usable set.
fn is_m2_selectable(mode: ExecutionMode) -> bool {
    matches!(
        mode,
        ExecutionMode::SuggestOnly | ExecutionMode::ReadOnly | ExecutionMode::ConfirmEachAction
    )
}

/// Default ceiling on commands running at once on this device. Low on purpose:
/// AI-driven execution is interactive and approved one command at a time, so
/// several at once is already unusual, and the ceiling exists to bound a caller
/// that ignores that rather than to size normal work.
pub const DEFAULT_MAX_CONCURRENT_EXECUTIONS: u32 = 4;

/// Bounds accepted from the settings UI. At least one, or the device could never
/// run anything and the setting would become an obscure way to disable execution
/// entirely — refusing execution is what the execution mode is for. The upper
/// bound is a sanity rail, not a capability claim: the point of the ceiling is to
/// stay well under what the machine can take.
pub const MIN_MAX_CONCURRENT_EXECUTIONS: u32 = 1;
pub const MAX_MAX_CONCURRENT_EXECUTIONS: u32 = 64;

/// Default and accepted range for one AI command's total wall time. Eight
/// seconds only controls when an agent stops waiting in the foreground; this
/// separate ceiling bounds the worker process after it becomes a background
/// task. The two-hour maximum matches the protocol's absolute fail-safe.
pub const DEFAULT_MAX_COMMAND_RUNTIME_SECONDS: u32 = 10 * 60;
pub const MIN_MAX_COMMAND_RUNTIME_SECONDS: u32 = 10;
pub const MAX_MAX_COMMAND_RUNTIME_SECONDS: u32 =
    desk_agent_protocol::exec_policy::PRODUCT_MAX_BACKGROUND_MS / 1_000;

/// Persisted edge-local AI execution policy.
///
/// Holds no model credentials (those live on the central brain); the fields are
/// the local execution ceiling and how much may run at once.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct AiExecutionPolicy {
    /// How far the AI may go in acting on the device. Default `suggest_only`
    /// (the AI only proposes commands). `read_only` permits only known read-only
    /// templates. `confirm_each_action` permits confirmed execution, including a
    /// trusted owner's blocklist-only command; every such command needs explicit
    /// approval. `session_approved` additionally lets the first approval of a
    /// template stand for the rest of the connection, but owner free-form remains
    /// per-command. `automated` is not implemented and is refused. On a central
    /// link this caps the centrally granted mode; it never widens it.
    pub execution_mode: ExecutionMode,
    /// How many commands may run concurrently on this device, across every
    /// caller.
    ///
    /// Enforced by the host itself rather than trusted to the caller. A central
    /// manager also schedules against its own quota, but that binds only work the
    /// manager dispatched — a control end reaching this device through an
    /// open-source signal server bypasses it entirely, so the device keeps its own
    /// ceiling.
    pub max_concurrent_executions: u32,
    /// Total wall-time ceiling for each AI-started command, in seconds.
    pub max_command_runtime_seconds: u32,
    /// Whether one-shot PTY execution is allowed on this device. This is a
    /// machine-wide local ceiling, independent of which signaling service the
    /// host connects to. Defaults on; the normal execution-mode and exact
    /// approval gates still apply.
    pub exec_pty_enabled: bool,
    /// Whether an otherwise-supported PTY may launch a bounded sudo/doas
    /// wrapper. Defaults off and is forced off whenever `exec_pty_enabled` is
    /// disabled. Runtime support is reported separately and cannot be widened
    /// by this preference.
    pub interactive_elevation_enabled: bool,
}

impl Default for AiExecutionPolicy {
    fn default() -> Self {
        Self {
            execution_mode: ExecutionMode::default(),
            max_concurrent_executions: DEFAULT_MAX_CONCURRENT_EXECUTIONS,
            max_command_runtime_seconds: DEFAULT_MAX_COMMAND_RUNTIME_SECONDS,
            exec_pty_enabled: true,
            interactive_elevation_enabled: false,
        }
    }
}

impl AiExecutionPolicy {
    /// Project the public view returned by the query endpoint.
    pub fn public_view(
        &self,
        exec_pty_supported: bool,
        interactive_elevation_supported: bool,
    ) -> AiExecutionPolicyPublic {
        AiExecutionPolicyPublic {
            execution_mode: self.execution_mode,
            max_concurrent_executions: self.max_concurrent_executions,
            max_command_runtime_seconds: self.max_command_runtime_seconds,
            exec_pty_enabled: self.exec_pty_enabled,
            interactive_elevation_enabled: self.interactive_elevation_enabled,
            exec_pty_supported,
            interactive_elevation_supported,
        }
    }

    /// Apply an update in place. `None` leaves the stored mode unchanged; a
    /// not-yet-selectable mode (`session_approved` / `automated`) is ignored so
    /// the persisted value stays in the usable set.
    pub fn apply_update(&mut self, update: AiExecutionPolicyUpdate) {
        if let Some(execution_mode) = update.execution_mode
            && is_m2_selectable(execution_mode)
        {
            self.execution_mode = execution_mode;
        }
        // Clamped rather than rejected: this is a safety rail, and a caller that
        // asks for more than the rail allows still gets a working device at the
        // highest permitted value instead of a failed save.
        if let Some(max) = update.max_concurrent_executions {
            self.max_concurrent_executions =
                max.clamp(MIN_MAX_CONCURRENT_EXECUTIONS, MAX_MAX_CONCURRENT_EXECUTIONS);
        }
        if let Some(seconds) = update.max_command_runtime_seconds {
            self.max_command_runtime_seconds = seconds.clamp(
                MIN_MAX_COMMAND_RUNTIME_SECONDS,
                MAX_MAX_COMMAND_RUNTIME_SECONDS,
            );
        }
        if let Some(enabled) = update.exec_pty_enabled {
            self.exec_pty_enabled = enabled;
            if !enabled {
                self.interactive_elevation_enabled = false;
            }
        }
        if let Some(enabled) = update.interactive_elevation_enabled {
            self.interactive_elevation_enabled = self.exec_pty_enabled && enabled;
        }
    }
}

/// Public view returned by `GET /api/desk/settings/ai-policy`. Carries no
/// secret (the policy never held one).
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema, Default)]
pub struct AiExecutionPolicyPublic {
    pub execution_mode: ExecutionMode,
    /// How many commands may run on this device at once.
    pub max_concurrent_executions: u32,
    /// Total wall-time ceiling for one AI command, in seconds.
    pub max_command_runtime_seconds: u32,
    /// Machine-wide local switch for ordinary one-shot PTY execution.
    pub exec_pty_enabled: bool,
    /// Machine-wide local switch for bounded sudo/doas PTY execution.
    pub interactive_elevation_enabled: bool,
    /// Whether this host build/runtime can execute an ordinary one-shot PTY.
    pub exec_pty_supported: bool,
    /// Whether this host currently has the root ServiceDaemon containment
    /// required for interactive elevation.
    pub interactive_elevation_supported: bool,
}

/// Update body for `POST /api/desk/settings/ai-policy`.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema, Default)]
pub struct AiExecutionPolicyUpdate {
    /// `None` leaves the stored mode unchanged. A not-yet-selectable mode
    /// (`session_approved` / `automated`) is ignored.
    pub execution_mode: Option<ExecutionMode>,
    /// `None` leaves the stored ceiling unchanged. Out-of-range values are clamped
    /// into [`MIN_MAX_CONCURRENT_EXECUTIONS`]..=[`MAX_MAX_CONCURRENT_EXECUTIONS`].
    pub max_concurrent_executions: Option<u32>,
    /// `None` leaves the stored wall-time ceiling unchanged. Out-of-range values
    /// are clamped into the device-supported range.
    pub max_command_runtime_seconds: Option<u32>,
    /// `None` leaves the local switch unchanged. Setting this to `false` also
    /// resets `interactive_elevation_enabled` to `false`.
    pub exec_pty_enabled: Option<bool>,
    /// `None` leaves the local switch unchanged. A `true` value is ignored while
    /// ordinary PTY execution is disabled.
    pub interactive_elevation_enabled: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ceiling defaults to a usable value and a config written before the field
    /// existed picks it up, rather than deserializing to zero and silently refusing
    /// every command.
    #[test]
    fn a_config_without_the_ceiling_gets_a_working_default() {
        let policy: AiExecutionPolicy = serde_json::from_str("{}").unwrap();
        assert_eq!(
            policy.max_concurrent_executions,
            DEFAULT_MAX_CONCURRENT_EXECUTIONS
        );
        assert!(policy.max_concurrent_executions >= MIN_MAX_CONCURRENT_EXECUTIONS);
        assert_eq!(
            policy.max_command_runtime_seconds,
            DEFAULT_MAX_COMMAND_RUNTIME_SECONDS
        );
    }

    /// An out-of-range ceiling is clamped, never stored as-is and never zero.
    #[test]
    fn the_ceiling_is_clamped_into_range() {
        let mut policy = AiExecutionPolicy::default();

        policy.apply_update(AiExecutionPolicyUpdate {
            execution_mode: None,
            max_concurrent_executions: Some(0),
            max_command_runtime_seconds: None,
            ..Default::default()
        });
        assert_eq!(
            policy.max_concurrent_executions,
            MIN_MAX_CONCURRENT_EXECUTIONS
        );

        policy.apply_update(AiExecutionPolicyUpdate {
            execution_mode: None,
            max_concurrent_executions: Some(u32::MAX),
            max_command_runtime_seconds: None,
            ..Default::default()
        });
        assert_eq!(
            policy.max_concurrent_executions,
            MAX_MAX_CONCURRENT_EXECUTIONS
        );

        policy.apply_update(AiExecutionPolicyUpdate {
            execution_mode: None,
            max_concurrent_executions: Some(8),
            max_command_runtime_seconds: None,
            ..Default::default()
        });
        assert_eq!(policy.max_concurrent_executions, 8);
    }

    /// An update that only changes the mode leaves the ceiling alone, and vice
    /// versa: the two fields are independently optional.
    #[test]
    fn each_field_can_be_updated_without_disturbing_the_other() {
        let mut policy = AiExecutionPolicy {
            execution_mode: ExecutionMode::SuggestOnly,
            max_concurrent_executions: 9,
            max_command_runtime_seconds: DEFAULT_MAX_COMMAND_RUNTIME_SECONDS,
            ..Default::default()
        };
        policy.apply_update(AiExecutionPolicyUpdate {
            execution_mode: Some(ExecutionMode::ConfirmEachAction),
            max_concurrent_executions: None,
            max_command_runtime_seconds: None,
            ..Default::default()
        });
        assert_eq!(policy.max_concurrent_executions, 9);
        assert_eq!(policy.execution_mode, ExecutionMode::ConfirmEachAction);

        policy.apply_update(AiExecutionPolicyUpdate {
            execution_mode: None,
            max_concurrent_executions: Some(3),
            max_command_runtime_seconds: None,
            ..Default::default()
        });
        assert_eq!(policy.execution_mode, ExecutionMode::ConfirmEachAction);
        assert_eq!(policy.max_concurrent_executions, 3);
    }

    #[test]
    fn command_runtime_defaults_to_ten_minutes_and_is_clamped() {
        let mut policy = AiExecutionPolicy::default();
        assert_eq!(policy.max_command_runtime_seconds, 600);

        policy.apply_update(AiExecutionPolicyUpdate {
            execution_mode: None,
            max_concurrent_executions: None,
            max_command_runtime_seconds: Some(1),
            ..Default::default()
        });
        assert_eq!(
            policy.max_command_runtime_seconds,
            MIN_MAX_COMMAND_RUNTIME_SECONDS
        );

        policy.apply_update(AiExecutionPolicyUpdate {
            execution_mode: None,
            max_concurrent_executions: None,
            max_command_runtime_seconds: Some(u32::MAX),
            ..Default::default()
        });
        assert_eq!(
            policy.max_command_runtime_seconds,
            MAX_MAX_COMMAND_RUNTIME_SECONDS
        );
    }

    /// Default execution mode is `suggest_only`, and a config written before the
    /// field existed deserializes to it (via `#[serde(default)]`).
    #[test]
    fn execution_mode_defaults_to_suggest_only() {
        assert_eq!(
            AiExecutionPolicy::default().execution_mode,
            ExecutionMode::SuggestOnly
        );
        let s: AiExecutionPolicy = serde_json::from_str("{}").expect("empty config");
        assert_eq!(s.execution_mode, ExecutionMode::SuggestOnly);
    }

    /// Update accepts the three selectable modes and ignores the
    /// not-yet-selectable ones, so the persisted value never leaves the usable
    /// set. `None` leaves the stored mode unchanged.
    #[test]
    fn update_execution_mode_rejects_non_selectable() {
        let mut s = AiExecutionPolicy::default();

        for mode in [
            ExecutionMode::ReadOnly,
            ExecutionMode::ConfirmEachAction,
            ExecutionMode::SuggestOnly,
        ] {
            s.apply_update(AiExecutionPolicyUpdate {
                execution_mode: Some(mode),
                max_concurrent_executions: None,
                max_command_runtime_seconds: None,
                ..Default::default()
            });
            assert_eq!(s.execution_mode, mode);
        }

        s.apply_update(AiExecutionPolicyUpdate {
            execution_mode: Some(ExecutionMode::ConfirmEachAction),
            max_concurrent_executions: None,
            max_command_runtime_seconds: None,
            ..Default::default()
        });
        for mode in [ExecutionMode::SessionApproved, ExecutionMode::Automated] {
            s.apply_update(AiExecutionPolicyUpdate {
                execution_mode: Some(mode),
                max_concurrent_executions: None,
                max_command_runtime_seconds: None,
                ..Default::default()
            });
            assert_eq!(
                s.execution_mode,
                ExecutionMode::ConfirmEachAction,
                "not-selectable mode {mode:?} must not be persisted"
            );
        }

        // None leaves the stored mode unchanged.
        s.apply_update(AiExecutionPolicyUpdate::default());
        assert_eq!(s.execution_mode, ExecutionMode::ConfirmEachAction);
    }

    /// The public view carries the execution mode (it is not a secret).
    #[test]
    fn public_view_reports_execution_mode() {
        let mut s = AiExecutionPolicy::default();
        s.apply_update(AiExecutionPolicyUpdate {
            execution_mode: Some(ExecutionMode::ConfirmEachAction),
            max_concurrent_executions: None,
            max_command_runtime_seconds: None,
            ..Default::default()
        });
        assert_eq!(
            s.public_view(true, false).execution_mode,
            ExecutionMode::ConfirmEachAction
        );
    }

    #[test]
    fn pty_defaults_are_safe_and_missing_fields_use_them() {
        let default = AiExecutionPolicy::default();
        assert!(default.exec_pty_enabled);
        assert!(!default.interactive_elevation_enabled);

        let legacy: AiExecutionPolicy = serde_json::from_str("{}").expect("legacy config");
        assert!(legacy.exec_pty_enabled);
        assert!(!legacy.interactive_elevation_enabled);
    }

    #[test]
    fn disabling_pty_persistently_resets_elevation() {
        let mut policy = AiExecutionPolicy {
            interactive_elevation_enabled: true,
            ..Default::default()
        };

        policy.apply_update(AiExecutionPolicyUpdate {
            exec_pty_enabled: Some(false),
            interactive_elevation_enabled: Some(true),
            ..Default::default()
        });

        assert!(!policy.exec_pty_enabled);
        assert!(!policy.interactive_elevation_enabled);
    }
}
