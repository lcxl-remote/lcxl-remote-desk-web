//! Candidate selection and scope projection. Neither operation grants authority.
use crate::{provider_registry::ProviderRegistry, registry::RegisteredTool};
use desk_agent_protocol::Capability;

/// Required at every Provider registration; deliberately has no Default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExposureRequirement {
    /// Inputs and resource consent are checked by issuance and dispatch.
    NoAttachment,
    /// Bounded desktop reads may request permission without an attachment.
    DesktopRead,
    /// Requires the exact capability's selected context (objects or connection).
    SelectedContext,
}

impl ExposureRequirement {
    pub fn accepts(self, selected: bool) -> bool {
        match self {
            Self::NoAttachment | Self::DesktopRead => true,
            Self::SelectedContext => selected,
        }
    }
}

pub fn retain_candidates(
    providers: &ProviderRegistry,
    tools: &mut Vec<RegisteredTool>,
    selected_ids: &[String],
) {
    tools.retain(|tool| {
        providers
            .capability_for_tool(tool.name())
            .is_some_and(|descriptor| {
                descriptor
                    .exposure
                    .accepts(selected_ids.contains(&descriptor.wire.capability_id))
            })
    });
}

/// Call only after readiness, policy and context have selected these candidates.
/// Actual invocation still requires a live grant and exact-input authorization.
pub fn extend_scope(tools: &[RegisteredTool], granted: &mut Vec<Capability>) {
    for tool in tools {
        if !granted.contains(&tool.required_capability) {
            granted.push(tool.required_capability);
        }
    }
}

/// Desktop read candidates require a selected source or an active read grant
/// before entering the scope. Other candidates remain subject to the common
/// per-step grant filter; no invocation authority is minted here.
pub fn extend_candidate_scope(
    providers: &ProviderRegistry,
    tools: &[RegisteredTool],
    selected_ids: &[String],
    authorized_reads: &[String],
    granted: &mut Vec<Capability>,
) {
    for tool in tools {
        let Some(descriptor) = providers.capability_for_tool(tool.name()) else {
            continue;
        };
        let selected = selected_ids.contains(&descriptor.wire.capability_id);
        let eligible = match descriptor.exposure {
            ExposureRequirement::NoAttachment => true,
            ExposureRequirement::DesktopRead => {
                selected || authorized_reads.iter().any(|name| name == tool.name())
            }
            ExposureRequirement::SelectedContext => selected,
        };
        if eligible && !granted.contains(&tool.required_capability) {
            granted.push(tool.required_capability);
        }
    }
}

pub fn scope_has_mutation(providers: &ProviderRegistry, granted: &[Capability]) -> bool {
    providers.registered_tools().iter().any(|tool| {
        granted.contains(&tool.required_capability)
            && tool.effect == crate::registry::ToolEffect::Mutating
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        registry::exposed_tools,
        session::{ExecutionState, TriggerOrigin},
    };
    use desk_agent_protocol::{AgentScope, ExecutionMode};

    #[test]
    fn every_registered_tool_has_a_consistent_candidate_and_scope_projection() {
        let providers = crate::device_assistant::device_assistant_provider_registry();
        for tool in providers.registered_tools() {
            let descriptor = providers.capability_for_tool(tool.name()).unwrap();
            for selected in [false, true] {
                let ids = if selected {
                    vec![descriptor.wire.capability_id.clone()]
                } else {
                    vec![]
                };
                let mut tools = vec![tool.clone()];
                retain_candidates(&providers, &mut tools, &ids);
                assert_eq!(
                    !tools.is_empty(),
                    descriptor.exposure.accepts(selected),
                    "{}",
                    tool.name()
                );
                for authorized in [false, true] {
                    let read_names = if authorized {
                        vec![tool.name().to_string()]
                    } else {
                        vec![]
                    };
                    let mut candidates = Vec::new();
                    extend_candidate_scope(&providers, &tools, &ids, &read_names, &mut candidates);
                    let expected = match descriptor.exposure {
                        ExposureRequirement::NoAttachment => true,
                        ExposureRequirement::DesktopRead => selected || authorized,
                        ExposureRequirement::SelectedContext => selected,
                    };
                    assert_eq!(
                        candidates.contains(&tool.required_capability),
                        expected,
                        "{} selected={selected} authorized={authorized}",
                        tool.name()
                    );
                }
                let mut scope = AgentScope {
                    granted: vec![],
                    mode: ExecutionMode::ConfirmEachAction,
                    expires_at: None,
                    policy_name: None,
                };
                extend_scope(&tools, &mut scope.granted);
                let exposed = exposed_tools(
                    &tools,
                    &scope,
                    &ExecutionState::None,
                    TriggerOrigin::PermissionDecision,
                );
                assert_eq!(exposed.len(), tools.len(), "{}", tool.name());
                assert!(
                    exposed_tools(
                        &tools,
                        &AgentScope {
                            granted: vec![],
                            ..scope
                        },
                        &ExecutionState::None,
                        TriggerOrigin::PermissionDecision
                    )
                    .is_empty()
                );
            }
        }
    }
}
