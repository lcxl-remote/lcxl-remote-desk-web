//! Static first-party edge adapter registry.
//!
//! The registry describes code that is compiled into the thin edge. It is not
//! runtime plugin discovery: adapters are registered explicitly by Rust code,
//! and their capability ids must already exist in the central Provider
//! Registry.

use std::{collections::BTreeMap, fmt};

use desk_agent_protocol::capability_provider::{CapabilityLimits, ExecutionLocality};

use crate::provider_registry::ProviderRegistry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeAdapterDescriptor {
    pub adapter_id: String,
    pub adapter_version: String,
    pub capability_ids: Vec<String>,
    pub limits: CapabilityLimits,
}

impl EdgeAdapterDescriptor {
    fn validate(&self, providers: &ProviderRegistry) -> Result<(), EdgeRegistryError> {
        if self.adapter_id.trim().is_empty() || self.adapter_version.trim().is_empty() {
            return Err(EdgeRegistryError::InvalidAdapterIdentity(
                self.adapter_id.clone(),
            ));
        }
        self.limits
            .validate()
            .map_err(|_| EdgeRegistryError::InvalidLimits(self.adapter_id.clone()))?;
        if self.capability_ids.is_empty() {
            return Err(EdgeRegistryError::MissingCapability(
                self.adapter_id.clone(),
            ));
        }
        let mut canonical = self.capability_ids.clone();
        canonical.sort();
        canonical.dedup();
        if canonical.len() != self.capability_ids.len()
            || canonical.iter().any(|value| value.trim().is_empty())
        {
            return Err(EdgeRegistryError::InvalidCapabilityList(
                self.adapter_id.clone(),
            ));
        }
        for capability_id in &self.capability_ids {
            let capability = providers.capability(capability_id).ok_or_else(|| {
                EdgeRegistryError::UnknownCapability {
                    adapter_id: self.adapter_id.clone(),
                    capability_id: capability_id.clone(),
                }
            })?;
            if capability.wire.execution_locality == ExecutionLocality::Central {
                return Err(EdgeRegistryError::CentralCapability {
                    adapter_id: self.adapter_id.clone(),
                    capability_id: capability_id.clone(),
                });
            }
            if !capability.adapter_ids.contains(&self.adapter_id) {
                return Err(EdgeRegistryError::AdapterNotDeclared {
                    adapter_id: self.adapter_id.clone(),
                    capability_id: capability_id.clone(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct EdgeAdapterRegistryBuilder {
    adapters: Vec<EdgeAdapterDescriptor>,
}

impl EdgeAdapterRegistryBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(mut self, adapter: EdgeAdapterDescriptor) -> Self {
        self.adapters.push(adapter);
        self
    }

    pub fn build(
        self,
        providers: &ProviderRegistry,
    ) -> Result<EdgeAdapterRegistry, EdgeRegistryError> {
        let mut adapters = BTreeMap::new();
        for mut adapter in self.adapters {
            adapter.validate(providers)?;
            adapter.capability_ids.sort();
            let adapter_id = adapter.adapter_id.clone();
            if adapters.insert(adapter_id.clone(), adapter).is_some() {
                return Err(EdgeRegistryError::DuplicateAdapterId(adapter_id));
            }
        }
        Ok(EdgeAdapterRegistry { adapters })
    }
}

#[derive(Debug, Clone)]
pub struct EdgeAdapterRegistry {
    adapters: BTreeMap<String, EdgeAdapterDescriptor>,
}

impl EdgeAdapterRegistry {
    pub fn adapters(&self) -> impl ExactSizeIterator<Item = &EdgeAdapterDescriptor> {
        self.adapters.values()
    }

    pub fn adapter(&self, adapter_id: &str) -> Option<&EdgeAdapterDescriptor> {
        self.adapters.get(adapter_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeRegistryError {
    InvalidAdapterIdentity(String),
    InvalidLimits(String),
    MissingCapability(String),
    InvalidCapabilityList(String),
    UnknownCapability {
        adapter_id: String,
        capability_id: String,
    },
    CentralCapability {
        adapter_id: String,
        capability_id: String,
    },
    AdapterNotDeclared {
        adapter_id: String,
        capability_id: String,
    },
    DuplicateAdapterId(String),
}

impl fmt::Display for EdgeRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAdapterIdentity(adapter) => {
                write!(f, "adapter {adapter} has an invalid identity")
            }
            Self::InvalidLimits(adapter) => write!(f, "adapter {adapter} has invalid limits"),
            Self::MissingCapability(adapter) => {
                write!(f, "adapter {adapter} does not declare a capability")
            }
            Self::InvalidCapabilityList(adapter) => {
                write!(f, "adapter {adapter} has an invalid capability list")
            }
            Self::UnknownCapability {
                adapter_id,
                capability_id,
            } => write!(
                f,
                "adapter {adapter_id} references unknown capability {capability_id}"
            ),
            Self::CentralCapability {
                adapter_id,
                capability_id,
            } => write!(
                f,
                "adapter {adapter_id} cannot implement central capability {capability_id}"
            ),
            Self::AdapterNotDeclared {
                adapter_id,
                capability_id,
            } => write!(
                f,
                "capability {capability_id} does not declare adapter {adapter_id}"
            ),
            Self::DuplicateAdapterId(adapter) => write!(f, "duplicate adapter id {adapter}"),
        }
    }
}

impl std::error::Error for EdgeRegistryError {}

#[cfg(test)]
mod tests {
    use desk_agent_protocol::capability_provider::CapabilityLimits;

    use super::*;
    use crate::device_assistant::{DESKTOP_UI_CAPABILITY_ID, device_assistant_provider_registry};

    fn descriptor(adapter_id: &str) -> EdgeAdapterDescriptor {
        EdgeAdapterDescriptor {
            adapter_id: adapter_id.into(),
            adapter_version: "v1".into(),
            capability_ids: vec![DESKTOP_UI_CAPABILITY_ID.into()],
            limits: CapabilityLimits {
                max_input_bytes: 1024,
                max_output_bytes: 1024,
                max_objects: 1,
                hard_timeout_ms: 1_000,
            },
        }
    }

    #[test]
    fn inventory_is_stable_and_bound_to_provider_capabilities() {
        let providers = device_assistant_provider_registry();
        let registry = EdgeAdapterRegistryBuilder::new()
            .register(descriptor("windows.uia"))
            .build(&providers)
            .unwrap();
        assert_eq!(registry.adapters().len(), 1);
        assert_eq!(
            registry.adapters().next().unwrap().adapter_id,
            "windows.uia"
        );
    }

    #[test]
    fn undeclared_adapter_fails_closed() {
        let providers = device_assistant_provider_registry();
        let error = EdgeAdapterRegistryBuilder::new()
            .register(descriptor("wrong.adapter"))
            .build(&providers)
            .unwrap_err();
        assert!(matches!(
            error,
            EdgeRegistryError::AdapterNotDeclared { .. }
        ));
    }
}
