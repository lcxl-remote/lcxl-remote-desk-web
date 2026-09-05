//! Static first-party Capability Provider registry.
//!
//! Registration is explicit code. Configuration may later disable or narrow a
//! descriptor, but it cannot load code or invent a capability that was not
//! compiled into this registry.

use std::{collections::BTreeMap, fmt};

use desk_agent_protocol::{
    Capability,
    capability_provider::{
        CapabilityContractError, CapabilityWireDescriptor, ProviderWireDescriptor,
    },
};

use crate::{
    chat::ToolSpec,
    registry::{RegisteredTool, ToolEffect},
};

pub const MAX_TOOL_SCHEMA_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityDescriptor {
    pub wire: CapabilityWireDescriptor,
    pub tool_spec: ToolSpec,
    pub required_capability: Capability,
    /// First-party adapter ids that may satisfy this capability. An empty list
    /// is valid only for a central-only capability.
    pub adapter_ids: Vec<String>,
}

impl CapabilityDescriptor {
    pub fn validate(&self) -> Result<(), RegistryError> {
        self.wire.validate().map_err(RegistryError::Contract)?;
        if self.tool_spec.name != self.wire.tool_name {
            return Err(RegistryError::ToolSpecNameMismatch {
                capability_id: self.wire.capability_id.clone(),
            });
        }
        let schema = serde_json::to_vec(&self.tool_spec.parameters_schema)
            .map_err(|_| RegistryError::InvalidToolSchema(self.tool_spec.name.clone()))?;
        if schema.is_empty() || schema.len() > MAX_TOOL_SCHEMA_BYTES {
            return Err(RegistryError::InvalidToolSchema(
                self.tool_spec.name.clone(),
            ));
        }
        if !self.tool_spec.parameters_schema.is_object() {
            return Err(RegistryError::InvalidToolSchema(
                self.tool_spec.name.clone(),
            ));
        }
        if self.wire.execution_locality
            != desk_agent_protocol::capability_provider::ExecutionLocality::Central
            && self.adapter_ids.is_empty()
        {
            return Err(RegistryError::MissingEdgeAdapter(
                self.wire.capability_id.clone(),
            ));
        }
        let mut canonical = self.adapter_ids.clone();
        canonical.sort();
        canonical.dedup();
        if canonical.len() != self.adapter_ids.len()
            || canonical.iter().any(|value| value.trim().is_empty())
        {
            return Err(RegistryError::InvalidEdgeAdapter(
                self.wire.capability_id.clone(),
            ));
        }
        Ok(())
    }

    fn legacy_effect(&self) -> ToolEffect {
        if self.wire.effect.is_side_effecting() {
            ToolEffect::Mutating
        } else {
            ToolEffect::ReadOnly
        }
    }

    pub fn registered_tool(&self) -> RegisteredTool {
        RegisteredTool {
            spec: self.tool_spec.clone(),
            required_capability: self.required_capability,
            effect: self.legacy_effect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDescriptor {
    pub wire: ProviderWireDescriptor,
    pub capabilities: Vec<CapabilityDescriptor>,
}

impl ProviderDescriptor {
    pub fn validate(&self) -> Result<(), RegistryError> {
        self.wire.validate().map_err(RegistryError::Contract)?;
        if self.wire.capabilities.len() != self.capabilities.len() {
            return Err(RegistryError::WireCapabilityMismatch(
                self.wire.provider_id.clone(),
            ));
        }
        for capability in &self.capabilities {
            capability.validate()?;
            let Some(wire) = self
                .wire
                .capabilities
                .iter()
                .find(|wire| wire.capability_id == capability.wire.capability_id)
            else {
                return Err(RegistryError::WireCapabilityMismatch(
                    self.wire.provider_id.clone(),
                ));
            };
            if wire != &capability.wire {
                return Err(RegistryError::WireCapabilityMismatch(
                    self.wire.provider_id.clone(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProviderRegistryBuilder {
    providers: Vec<ProviderDescriptor>,
}

impl ProviderRegistryBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(mut self, provider: ProviderDescriptor) -> Self {
        self.providers.push(provider);
        self
    }

    pub fn build(self) -> Result<ProviderRegistry, RegistryError> {
        let mut providers = BTreeMap::new();
        let mut capability_ids = BTreeMap::<String, String>::new();
        let mut tool_names = BTreeMap::<String, String>::new();

        for mut provider in self.providers {
            provider.validate()?;
            let provider_id = provider.wire.provider_id.clone();
            if providers.contains_key(&provider_id) {
                return Err(RegistryError::DuplicateProviderId(provider_id));
            }
            provider
                .capabilities
                .sort_by(|left, right| left.wire.capability_id.cmp(&right.wire.capability_id));
            provider.wire.capabilities = provider
                .capabilities
                .iter()
                .map(|capability| capability.wire.clone())
                .collect();
            for capability in &provider.capabilities {
                if let Some(first_provider) = capability_ids
                    .insert(capability.wire.capability_id.clone(), provider_id.clone())
                {
                    return Err(RegistryError::DuplicateCapabilityId {
                        capability_id: capability.wire.capability_id.clone(),
                        first_provider,
                        second_provider: provider_id,
                    });
                }
                if let Some(first_provider) =
                    tool_names.insert(capability.tool_spec.name.clone(), provider_id.clone())
                {
                    return Err(RegistryError::DuplicateToolName {
                        tool_name: capability.tool_spec.name.clone(),
                        first_provider,
                        second_provider: provider_id,
                    });
                }
            }
            providers.insert(provider.wire.provider_id.clone(), provider);
        }
        for provider in providers.values() {
            for capability in &provider.capabilities {
                for fallback in &capability.wire.fallback_capability_ids {
                    if !capability_ids.contains_key(fallback) {
                        return Err(RegistryError::UnknownFallbackCapability {
                            capability_id: capability.wire.capability_id.clone(),
                            fallback_capability_id: fallback.clone(),
                        });
                    }
                }
            }
        }
        Ok(ProviderRegistry {
            providers,
            web_search_binding: None,
            command_policy: None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, ProviderDescriptor>,
    web_search_binding: Option<crate::web_research::SearchBinding>,
    command_policy: Option<crate::command_confirmation::CommandPolicyContext>,
}

impl ProviderRegistry {
    /// Per-request authority supplied by the runtime after authenticating the
    /// owner. It is not included in the discoverable capability catalog.
    pub fn with_command_policy(
        mut self,
        policy: crate::command_confirmation::CommandPolicyContext,
    ) -> Self {
        for provider in self.providers.values_mut() {
            for capability in &mut provider.capabilities {
                if capability.tool_spec.name == crate::command_confirmation::COMMAND_TOOL {
                    if let Some(start) = capability
                        .tool_spec
                        .description
                        .find(" Current device shells:")
                    {
                        capability.tool_spec.description.truncate(start);
                    }
                    capability.tool_spec.parameters_schema["properties"]["shell"]["enum"] =
                        serde_json::json!(policy.available_shells);
                    capability.tool_spec.description.push_str(&format!(
                        " Current device shells: {}. Effective runtime ceiling: {} ms; stdout/stderr each bounded to 65536 bytes. Changes require fresh approval.",
                        policy.available_shells.join(", "), policy.max_runtime_ms));
                }
            }
        }
        self.command_policy = Some(policy);
        self
    }

    pub fn command_policy(&self) -> Option<&crate::command_confirmation::CommandPolicyContext> {
        self.command_policy.as_ref()
    }

    pub fn with_web_search_binding(
        mut self,
        binding: Option<crate::web_research::SearchBinding>,
    ) -> Self {
        self.web_search_binding = binding;
        self
    }

    pub fn web_search_binding(&self) -> Option<&crate::web_research::SearchBinding> {
        self.web_search_binding.as_ref()
    }

    pub fn providers(&self) -> impl ExactSizeIterator<Item = &ProviderDescriptor> {
        self.providers.values()
    }

    pub fn provider(&self, provider_id: &str) -> Option<&ProviderDescriptor> {
        self.providers.get(provider_id)
    }

    pub fn capability(&self, capability_id: &str) -> Option<&CapabilityDescriptor> {
        self.providers
            .values()
            .flat_map(|provider| provider.capabilities.iter())
            .find(|capability| capability.wire.capability_id == capability_id)
    }

    pub fn provider_for_capability(&self, capability_id: &str) -> Option<&ProviderDescriptor> {
        self.providers.values().find(|provider| {
            provider
                .capabilities
                .iter()
                .any(|capability| capability.wire.capability_id == capability_id)
        })
    }

    pub fn capability_for_tool(&self, tool_name: &str) -> Option<&CapabilityDescriptor> {
        self.providers
            .values()
            .flat_map(|provider| provider.capabilities.iter())
            .find(|capability| capability.tool_spec.name == tool_name)
    }

    /// Stable descriptor inventory safe for persistence/UI projection. Tool JSON
    /// schema and credentials are intentionally absent from this view.
    pub fn wire_inventory(&self) -> Vec<ProviderWireDescriptor> {
        self.providers
            .values()
            .map(|provider| provider.wire.clone())
            .collect()
    }

    /// Transitional agent-loop view. Stage 0 migrations use this to preserve the
    /// exact existing tool exposure while making the Provider Registry the only
    /// source that creates the list.
    pub fn registered_tools(&self) -> Vec<RegisteredTool> {
        let mut capabilities: Vec<_> = self
            .providers
            .values()
            .flat_map(|provider| provider.capabilities.iter())
            .collect();
        capabilities.sort_by(|left, right| left.tool_spec.name.cmp(&right.tool_spec.name));
        capabilities
            .into_iter()
            .map(CapabilityDescriptor::registered_tool)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    Contract(CapabilityContractError),
    ToolSpecNameMismatch {
        capability_id: String,
    },
    InvalidToolSchema(String),
    MissingEdgeAdapter(String),
    InvalidEdgeAdapter(String),
    WireCapabilityMismatch(String),
    DuplicateProviderId(String),
    DuplicateCapabilityId {
        capability_id: String,
        first_provider: String,
        second_provider: String,
    },
    DuplicateToolName {
        tool_name: String,
        first_provider: String,
        second_provider: String,
    },
    UnknownFallbackCapability {
        capability_id: String,
        fallback_capability_id: String,
    },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Contract(error) => write!(f, "invalid capability contract: {error}"),
            Self::ToolSpecNameMismatch { capability_id } => {
                write!(f, "tool spec name disagrees for capability {capability_id}")
            }
            Self::InvalidToolSchema(tool) => write!(f, "tool {tool} has an invalid schema"),
            Self::MissingEdgeAdapter(capability) => {
                write!(f, "edge capability {capability} has no adapter")
            }
            Self::InvalidEdgeAdapter(capability) => {
                write!(f, "capability {capability} has invalid edge adapters")
            }
            Self::WireCapabilityMismatch(provider) => {
                write!(
                    f,
                    "provider {provider} wire and runtime capabilities disagree"
                )
            }
            Self::DuplicateProviderId(provider) => write!(f, "duplicate provider id {provider}"),
            Self::DuplicateCapabilityId {
                capability_id,
                first_provider,
                second_provider,
            } => write!(
                f,
                "duplicate capability id {capability_id} in {first_provider} and {second_provider}"
            ),
            Self::DuplicateToolName {
                tool_name,
                first_provider,
                second_provider,
            } => write!(
                f,
                "duplicate tool name {tool_name} in {first_provider} and {second_provider}"
            ),
            Self::UnknownFallbackCapability {
                capability_id,
                fallback_capability_id,
            } => write!(
                f,
                "capability {capability_id} references unknown fallback {fallback_capability_id}"
            ),
        }
    }
}

impl std::error::Error for RegistryError {}

#[cfg(test)]
mod tests {
    use desk_agent_protocol::capability_provider::{
        AuthorizationResourceKind, CAPABILITY_PROVIDER_SCHEMA_VERSION, CapabilityAuthorizationHint,
        CapabilityDataCategory, CapabilityDataPolicy, CapabilityEffect, CapabilityLimits,
        CapabilityPlatform, CapabilityPrerequisites, CapabilityRateClass, ExecutionLocality,
        ExecutionPolicy, ProductSurface,
    };
    use serde_json::json;

    use super::*;

    fn provider(provider_id: &str, capability_id: &str, tool_name: &str) -> ProviderDescriptor {
        let wire = CapabilityWireDescriptor {
            capability_id: capability_id.into(),
            tool_name: tool_name.into(),
            display_name_key: format!("capability.{capability_id}"),
            input_schema_version: 1,
            output_schema_version: 1,
            effect: CapabilityEffect::ReadDevice,
            execution_locality: ExecutionLocality::Edge,
            prerequisites: CapabilityPrerequisites {
                platforms: vec![CapabilityPlatform::Windows],
                applications: Vec::new(),
                requires_edge_connection: true,
                requires_interactive_session: true,
                requires_credential_connection: false,
            },
            execution_policy: ExecutionPolicy::InlineOnly,
            rate_class: CapabilityRateClass::InteractiveRead,
            limits: CapabilityLimits {
                max_input_bytes: 1024,
                max_output_bytes: 1024,
                max_objects: 1,
                hard_timeout_ms: 1_000,
            },
            supports_progress: false,
            supports_cancel: false,
            data_policy: CapabilityDataPolicy {
                reads: vec![CapabilityDataCategory::DesktopSessionMetadata],
                may_export_data: false,
            },
            authorization_hint: CapabilityAuthorizationHint {
                resources: vec![AuthorizationResourceKind::TargetDevice],
            },
            fallback_capability_ids: Vec::new(),
            surfaces: vec![ProductSurface::OssPersonalOwner],
        };
        let capability = CapabilityDescriptor {
            wire: wire.clone(),
            tool_spec: ToolSpec {
                name: tool_name.into(),
                description: "read".into(),
                parameters_schema: json!({"type": "object"}),
            },
            required_capability: Capability::SystemInfo,
            adapter_ids: vec!["test.adapter".into()],
        };
        ProviderDescriptor {
            wire: ProviderWireDescriptor {
                schema_version: CAPABILITY_PROVIDER_SCHEMA_VERSION,
                provider_id: provider_id.into(),
                display_name_key: format!("provider.{provider_id}"),
                provider_version: 1,
                capabilities: vec![wire],
            },
            capabilities: vec![capability],
        }
    }

    #[test]
    fn inventory_is_stable_regardless_of_registration_order() {
        let a = provider("a", "a.read", "read_a");
        let b = provider("b", "b.read", "read_b");
        let left = ProviderRegistryBuilder::new()
            .register(b.clone())
            .register(a.clone())
            .build()
            .unwrap()
            .wire_inventory();
        let right = ProviderRegistryBuilder::new()
            .register(a)
            .register(b)
            .build()
            .unwrap()
            .wire_inventory();
        assert_eq!(left, right);
        assert_eq!(left[0].provider_id, "a");
    }

    #[test]
    fn tool_name_is_globally_unique() {
        let error = ProviderRegistryBuilder::new()
            .register(provider("a", "a.read", "same"))
            .register(provider("b", "b.read", "same"))
            .build()
            .unwrap_err();
        assert!(matches!(error, RegistryError::DuplicateToolName { .. }));
    }

    #[test]
    fn wire_inventory_does_not_include_tool_description_or_schema() {
        let registry = ProviderRegistryBuilder::new()
            .register(provider("a", "a.read", "read_a"))
            .build()
            .unwrap();
        let json = serde_json::to_string(&registry.wire_inventory()).unwrap();
        assert!(!json.contains("parameters_schema"));
        assert!(!json.contains("description"));
        assert!(!json.contains("api_key"));
        assert!(!json.contains("access_token"));
        assert!(!json.contains("\"secret\""));
    }
}
