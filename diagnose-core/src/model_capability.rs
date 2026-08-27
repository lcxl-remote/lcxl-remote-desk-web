//! Provider-neutral model capability and request requirement contracts.

use desk_agent_protocol::Capability;

use crate::capability_availability::CapabilityAvailability;
use crate::chat::ChatMessage;
use crate::registry::RegisteredTool;
use desk_agent_protocol::capability_provider::CapabilityBlockedReason;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub image_input: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelRequirements {
    pub image_input: bool,
}

impl ModelRequirements {
    pub const TEXT_ONLY: Self = Self { image_input: false };
    pub const IMAGE_INPUT: Self = Self { image_input: true };

    pub fn for_messages(messages: &[ChatMessage]) -> Self {
        Self {
            image_input: messages
                .iter()
                .any(|message| message.image_data_url.is_some()),
        }
    }

    /// Derive requirements from server-authoritative registered-tool metadata.
    /// This deliberately uses the required capability rather than a model-facing
    /// tool name, so renaming a tool cannot silently remove its image gate.
    pub fn for_registered_tools<'a>(tools: impl IntoIterator<Item = &'a RegisteredTool>) -> Self {
        Self {
            image_input: tools.into_iter().any(tool_requires_image_input),
        }
    }

    pub fn union(self, other: Self) -> Self {
        Self {
            image_input: self.image_input || other.image_input,
        }
    }
}

impl ModelCapabilities {
    pub fn satisfies(self, requirements: ModelRequirements) -> bool {
        !requirements.image_input || self.image_input
    }
}

/// Remove tools that can produce a visual attachment when the selected model is
/// text-only. The same filtered registry must be used both for advertising and
/// returned-call lookup.
pub fn filter_model_compatible_tools(
    registry: &[RegisteredTool],
    capabilities: ModelCapabilities,
) -> Vec<RegisteredTool> {
    registry
        .iter()
        .filter(|tool| capabilities.image_input || !tool_requires_image_input(tool))
        .cloned()
        .collect()
}

/// Apply model-input requirements to an already projected Provider inventory.
/// Edge readiness cannot know which model the control plane selected, so this
/// central projection is the authoritative discoverable/callable image gate.
pub fn apply_model_compatibility(
    inventory: &mut [CapabilityAvailability],
    capabilities: ModelCapabilities,
) {
    if capabilities.image_input {
        return;
    }
    for item in inventory
        .iter_mut()
        .filter(|item| item.capability_id == crate::device_assistant::CURRENT_SCREEN_CAPABILITY_ID)
    {
        item.ready = false;
        item.reason = Some(CapabilityBlockedReason::ModelIncompatible);
    }
}

fn tool_requires_image_input(tool: &RegisteredTool) -> bool {
    tool.required_capability == Capability::ScreenCaptureCurrent
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ChatMessage, ChatRole};
    use crate::read_tools::read_tool_registry;

    #[test]
    fn requirements_follow_actual_message_images() {
        let text = vec![ChatMessage::text("m1", ChatRole::User, "hello")];
        assert_eq!(
            ModelRequirements::for_messages(&text),
            ModelRequirements::TEXT_ONLY
        );
        let visual = vec![
            ChatMessage::text("m2", ChatRole::User, "screen")
                .with_image("data:image/jpeg;base64,AQID"),
        ];
        assert_eq!(
            ModelRequirements::for_messages(&visual),
            ModelRequirements::IMAGE_INPUT
        );
    }

    #[test]
    fn capabilities_satisfy_only_supported_requirements() {
        assert!(ModelCapabilities::default().satisfies(ModelRequirements::TEXT_ONLY));
        assert!(!ModelCapabilities::default().satisfies(ModelRequirements::IMAGE_INPUT));
        assert!(ModelCapabilities { image_input: true }.satisfies(ModelRequirements::IMAGE_INPUT));
    }

    #[test]
    fn text_model_filters_only_visual_read_tool() {
        let all = read_tool_registry();
        let filtered = filter_model_compatible_tools(&all, ModelCapabilities::default());
        assert!(all.iter().any(|tool| tool.name() == "read_current_screen"));
        assert!(
            !filtered
                .iter()
                .any(|tool| tool.name() == "read_current_screen")
        );
        assert!(
            filtered
                .iter()
                .any(|tool| tool.name() == "read_system_info")
        );
    }

    #[test]
    fn tool_requirements_follow_capability_not_tool_name() {
        let mut screen = read_tool_registry()
            .into_iter()
            .find(|tool| tool.required_capability == Capability::ScreenCaptureCurrent)
            .unwrap();
        screen.spec.name = "renamed_screen_reader".into();

        assert_eq!(
            ModelRequirements::for_registered_tools([&screen]),
            ModelRequirements::IMAGE_INPUT
        );
    }

    #[test]
    fn text_model_marks_current_screen_unavailable_in_inventory() {
        let mut inventory = vec![CapabilityAvailability {
            provider_id: crate::device_assistant::CURRENT_SCREEN_PROVIDER_ID.into(),
            capability_id: crate::device_assistant::CURRENT_SCREEN_CAPABILITY_ID.into(),
            tool_name: "read_current_screen".into(),
            compiled: true,
            enabled: true,
            connected: true,
            ready: true,
            reason: None,
        }];
        apply_model_compatibility(&mut inventory, ModelCapabilities::default());
        assert!(!inventory[0].ready);
        assert_eq!(
            inventory[0].reason,
            Some(CapabilityBlockedReason::ModelIncompatible)
        );

        inventory[0].ready = true;
        inventory[0].reason = None;
        apply_model_compatibility(&mut inventory, ModelCapabilities { image_input: true });
        assert!(inventory[0].ready);
        assert_eq!(inventory[0].reason, None);
    }
}
