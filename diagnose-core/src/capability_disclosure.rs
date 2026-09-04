//! Bounded progressive disclosure for first-party Provider capabilities.
//!
//! The durable state stores stable tool names only. Descriptors are projected
//! from the current registry and readiness immediately before a model request;
//! loading a name never grants authority or makes an unavailable tool callable.

use std::collections::BTreeSet;

use desk_agent_protocol::{AgentError, AgentErrorKind, Capability};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    capability_availability::CapabilityAvailability,
    chat::{ToolCall, ToolSpec},
    provider_registry::ProviderRegistry,
    registry::{RegisteredTool, ToolEffect},
};

pub const CAPABILITY_DISCLOSURE_SCHEMA_VERSION: u16 = 1;
pub const LOAD_CAPABILITY_DETAILS_TOOL_NAME: &str = "load_capability_details";
/// The current 47-name inventory serializes below 2 KiB. Leave bounded growth
/// room while making registry growth fail loudly rather than silently hiding a
/// capability.
pub const MAX_CAPABILITY_INDEX_BYTES: usize = 8 * 1024;
/// PR0 measured p95 descriptor size at 3,260 bytes and max at 8,104 bytes.
/// Eight p95 descriptors fit; twelve do not. Actual serialized bytes remain the
/// final authority, so an unusually large set is rejected as a whole.
pub const MAX_LOADED_CAPABILITY_COUNT: usize = 8;
pub const MAX_LOADED_CAPABILITY_DETAIL_BYTES: usize = 32 * 1024;
pub const MAX_REQUIRED_CAPABILITY_PIN_COUNT: usize = 16;
pub const MAX_ADVERTISED_PROVIDER_TOOL_BYTES: usize = 128 * 1024;
pub const DETAIL_CONTEXT_RATIO_DENOMINATOR: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityDisclosureState {
    pub schema_version: u16,
    pub focus_input_revision: u64,
    pub loaded_tool_names: Vec<String>,
    pub updated_input_revision: u64,
}

impl Default for CapabilityDisclosureState {
    fn default() -> Self {
        Self {
            schema_version: CAPABILITY_DISCLOSURE_SCHEMA_VERSION,
            focus_input_revision: 0,
            loaded_tool_names: Vec::new(),
            updated_input_revision: 0,
        }
    }
}

impl CapabilityDisclosureState {
    pub fn reset_for_input(&mut self, input_revision: u64) {
        self.schema_version = CAPABILITY_DISCLOSURE_SCHEMA_VERSION;
        self.focus_input_revision = input_revision;
        self.loaded_tool_names.clear();
        self.updated_input_revision = input_revision;
    }

    pub fn validate(&self, session_input_revision: u64) -> Result<(), &'static str> {
        if self.schema_version != CAPABILITY_DISCLOSURE_SCHEMA_VERSION {
            return Err("unsupported capability disclosure schema version");
        }
        if self.focus_input_revision > session_input_revision
            || self.updated_input_revision > session_input_revision
        {
            return Err("capability disclosure is ahead of input revision");
        }
        if self.loaded_tool_names.len() > MAX_LOADED_CAPABILITY_COUNT {
            return Err("too many loaded capability names");
        }
        let mut names = self.loaded_tool_names.clone();
        names.sort();
        names.dedup();
        if names != self.loaded_tool_names || names.iter().any(|name| name.trim().is_empty()) {
            return Err("capability disclosure names are not canonical");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityDisclosureProjection {
    pub index_prompt: String,
    pub detail_prompt: String,
    pub active_working_set: Vec<String>,
    pub index_utf8_bytes: usize,
    pub detail_utf8_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityDisclosureError {
    IndexTooLarge { actual: usize, maximum: usize },
    DetailTooLarge { actual: usize, maximum: usize },
    AdvertisedToolsTooLarge { actual: usize, maximum: usize },
    TooManyNames { actual: usize, maximum: usize },
    UnknownOrOutOfSurface(String),
}

fn invalid(error: CapabilityDisclosureError) -> AgentError {
    let message = match error {
        CapabilityDisclosureError::IndexTooLarge { actual, maximum } => {
            format!("capability name index is too large ({actual} > {maximum} bytes)")
        }
        CapabilityDisclosureError::DetailTooLarge { actual, maximum } => {
            format!("capability detail selection is too large ({actual} > {maximum} bytes)")
        }
        CapabilityDisclosureError::AdvertisedToolsTooLarge { actual, maximum } => {
            format!("advertised Provider tools are too large ({actual} > {maximum} bytes)")
        }
        CapabilityDisclosureError::TooManyNames { actual, maximum } => {
            format!("too many capability names ({actual} > {maximum})")
        }
        CapabilityDisclosureError::UnknownOrOutOfSurface(name) => {
            format!("unknown or unavailable capability name `{name}`")
        }
    };
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message,
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

fn current_names(inventory: &[CapabilityAvailability]) -> BTreeSet<&str> {
    inventory
        .iter()
        .map(|item| item.tool_name.as_str())
        .collect()
}

fn canonical_names(
    names: impl IntoIterator<Item = impl AsRef<str>>,
    registry: &ProviderRegistry,
    inventory: &[CapabilityAvailability],
    maximum: usize,
) -> Result<Vec<String>, CapabilityDisclosureError> {
    let current = current_names(inventory);
    let mut canonical = names
        .into_iter()
        .map(|name| name.as_ref().to_string())
        .collect::<Vec<_>>();
    canonical.sort();
    canonical.dedup();
    if canonical.len() > maximum {
        return Err(CapabilityDisclosureError::TooManyNames {
            actual: canonical.len(),
            maximum,
        });
    }
    if let Some(name) = canonical.iter().find(|name| {
        !current.contains(name.as_str()) || registry.capability_for_tool(name).is_none()
    }) {
        return Err(CapabilityDisclosureError::UnknownOrOutOfSurface(
            name.clone(),
        ));
    }
    Ok(canonical)
}

fn names_of(tools: &[RegisteredTool]) -> BTreeSet<&str> {
    tools.iter().map(|tool| tool.name()).collect()
}

/// Build a stable, names-only current-surface index. Free-form Provider/edge
/// diagnostic text is never included; unavailable reasons are closed enums.
pub fn capability_name_index_prompt(
    registry: &ProviderRegistry,
    inventory: &[CapabilityAvailability],
    callable_tools: &[RegisteredTool],
    permission_candidates: &[RegisteredTool],
    advertised_tool_names: &BTreeSet<&str>,
) -> Result<String, CapabilityDisclosureError> {
    let callable = names_of(callable_tools);
    let requestable = names_of(permission_candidates);
    let mut callable_when_loaded_now = Vec::new();
    let mut permission_requestable_when_loaded_now = Vec::new();
    let mut known_but_not_requestable_now = Vec::new();
    let mut unavailable_now = Vec::new();

    for provider in registry.providers() {
        for capability in &provider.capabilities {
            let name = capability.tool_spec.name.as_str();
            let Some(availability) = inventory.iter().find(|item| item.tool_name == name) else {
                continue;
            };
            if advertised_tool_names.contains(name) {
                continue;
            }
            if !availability.callable() {
                unavailable_now.push(name);
            } else if callable.contains(name) {
                callable_when_loaded_now.push(name);
            } else if requestable.contains(name) {
                permission_requestable_when_loaded_now.push(name);
            } else {
                known_but_not_requestable_now.push(name);
            }
        }
    }
    callable_when_loaded_now.sort_unstable();
    permission_requestable_when_loaded_now.sort_unstable();
    known_but_not_requestable_now.sort_unstable();
    unavailable_now.sort_unstable();
    let index = json!({
        "callable_when_loaded_now": callable_when_loaded_now,
        "permission_requestable_when_loaded_now": permission_requestable_when_loaded_now,
        "known_but_not_requestable_now": known_but_not_requestable_now,
        "unavailable_now": unavailable_now,
    });
    let prompt = format!(
        "This server-authored capability index contains names only. Tools already advertised by the model API are omitted. Use {LOAD_CAPABILITY_DETAILS_TOOL_NAME} with exact names to replace the current working set before planning a capability that is not already advertised. Loading grants no authority and cannot change readiness.\n<capability_index>{}</capability_index>",
        serde_json::to_string(&index).expect("name index is serializable")
    );
    if prompt.len() > MAX_CAPABILITY_INDEX_BYTES {
        return Err(CapabilityDisclosureError::IndexTooLarge {
            actual: prompt.len(),
            maximum: MAX_CAPABILITY_INDEX_BYTES,
        });
    }
    Ok(prompt)
}

fn detail_entries(
    registry: &ProviderRegistry,
    inventory: &[CapabilityAvailability],
    callable_tools: &[RegisteredTool],
    permission_candidates: &[RegisteredTool],
    advertised_tool_names: &BTreeSet<&str>,
    loaded_names: &[String],
) -> Vec<serde_json::Value> {
    let callable = names_of(callable_tools);
    let requestable = names_of(permission_candidates);
    loaded_names
        .iter()
        .filter_map(|name| {
            let capability = registry.capability_for_tool(name)?;
            let provider = registry.provider_for_capability(&capability.wire.capability_id)?;
            let availability = inventory.iter().find(|item| item.tool_name == *name)?;
            if advertised_tool_names.contains(name.as_str()) {
                return Some(json!({"tool_name": name, "state": "advertised_callable"}));
            }
            if availability.callable() && callable.contains(name.as_str()) {
                return Some(json!({"tool_name": name, "state": "callable_when_loaded"}));
            }
            if availability.callable() && requestable.contains(name.as_str()) {
                return Some(json!({
                    "provider_id": provider.wire.provider_id,
                    "capability_id": capability.wire.capability_id,
                    "tool_name": name,
                    "state": "permission_requestable",
                    "effect": capability.wire.effect,
                    "description": capability.tool_spec.description,
                    "input_schema": capability.tool_spec.parameters_schema,
                    "execution_policy": capability.wire.execution_policy,
                    "authorization_hint": capability.wire.authorization_hint,
                    "limits": capability.wire.limits,
                    "supports_progress": capability.wire.supports_progress,
                    "supports_cancel": capability.wire.supports_cancel,
                }));
            }
            if availability.callable() {
                Some(json!({"tool_name": name, "state": "not_requestable"}))
            } else {
                Some(json!({
                    "tool_name": name,
                    "state": "unavailable",
                    "blocked_reason_code": availability.reason,
                }))
            }
        })
        .collect()
}

fn detail_budget(max_context_bytes: usize) -> usize {
    MAX_LOADED_CAPABILITY_DETAIL_BYTES.min(max_context_bytes / DETAIL_CONTEXT_RATIO_DENOMINATOR)
}

fn validate_advertised_tool_bytes<'a>(
    tools: impl IntoIterator<Item = &'a RegisteredTool>,
) -> Result<(), CapabilityDisclosureError> {
    let specs = tools.into_iter().map(|tool| &tool.spec).collect::<Vec<_>>();
    let actual = serde_json::to_vec(&specs)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX);
    if actual > MAX_ADVERTISED_PROVIDER_TOOL_BYTES {
        return Err(CapabilityDisclosureError::AdvertisedToolsTooLarge {
            actual,
            maximum: MAX_ADVERTISED_PROVIDER_TOOL_BYTES,
        });
    }
    Ok(())
}

/// Project one model step. The loaded set uses replace semantics and the actual
/// serialized detail block is checked against the resolved model's budget.
pub fn project_capability_disclosure(
    registry: &ProviderRegistry,
    inventory: &[CapabilityAvailability],
    callable_tools: &[RegisteredTool],
    permission_candidates: &[RegisteredTool],
    required_pins: &[String],
    state: &CapabilityDisclosureState,
    max_context_bytes: usize,
) -> Result<CapabilityDisclosureProjection, CapabilityDisclosureError> {
    let loaded = canonical_names(
        state.loaded_tool_names.iter(),
        registry,
        inventory,
        MAX_LOADED_CAPABILITY_COUNT,
    )?;
    let pins = canonical_names(
        required_pins.iter(),
        registry,
        inventory,
        MAX_REQUIRED_CAPABILITY_PIN_COUNT,
    )?;
    let callable = names_of(callable_tools);
    let mut active_working_set = loaded
        .iter()
        .filter(|name| callable.contains(name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    active_working_set.extend(pins);
    active_working_set.sort();
    active_working_set.dedup();
    validate_advertised_tool_bytes(callable_tools.iter().filter(|tool| {
        active_working_set
            .binary_search_by(|name| name.as_str().cmp(tool.name()))
            .is_ok()
    }))?;
    let advertised = active_working_set
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let index_prompt = capability_name_index_prompt(
        registry,
        inventory,
        callable_tools,
        permission_candidates,
        &advertised,
    )?;
    let details = detail_entries(
        registry,
        inventory,
        callable_tools,
        permission_candidates,
        &advertised,
        &loaded,
    );
    let detail_prompt = if details.is_empty() {
        String::new()
    } else {
        format!(
            "<loaded_capability_details>{}</loaded_capability_details>",
            serde_json::to_string(&details).expect("capability details are serializable")
        )
    };
    let maximum = detail_budget(max_context_bytes);
    if detail_prompt.len() > maximum {
        return Err(CapabilityDisclosureError::DetailTooLarge {
            actual: detail_prompt.len(),
            maximum,
        });
    }
    Ok(CapabilityDisclosureProjection {
        index_utf8_bytes: index_prompt.len(),
        detail_utf8_bytes: detail_prompt.len(),
        index_prompt,
        detail_prompt,
        active_working_set,
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoadCapabilityDetailsInput {
    tool_names: Vec<String>,
}

pub fn capability_discovery_tool_registry() -> Vec<RegisteredTool> {
    vec![RegisteredTool {
        spec: ToolSpec {
            name: LOAD_CAPABILITY_DETAILS_TOOL_NAME.into(),
            description: "Replace the current focus epoch's bounded Provider capability working set with exact tool names from the server-authored capability index. This reveals current details on the next model step but grants no permission and executes nothing.".into(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "tool_names": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_LOADED_CAPABILITY_COUNT,
                        "items": {"type": "string", "maxLength": 128}
                    }
                },
                "required": ["tool_names"],
                "additionalProperties": false
            }),
        },
        required_capability: Capability::SystemInfo,
        effect: ToolEffect::CapabilityDiscovery,
    }]
}

/// Deterministic preload from server-owned structured context. Already
/// authorized callable tools come first, then permission candidates. No user
/// text or semantic classifier participates.
pub fn deterministic_preload_names(
    registry: &ProviderRegistry,
    callable_tools: &[RegisteredTool],
    permission_candidates: &[RegisteredTool],
) -> Vec<String> {
    let mut callable = callable_tools
        .iter()
        .filter(|tool| registry.capability_for_tool(tool.name()).is_some())
        .map(|tool| tool.name().to_string())
        .collect::<Vec<_>>();
    callable.sort();
    callable.dedup();

    let callable_set = callable.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let mut requestable = permission_candidates
        .iter()
        .filter(|tool| {
            registry.capability_for_tool(tool.name()).is_some()
                && !callable_set.contains(tool.name())
        })
        .map(|tool| tool.name().to_string())
        .collect::<Vec<_>>();
    requestable.sort();
    requestable.dedup();

    let mut names = callable;
    names.extend(requestable);
    names.truncate(MAX_LOADED_CAPABILITY_COUNT);
    // Durable state is canonical regardless of the priority used to select it.
    names.sort();
    names
}

pub struct CapabilityLoadContext<'a> {
    pub registry: &'a ProviderRegistry,
    pub inventory: &'a [CapabilityAvailability],
    pub max_context_bytes: usize,
    pub callable_tools: &'a [RegisteredTool],
    pub permission_candidates: &'a [RegisteredTool],
}

pub fn apply_load_call(
    call: &ToolCall,
    state: &mut CapabilityDisclosureState,
    input_revision: u64,
    context: &CapabilityLoadContext<'_>,
) -> Result<String, AgentError> {
    if call.name != LOAD_CAPABILITY_DETAILS_TOOL_NAME {
        return Err(invalid(CapabilityDisclosureError::UnknownOrOutOfSurface(
            call.name.clone(),
        )));
    }
    let input: LoadCapabilityDetailsInput =
        serde_json::from_str(&call.arguments_json).map_err(|_| {
            invalid(CapabilityDisclosureError::UnknownOrOutOfSurface(
                "invalid load request".into(),
            ))
        })?;
    if input.tool_names.is_empty() {
        return Err(invalid(CapabilityDisclosureError::TooManyNames {
            actual: 0,
            maximum: MAX_LOADED_CAPABILITY_COUNT,
        }));
    }
    let names = canonical_names(
        input.tool_names,
        context.registry,
        context.inventory,
        MAX_LOADED_CAPABILITY_COUNT,
    )
    .map_err(invalid)?;
    let candidate = CapabilityDisclosureState {
        schema_version: CAPABILITY_DISCLOSURE_SCHEMA_VERSION,
        focus_input_revision: input_revision,
        loaded_tool_names: names.clone(),
        updated_input_revision: input_revision,
    };
    project_capability_disclosure(
        context.registry,
        context.inventory,
        context.callable_tools,
        context.permission_candidates,
        &[],
        &candidate,
        context.max_context_bytes,
    )
    .map_err(invalid)?;
    *state = candidate;
    Ok(serde_json::to_string(&json!({"loaded": names})).expect("load receipt is serializable"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        capability_availability::CentralCapabilityReadiness,
        device_assistant::device_assistant_provider_registry,
    };
    use desk_agent_protocol::capability_provider::ProductSurface;

    fn all_ready() -> (
        ProviderRegistry,
        Vec<CapabilityAvailability>,
        Vec<RegisteredTool>,
    ) {
        let registry = device_assistant_provider_registry();
        let central = registry
            .providers()
            .flat_map(|provider| provider.capabilities.iter())
            .filter(|capability| {
                capability.wire.execution_locality
                    == desk_agent_protocol::capability_provider::ExecutionLocality::Central
            })
            .map(|capability| CentralCapabilityReadiness::ready(&capability.wire.capability_id))
            .collect::<Vec<_>>();
        let inventory = crate::capability_availability::project_capability_availability(
            &registry,
            ProductSurface::OssPersonalOwner,
            1,
            central,
            Vec::new(),
        )
        .unwrap();
        let callable =
            crate::capability_availability::callable_tools(&registry, &inventory).unwrap();
        (registry, inventory, callable)
    }

    fn every_capability_ready() -> (
        ProviderRegistry,
        Vec<CapabilityAvailability>,
        Vec<RegisteredTool>,
    ) {
        let registry = device_assistant_provider_registry();
        let inventory = registry
            .providers()
            .flat_map(|provider| {
                provider
                    .capabilities
                    .iter()
                    .map(|capability| CapabilityAvailability {
                        provider_id: provider.wire.provider_id.clone(),
                        capability_id: capability.wire.capability_id.clone(),
                        tool_name: capability.tool_spec.name.clone(),
                        compiled: true,
                        enabled: true,
                        connected: true,
                        ready: true,
                        reason: None,
                    })
            })
            .collect::<Vec<_>>();
        let callable = registry.registered_tools();
        (registry, inventory, callable)
    }

    #[test]
    fn index_is_stable_names_only_and_bounded() {
        let (registry, inventory, callable) = all_ready();
        let one =
            capability_name_index_prompt(&registry, &inventory, &callable, &[], &BTreeSet::new())
                .unwrap();
        let two =
            capability_name_index_prompt(&registry, &inventory, &callable, &[], &BTreeSet::new())
                .unwrap();
        assert_eq!(one, two);
        assert!(one.len() <= MAX_CAPABILITY_INDEX_BYTES);
        assert!(!one.contains("input_schema"));
        assert!(!one.contains("description"));
        assert_eq!(
            one,
            include_str!("../tests/fixtures/capability_name_index.txt").trim_end()
        );
    }

    #[test]
    fn load_replaces_deduplicates_and_rejects_unknown_names() {
        let (registry, inventory, callable) = all_ready();
        let name = callable[0].name().to_string();
        let mut state = CapabilityDisclosureState::default();
        let receipt = apply_load_call(
            &ToolCall {
                id: "load-1".into(),
                name: LOAD_CAPABILITY_DETAILS_TOOL_NAME.into(),
                arguments_json: serde_json::to_string(&json!({"tool_names": [name, name]}))
                    .unwrap(),
            },
            &mut state,
            7,
            &CapabilityLoadContext {
                registry: &registry,
                inventory: &inventory,
                max_context_bytes: 131_072,
                callable_tools: &callable,
                permission_candidates: &[],
            },
        )
        .unwrap();
        assert!(receipt.contains("loaded"));
        assert_eq!(state.loaded_tool_names.len(), 1);
        assert_eq!(state.focus_input_revision, 7);

        let before = state.clone();
        assert!(
            apply_load_call(
                &ToolCall {
                    id: "load-2".into(),
                    name: LOAD_CAPABILITY_DETAILS_TOOL_NAME.into(),
                    arguments_json: r#"{"tool_names":["invented_tool"]}"#.into(),
                },
                &mut state,
                7,
                &CapabilityLoadContext {
                    registry: &registry,
                    inventory: &inventory,
                    max_context_bytes: 131_072,
                    callable_tools: &callable,
                    permission_candidates: &[],
                },
            )
            .is_err()
        );
        assert_eq!(state, before);
    }

    #[test]
    fn count_detail_and_required_pin_limits_fail_closed() {
        let (registry, inventory, callable) = every_capability_ready();
        let names = callable
            .iter()
            .take(MAX_LOADED_CAPABILITY_COUNT + 1)
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();
        let mut state = CapabilityDisclosureState::default();
        let call = ToolCall {
            id: "too-many".into(),
            name: LOAD_CAPABILITY_DETAILS_TOOL_NAME.into(),
            arguments_json: serde_json::to_string(&json!({"tool_names": names})).unwrap(),
        };
        assert!(
            apply_load_call(
                &call,
                &mut state,
                1,
                &CapabilityLoadContext {
                    registry: &registry,
                    inventory: &inventory,
                    max_context_bytes: 131_072,
                    callable_tools: &[],
                    permission_candidates: &callable,
                },
            )
            .is_err()
        );
        assert!(state.loaded_tool_names.is_empty());

        let large = callable
            .iter()
            .max_by_key(|tool| serde_json::to_vec(&tool.spec).unwrap().len())
            .unwrap()
            .name()
            .to_string();
        state.loaded_tool_names = vec![large];
        state.focus_input_revision = 1;
        state.updated_input_revision = 1;
        assert!(matches!(
            project_capability_disclosure(
                &registry,
                &inventory,
                &[],
                &callable,
                &[],
                &state,
                crate::MIN_MODEL_CONTEXT_BYTES,
            ),
            Err(CapabilityDisclosureError::DetailTooLarge { .. })
        ));

        state.loaded_tool_names.clear();
        let pins = callable
            .iter()
            .take(MAX_REQUIRED_CAPABILITY_PIN_COUNT + 1)
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();
        assert!(matches!(
            project_capability_disclosure(
                &registry,
                &inventory,
                &callable,
                &[],
                &pins,
                &state,
                131_072,
            ),
            Err(CapabilityDisclosureError::TooManyNames { .. })
        ));
    }

    #[test]
    fn preload_selects_authorized_callable_tools_before_permission_candidates() {
        let (registry, _, callable) = every_capability_ready();
        let mut sorted = callable.clone();
        sorted.sort_by(|left, right| left.name().cmp(right.name()));
        let authorized = sorted.last().unwrap().clone();
        let candidates = sorted
            .iter()
            .take(MAX_LOADED_CAPABILITY_COUNT)
            .cloned()
            .collect::<Vec<_>>();

        let preload =
            deterministic_preload_names(&registry, std::slice::from_ref(&authorized), &candidates);

        assert_eq!(preload.len(), MAX_LOADED_CAPABILITY_COUNT);
        assert!(preload.iter().any(|name| name == authorized.name()));
    }

    #[test]
    fn advertised_provider_tool_byte_limit_fails_closed() {
        let oversized = RegisteredTool {
            spec: ToolSpec {
                name: "oversized".into(),
                description: "x".repeat(MAX_ADVERTISED_PROVIDER_TOOL_BYTES),
                parameters_schema: json!({"type": "object"}),
            },
            required_capability: Capability::SystemInfo,
            effect: ToolEffect::ReadOnly,
        };

        assert!(matches!(
            validate_advertised_tool_bytes([&oversized]),
            Err(CapabilityDisclosureError::AdvertisedToolsTooLarge { .. })
        ));
    }

    #[test]
    fn loaded_capability_downgrades_when_current_readiness_changes() {
        use desk_agent_protocol::capability_provider::CapabilityBlockedReason;

        let (registry, mut inventory, callable) = every_capability_ready();
        let loaded_name = callable[0].name().to_string();
        let mut state = CapabilityDisclosureState::default();
        state.reset_for_input(1);
        state.loaded_tool_names = vec![loaded_name.clone()];

        let availability = inventory
            .iter_mut()
            .find(|item| item.tool_name == loaded_name)
            .unwrap();
        availability.ready = false;
        availability.reason = Some(CapabilityBlockedReason::AdapterUnavailable);
        let current_callable = callable
            .iter()
            .filter(|tool| tool.name() != loaded_name)
            .cloned()
            .collect::<Vec<_>>();

        let projection = project_capability_disclosure(
            &registry,
            &inventory,
            &current_callable,
            &[],
            &[],
            &state,
            131_072,
        )
        .unwrap();

        assert!(projection.active_working_set.is_empty());
        assert!(
            projection
                .detail_prompt
                .contains("\"state\":\"unavailable\"")
        );
        assert!(projection.detail_prompt.contains("adapter_unavailable"));
        assert!(!projection.detail_prompt.contains("input_schema"));
        assert!(!projection.detail_prompt.contains("description"));
    }

    #[test]
    #[ignore = "measurement harness; run explicitly with --nocapture"]
    fn print_progressive_disclosure_baseline() {
        let (registry, inventory, callable) = every_capability_ready();
        let permission_candidates = callable
            .iter()
            .filter(|tool| tool.effect != ToolEffect::ReadOnly)
            .cloned()
            .collect::<Vec<_>>();
        let legacy =
            crate::permission_tools::discoverable_catalog_prompt_with_permission_candidates(
                &registry,
                &inventory,
                &callable,
                &permission_candidates,
            );
        let index = capability_name_index_prompt(
            &registry,
            &inventory,
            &callable,
            &permission_candidates,
            &BTreeSet::new(),
        )
        .unwrap();
        let entries = crate::permission_tools::capability_catalog_entries(
            &registry,
            &inventory,
            &callable,
            &permission_candidates,
        );
        let mut detail_sizes = entries
            .iter()
            .map(|entry| serde_json::to_vec(entry).unwrap().len())
            .collect::<Vec<_>>();
        detail_sizes.sort_unstable();
        let percentile = |percent: usize| detail_sizes[(detail_sizes.len() - 1) * percent / 100];
        let candidate_p95_bytes = [4, 8, 12, 16].map(|count| count * percentile(95));
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "registry_count": entries.len(),
                "runtime_ready_count": inventory.iter().filter(|item| item.callable()).count(),
                "legacy_catalog_utf8_bytes": legacy.len(),
                "name_index_utf8_bytes": index.len(),
                "detail_min_utf8_bytes": detail_sizes.first().copied().unwrap_or(0),
                "detail_p50_utf8_bytes": percentile(50),
                "detail_p95_utf8_bytes": percentile(95),
                "detail_max_utf8_bytes": detail_sizes.last().copied().unwrap_or(0),
                "candidate_counts": [4, 8, 12, 16],
                "candidate_p95_bytes": candidate_p95_bytes,
            }))
            .unwrap()
        );
    }
}
