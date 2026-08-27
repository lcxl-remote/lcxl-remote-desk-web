//! Pure projection from static Provider descriptors plus connection-scoped
//! readiness into discoverable/callable capability inventory.

use std::{collections::BTreeMap, fmt};

use desk_agent_protocol::capability_provider::{
    CAPABILITY_PROVIDER_SCHEMA_VERSION, CapabilityBlockedReason, CapabilityInventoryEntry,
    CapabilityInventorySnapshot, CapabilityReadinessReport, ExecutionLocality, ProductSurface,
};

use crate::{provider_registry::ProviderRegistry, registry::RegisteredTool};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityAvailability {
    pub provider_id: String,
    pub capability_id: String,
    pub tool_name: String,
    pub compiled: bool,
    pub enabled: bool,
    pub connected: bool,
    pub ready: bool,
    pub reason: Option<CapabilityBlockedReason>,
}

impl CapabilityAvailability {
    pub fn callable(&self) -> bool {
        self.compiled && self.enabled && self.connected && self.ready && self.reason.is_none()
    }
}

/// Build the stable inventory for one product surface. Central-only compiled
/// capabilities are ready by construction. Every edge capability requires a
/// fresh, exact readiness report from the currently live connection.
pub fn project_capability_availability(
    registry: &ProviderRegistry,
    surface: ProductSurface,
    now_unix_ms: u64,
    readiness: impl IntoIterator<Item = CapabilityReadinessReport>,
) -> Result<Vec<CapabilityAvailability>, AvailabilityError> {
    let mut reports = BTreeMap::new();
    for report in readiness {
        report
            .validate()
            .map_err(|_| AvailabilityError::InvalidReadiness(report.capability_id.clone()))?;
        let capability = registry
            .capability(&report.capability_id)
            .ok_or_else(|| AvailabilityError::UnknownCapability(report.capability_id.clone()))?;
        let expected_provider = registry
            .provider_for_capability(&report.capability_id)
            .expect("known capability has a provider");
        if expected_provider.wire.provider_id != report.provider_id {
            return Err(AvailabilityError::ProviderMismatch {
                capability_id: report.capability_id,
                expected: expected_provider.wire.provider_id.clone(),
                actual: report.provider_id,
            });
        }
        if capability.wire.execution_locality == ExecutionLocality::Central {
            return Err(AvailabilityError::UnexpectedCentralReadiness(
                capability.wire.capability_id.clone(),
            ));
        }
        let Some(adapter_id) = report.adapter_id.as_deref() else {
            return Err(AvailabilityError::AdapterMismatch(
                capability.wire.capability_id.clone(),
            ));
        };
        if !capability
            .adapter_ids
            .iter()
            .any(|known| known == adapter_id)
        {
            return Err(AvailabilityError::AdapterMismatch(
                capability.wire.capability_id.clone(),
            ));
        }
        let key = report.capability_id.clone();
        if reports.insert(key.clone(), report).is_some() {
            return Err(AvailabilityError::DuplicateReadiness(key));
        }
    }

    let mut inventory = Vec::new();
    for provider in registry.providers() {
        for capability in &provider.capabilities {
            if !capability.wire.surfaces.contains(&surface) {
                continue;
            }
            let availability = if capability.wire.execution_locality == ExecutionLocality::Central {
                CapabilityAvailability {
                    provider_id: provider.wire.provider_id.clone(),
                    capability_id: capability.wire.capability_id.clone(),
                    tool_name: capability.tool_spec.name.clone(),
                    compiled: true,
                    enabled: true,
                    connected: true,
                    ready: true,
                    reason: None,
                }
            } else if let Some(report) = reports.remove(&capability.wire.capability_id) {
                if report.expires_at_unix_ms <= now_unix_ms {
                    unavailable(
                        provider,
                        capability,
                        CapabilityBlockedReason::EdgeDisconnected,
                    )
                } else {
                    CapabilityAvailability {
                        provider_id: provider.wire.provider_id.clone(),
                        capability_id: capability.wire.capability_id.clone(),
                        tool_name: capability.tool_spec.name.clone(),
                        compiled: report.compiled,
                        enabled: report.enabled,
                        connected: report.connected,
                        ready: report.ready,
                        reason: report.reason,
                    }
                }
            } else {
                unavailable(
                    provider,
                    capability,
                    CapabilityBlockedReason::EdgeDisconnected,
                )
            };
            inventory.push(availability);
        }
    }
    debug_assert!(
        reports.is_empty(),
        "all known readiness reports were projected"
    );
    Ok(inventory)
}

fn unavailable(
    provider: &crate::provider_registry::ProviderDescriptor,
    capability: &crate::provider_registry::CapabilityDescriptor,
    reason: CapabilityBlockedReason,
) -> CapabilityAvailability {
    CapabilityAvailability {
        provider_id: provider.wire.provider_id.clone(),
        capability_id: capability.wire.capability_id.clone(),
        tool_name: capability.tool_spec.name.clone(),
        compiled: true,
        enabled: true,
        connected: false,
        ready: false,
        reason: Some(reason),
    }
}

pub fn callable_tools(
    registry: &ProviderRegistry,
    inventory: &[CapabilityAvailability],
) -> Result<Vec<RegisteredTool>, AvailabilityError> {
    let availability = inventory
        .iter()
        .map(|item| (item.capability_id.as_str(), item))
        .collect::<BTreeMap<_, _>>();
    let mut tools = Vec::new();
    for provider in registry.providers() {
        for capability in &provider.capabilities {
            let item = availability
                .get(capability.wire.capability_id.as_str())
                .ok_or_else(|| {
                    AvailabilityError::MissingAvailability(capability.wire.capability_id.clone())
                })?;
            if item.callable() {
                tools.push(capability.registered_tool());
            }
        }
    }
    tools.sort_by(|left, right| left.name().cmp(right.name()));
    Ok(tools)
}

pub fn inventory_snapshot(
    registry: &ProviderRegistry,
    surface: ProductSurface,
    generated_at_unix_ms: u64,
    inventory: &[CapabilityAvailability],
) -> Result<CapabilityInventorySnapshot, AvailabilityError> {
    let mut entries = Vec::with_capacity(inventory.len());
    for item in inventory {
        let provider = registry
            .provider(&item.provider_id)
            .ok_or_else(|| AvailabilityError::UnknownCapability(item.capability_id.clone()))?;
        let capability = provider
            .capabilities
            .iter()
            .find(|capability| capability.wire.capability_id == item.capability_id)
            .ok_or_else(|| AvailabilityError::UnknownCapability(item.capability_id.clone()))?;
        entries.push(CapabilityInventoryEntry {
            provider_id: provider.wire.provider_id.clone(),
            provider_display_name_key: provider.wire.display_name_key.clone(),
            provider_version: provider.wire.provider_version,
            capability: capability.wire.clone(),
            compiled: item.compiled,
            enabled: item.enabled,
            connected: item.connected,
            ready: item.ready,
            reason: item.reason,
        });
    }
    let snapshot = CapabilityInventorySnapshot {
        schema_version: CAPABILITY_PROVIDER_SCHEMA_VERSION,
        surface,
        generated_at_unix_ms,
        entries,
    };
    snapshot
        .validate()
        .map_err(|_| AvailabilityError::InvalidInventory)?;
    Ok(snapshot)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AvailabilityError {
    InvalidReadiness(String),
    UnknownCapability(String),
    DuplicateReadiness(String),
    UnexpectedCentralReadiness(String),
    AdapterMismatch(String),
    ProviderMismatch {
        capability_id: String,
        expected: String,
        actual: String,
    },
    MissingAvailability(String),
    InvalidInventory,
}

impl fmt::Display for AvailabilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidReadiness(capability) => {
                write!(f, "invalid readiness for capability {capability}")
            }
            Self::UnknownCapability(capability) => {
                write!(f, "readiness references unknown capability {capability}")
            }
            Self::DuplicateReadiness(capability) => {
                write!(f, "duplicate readiness for capability {capability}")
            }
            Self::UnexpectedCentralReadiness(capability) => {
                write!(
                    f,
                    "central capability {capability} cannot accept edge readiness"
                )
            }
            Self::AdapterMismatch(capability) => {
                write!(
                    f,
                    "readiness adapter does not match capability {capability}"
                )
            }
            Self::ProviderMismatch {
                capability_id,
                expected,
                actual,
            } => write!(
                f,
                "capability {capability_id} belongs to {expected}, not {actual}"
            ),
            Self::MissingAvailability(capability) => {
                write!(
                    f,
                    "missing projected availability for capability {capability}"
                )
            }
            Self::InvalidInventory => f.write_str("projected capability inventory is invalid"),
        }
    }
}

impl std::error::Error for AvailabilityError {}

#[cfg(test)]
mod tests {
    use desk_agent_protocol::capability_provider::{
        CAPABILITY_PROVIDER_SCHEMA_VERSION, CapabilityReadinessReport,
    };

    use super::*;
    use crate::device_assistant::{
        ACTION_PREVIEW_CAPABILITY_ID, DESKTOP_UI_CAPABILITY_ID, DESKTOP_UI_PROVIDER_ID,
        device_assistant_provider_registry,
    };

    fn ready_ui() -> CapabilityReadinessReport {
        CapabilityReadinessReport {
            schema_version: CAPABILITY_PROVIDER_SCHEMA_VERSION,
            provider_id: DESKTOP_UI_PROVIDER_ID.into(),
            capability_id: DESKTOP_UI_CAPABILITY_ID.into(),
            adapter_id: Some("windows.uia".into()),
            adapter_version: Some("v1".into()),
            revision: 1,
            observed_at_unix_ms: 1_000,
            expires_at_unix_ms: 2_000,
            local_ceiling_revision: 1,
            compiled: true,
            enabled: true,
            connected: true,
            ready: true,
            reason: None,
        }
    }

    #[test]
    fn missing_edge_readiness_is_discoverable_but_not_callable() {
        let registry = device_assistant_provider_registry();
        let inventory = project_capability_availability(
            &registry,
            ProductSurface::OssPersonalOwner,
            1_500,
            Vec::new(),
        )
        .unwrap();
        assert_eq!(inventory.len(), 16);
        assert!(inventory.iter().any(|item| {
            item.capability_id == DESKTOP_UI_CAPABILITY_ID
                && !item.callable()
                && item.reason == Some(CapabilityBlockedReason::EdgeDisconnected)
        }));
        let tools = callable_tools(&registry, &inventory).unwrap();
        assert_eq!(tools.len(), 3);
        assert!(tools.iter().any(|tool| {
            registry
                .capability_for_tool(tool.name())
                .is_some_and(|capability| {
                    capability.wire.capability_id == ACTION_PREVIEW_CAPABILITY_ID
                })
        }));
    }

    #[test]
    fn only_fresh_exact_edge_readiness_makes_tool_callable() {
        let registry = device_assistant_provider_registry();
        let inventory = project_capability_availability(
            &registry,
            ProductSurface::OssPersonalOwner,
            1_500,
            vec![ready_ui()],
        )
        .unwrap();
        let tools = callable_tools(&registry, &inventory).unwrap();
        assert!(tools.iter().any(|tool| tool.name() == "inspect_desktop_ui"));

        let expired = project_capability_availability(
            &registry,
            ProductSurface::OssPersonalOwner,
            2_000,
            vec![ready_ui()],
        )
        .unwrap();
        assert!(
            !callable_tools(&registry, &expired)
                .unwrap()
                .iter()
                .any(|tool| tool.name() == "inspect_desktop_ui")
        );
    }

    #[test]
    fn mismatched_adapter_fails_closed() {
        let registry = device_assistant_provider_registry();
        let mut report = ready_ui();
        report.adapter_id = Some("office.excel.addin".into());
        assert!(matches!(
            project_capability_availability(
                &registry,
                ProductSurface::OssPersonalOwner,
                1_500,
                vec![report]
            ),
            Err(AvailabilityError::AdapterMismatch(_))
        ));
    }
}
