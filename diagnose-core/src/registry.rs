//! Tool registry and the server-authoritative exposure matrix.
//!
//! A [`RegisteredTool`] binds a model-facing [`ToolSpec`] to the [`Capability`]
//! it requires and the [`ToolEffect`] it has. [`exposed_tools`] is the first line
//! of prompt-injection defence (§D8): the model is only ever shown the tools that
//! the current scope grants and the current mode permits — and a mutating tool is
//! hidden while a prior execution's outcome is still unknown.
//!
//! Read tools are exposed in every mode (reads are not "execution"); a mutating
//! tool is exposed only at `ConfirmEachAction` or higher. The same matrix is used
//! both to advertise tools to the model and to validate a tool call the model
//! returns, so the two can never disagree.

use desk_agent_protocol::{AgentScope, Capability, ExecutionMode};
use serde::{Deserialize, Serialize};

use crate::chat::ToolSpec;
use crate::session::{ExecutionState, TriggerOrigin};

/// Whether a tool reads state or changes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffect {
    /// Reads device state; runs immediately, no approval.
    ReadOnly,
    /// Changes device state; requires user approval before running.
    Mutating,
    /// Waits on the session's own in-flight background task; changes no device
    /// state and needs no capability grant. Exposed only while a task is running (or
    /// its outcome is still recoverable), so the model can actively await a result
    /// instead of ending the turn and being notified passively.
    WaitTask,
    /// Updates only the model-maintained, user-visible task projection stored in
    /// the current run. It grants no device authority and dispatches no work.
    RunProjection,
    /// Creates a bounded user-facing permission request. It cannot mint grants,
    /// reserve uses, or dispatch tools; approval is a separate trusted action.
    PermissionPlanning,
}

/// A tool registered with the agent loop: its model-facing spec, the capability
/// it requires (for scope gating), and its effect (for mode / approval gating).
#[derive(Debug, Clone)]
pub struct RegisteredTool {
    pub spec: ToolSpec,
    pub required_capability: Capability,
    pub effect: ToolEffect,
}

impl RegisteredTool {
    /// The tool's model-facing name.
    pub fn name(&self) -> &str {
        &self.spec.name
    }
}

/// Whether the execution mode permits a tool with this effect at all. Reads are
/// allowed in every mode (including `SuggestOnly`); mutation needs an explicit
/// confirm-or-higher mode.
fn mode_allows_effect(mode: ExecutionMode, effect: ToolEffect) -> bool {
    match effect {
        // Reads and waiting on one's own task are allowed in every mode.
        ToolEffect::ReadOnly
        | ToolEffect::WaitTask
        | ToolEffect::RunProjection
        | ToolEffect::PermissionPlanning => true,
        ToolEffect::Mutating => matches!(
            mode,
            ExecutionMode::ConfirmEachAction
                | ExecutionMode::SessionApproved
                | ExecutionMode::Automated
        ),
    }
}

/// Whether a single tool is exposed under the given scope, execution state, and
/// trigger origin. Requires the granted capability, a mode that permits the
/// effect, and — for a mutating tool — no in-flight execution whose outcome is
/// unknown **and** a turn origin that may start a new mutation (an automation turn
/// may not, so completions cannot self-trigger an unbounded execution chain).
pub fn is_exposed(
    tool: &RegisteredTool,
    scope: &AgentScope,
    execution_state: &ExecutionState,
    origin: TriggerOrigin,
) -> bool {
    // The wait tool operates on the session's own task, not the device: it needs no
    // capability grant and is offered only while there is a task to wait on.
    if tool.effect == ToolEffect::WaitTask {
        return execution_state.waitable_task().is_some();
    }
    if tool.effect == ToolEffect::RunProjection {
        return true;
    }
    if tool.effect == ToolEffect::PermissionPlanning {
        return true;
    }
    if !scope.granted.contains(&tool.required_capability) {
        return false;
    }
    if !mode_allows_effect(scope.mode, tool.effect) {
        return false;
    }
    if tool.effect == ToolEffect::Mutating
        && (!execution_state.allows_new_mutation() || !origin.allows_new_mutation())
    {
        return false;
    }
    true
}

/// The registered tools exposed for a turn (server-authoritative). Used to build
/// the tool list advertised to the model and to validate a returned tool call.
pub fn exposed_tools<'a>(
    registry: &'a [RegisteredTool],
    scope: &AgentScope,
    execution_state: &ExecutionState,
    origin: TriggerOrigin,
) -> Vec<&'a RegisteredTool> {
    registry
        .iter()
        .filter(|t| is_exposed(t, scope, execution_state, origin))
        .collect()
}

/// Look up an exposed tool by the name the model used. Returns `None` if the name
/// is unknown **or** the tool is not exposed under the current scope/state/origin —
/// so a model that names a tool it was never shown is rejected uniformly.
pub fn lookup_exposed<'a>(
    registry: &'a [RegisteredTool],
    name: &str,
    scope: &AgentScope,
    execution_state: &ExecutionState,
    origin: TriggerOrigin,
) -> Option<&'a RegisteredTool> {
    registry
        .iter()
        .find(|t| t.name() == name && is_exposed(t, scope, execution_state, origin))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, cap: Capability, effect: ToolEffect) -> RegisteredTool {
        RegisteredTool {
            spec: ToolSpec {
                name: name.into(),
                description: "t".into(),
                parameters_schema: json!({"type":"object"}),
            },
            required_capability: cap,
            effect,
        }
    }

    fn scope(granted: &[Capability], mode: ExecutionMode) -> AgentScope {
        AgentScope {
            granted: granted.to_vec(),
            mode,
            expires_at: None,
            policy_name: None,
        }
    }

    fn registry() -> Vec<RegisteredTool> {
        vec![
            tool("file_read", Capability::LogRecent, ToolEffect::ReadOnly),
            tool("sysinfo", Capability::SystemInfo, ToolEffect::ReadOnly),
            tool("file_write", Capability::LogRecent, ToolEffect::Mutating),
        ]
    }

    /// Only tools whose required capability is granted are exposed; an ungranted
    /// capability hides its tool regardless of mode.
    #[test]
    fn exposure_requires_granted_capability() {
        let reg = registry();
        let s = scope(&[Capability::SystemInfo], ExecutionMode::ReadOnly);
        let names: Vec<_> = exposed_tools(&reg, &s, &ExecutionState::None, TriggerOrigin::User)
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert_eq!(names, vec!["sysinfo"]);
    }

    /// Read tools are exposed in every mode (including SuggestOnly); mutating
    /// tools are hidden below ConfirmEachAction.
    #[test]
    fn read_exposed_everywhere_mutating_gated_by_mode() {
        let reg = registry();
        let granted = [Capability::SystemInfo, Capability::LogRecent];

        for mode in [ExecutionMode::SuggestOnly, ExecutionMode::ReadOnly] {
            let names: Vec<_> = exposed_tools(
                &reg,
                &scope(&granted, mode),
                &ExecutionState::None,
                TriggerOrigin::User,
            )
            .iter()
            .map(|t| t.name().to_string())
            .collect();
            assert!(names.contains(&"file_read".to_string()));
            assert!(names.contains(&"sysinfo".to_string()));
            assert!(
                !names.contains(&"file_write".to_string()),
                "mutating tool must be hidden in {mode:?}"
            );
        }

        // ConfirmEachAction exposes the mutating tool too.
        let names: Vec<_> = exposed_tools(
            &reg,
            &scope(&granted, ExecutionMode::ConfirmEachAction),
            &ExecutionState::None,
            TriggerOrigin::User,
        )
        .iter()
        .map(|t| t.name().to_string())
        .collect();
        assert!(names.contains(&"file_write".to_string()));
    }

    /// While an execution outcome is unknown, mutating tools are hidden but read
    /// tools stay available (read-only follow-up is allowed).
    #[test]
    fn outcome_unknown_hides_mutating_keeps_read() {
        let reg = registry();
        let s = scope(
            &[Capability::SystemInfo, Capability::LogRecent],
            ExecutionMode::ConfirmEachAction,
        );
        let unknown = ExecutionState::OutcomeUnknown {
            action: crate::session::ActionIdentity::agent_exec(1, "x", "e"),
            placeholder_message_id: "p".into(),
            since: "t".into(),
        };
        let names: Vec<_> = exposed_tools(&reg, &s, &unknown, TriggerOrigin::User)
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.contains(&"file_read".to_string()));
        assert!(
            !names.contains(&"file_write".to_string()),
            "no new mutation while an outcome is unknown"
        );
    }

    /// The wait tool is exposed only while a task is in flight, needs no capability
    /// grant, and is hidden once there is nothing to wait on.
    #[test]
    fn wait_task_exposed_only_with_an_in_flight_task() {
        // An empty scope (no capabilities, SuggestOnly) — the wait tool ignores it.
        let s = scope(&[], ExecutionMode::SuggestOnly);
        let reg = vec![tool(
            "wait_for_task",
            Capability::SystemInfo,
            ToolEffect::WaitTask,
        )];

        // No task in flight: hidden.
        assert!(exposed_tools(&reg, &s, &ExecutionState::None, TriggerOrigin::User).is_empty());

        // A dispatched background task: exposed despite the empty scope.
        let executing = ExecutionState::Executing {
            action: crate::session::ActionIdentity::agent_exec(1, "exec_x", "e"),
        };
        let names: Vec<_> = exposed_tools(&reg, &s, &executing, TriggerOrigin::User)
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert_eq!(names, vec!["wait_for_task"]);

        // An interrupted turn with no recoverable identity: nothing to wait on.
        let interrupted = ExecutionState::Interrupted { since: "t".into() };
        assert!(exposed_tools(&reg, &s, &interrupted, TriggerOrigin::User).is_empty());
    }

    /// `lookup_exposed` rejects a tool the model was never shown (unknown name or
    /// not exposed), preventing a model from invoking an out-of-scope tool.
    #[test]
    fn lookup_rejects_unexposed_tool() {
        let reg = registry();
        let s = scope(&[Capability::SystemInfo], ExecutionMode::ReadOnly);
        assert!(
            lookup_exposed(
                &reg,
                "sysinfo",
                &s,
                &ExecutionState::None,
                TriggerOrigin::User
            )
            .is_some()
        );
        // file_read needs LogRecent, which is not granted here.
        assert!(
            lookup_exposed(
                &reg,
                "file_read",
                &s,
                &ExecutionState::None,
                TriggerOrigin::User
            )
            .is_none()
        );
        // Unknown name.
        assert!(
            lookup_exposed(&reg, "nope", &s, &ExecutionState::None, TriggerOrigin::User).is_none()
        );
    }

    /// An automation turn ([`TriggerOrigin::ExecCompletion`]) never sees a mutating
    /// tool — even with the capability granted, a permissive mode, and a clean
    /// execution state — so a completion cannot self-trigger a new command. Read
    /// tools stay available so the automation turn can still inspect the result.
    #[test]
    fn automation_origin_hides_mutating_keeps_read() {
        let reg = registry();
        let s = scope(
            &[Capability::SystemInfo, Capability::LogRecent],
            ExecutionMode::ConfirmEachAction,
        );

        // A user turn under the same scope/state does expose the mutating tool.
        assert!(
            lookup_exposed(
                &reg,
                "file_write",
                &s,
                &ExecutionState::None,
                TriggerOrigin::User
            )
            .is_some()
        );

        // The automation turn hides it while keeping reads.
        let names: Vec<_> = exposed_tools(
            &reg,
            &s,
            &ExecutionState::None,
            TriggerOrigin::ExecCompletion,
        )
        .iter()
        .map(|t| t.name().to_string())
        .collect();
        assert!(names.contains(&"file_read".to_string()));
        assert!(names.contains(&"sysinfo".to_string()));
        assert!(
            !names.contains(&"file_write".to_string()),
            "an automation turn must not be offered a mutating tool"
        );
        // And a direct call to the hidden tool is rejected uniformly.
        assert!(
            lookup_exposed(
                &reg,
                "file_write",
                &s,
                &ExecutionState::None,
                TriggerOrigin::ExecCompletion
            )
            .is_none()
        );
    }
}
