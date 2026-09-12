//! Internal permission-planning control tool.
//!
//! The model may propose a bounded batch, but the server resolves every
//! provider/tool/effect against the compiled registry and only persists a
//! `PermissionRequest`. This module cannot create a grant or dispatch work.

use std::collections::BTreeSet;

use desk_agent_protocol::browser_control::{BrowserNavigationTarget, BrowserPageRef};
use desk_agent_protocol::capability_grant::CapabilityGrant;
use desk_agent_protocol::capability_provider::{AuthorizationResourceKind, CapabilityEffect};
use desk_agent_protocol::{AgentError, AgentErrorKind, Capability};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::capability_availability::CapabilityAvailability;
use crate::capability_grant::{
    canonical_compiled_scope, exact_external_url_resource_scope, fresh_object_resource_scope,
};
use crate::chat::{ToolCall, ToolSpec};
use crate::dynamic_run::{
    GrantRequestItem, MAX_PERMISSION_REASON_BYTES, MAX_PERMISSION_REQUEST_ITEMS,
    MAX_PERMISSION_SCOPE_VALUES, PERMISSION_REQUEST_SCHEMA_VERSION, PermissionRequest,
    PermissionRequestState,
};
use crate::provider_registry::ProviderRegistry;
use crate::registry::{RegisteredTool, ToolEffect};

pub const REQUEST_CAPABILITY_GRANTS_TOOL_NAME: &str = "request_capability_grants";
pub const MAX_REQUEST_TTL_SECONDS: u32 = 3_600;
pub const MAX_REQUEST_USES: u32 = 16;
pub const MAX_CAPABILITY_CATALOG_PROMPT_BYTES: usize = 16 * 1024;

/// Content-free baseline measurements for the current capability projection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CapabilityCatalogMetrics {
    pub registry_count: u64,
    pub runtime_ready_count: u64,
    pub callable_count: u64,
    pub permission_candidate_count: u64,
    pub catalog_utf8_bytes: u64,
    pub detail_min_utf8_bytes: u64,
    pub detail_p50_utf8_bytes: u64,
    pub detail_p95_utf8_bytes: u64,
    pub detail_max_utf8_bytes: u64,
}

/// Render the current server-owned authorization projection for the model.
///
/// Permission decisions are durable run events rather than conversation text,
/// so an older assistant message may still say that a request was pending.
/// Re-projecting grants on every turn gives the model current authority facts
/// without exposing grant ids or trusting model-maintained history. Actual
/// dispatch still has to pass the grant matcher and transactional reservation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityAuthorizationPrompt {
    pub text: String,
    /// Earliest expiry among exact inputs included in `text`. The caller uses
    /// this to put the whole prompt behind the same model-egress boundary.
    pub approved_exact_input_expires_at_unix_ms: Option<u64>,
}

pub fn capability_authorization_prompt(
    grants: &[CapabilityGrant],
    permission_requests: &[PermissionRequest],
    now_unix_ms: u64,
    _current_input_revision: u64,
    current_readiness_revision: u64,
) -> CapabilityAuthorizationPrompt {
    let mut approved_exact_input_expires_at_unix_ms: Option<u64> = None;
    let mut entries = Vec::with_capacity(grants.len());
    // Approved scope remains valid across ordinary conversation inputs.
    for grant in grants {
        let mut state = if grant.revoked_at_unix_ms.is_some() {
            "revoked"
        } else if grant.expires_at_unix_ms <= now_unix_ms {
            "expired"
        } else if grant.remaining_uses == 0 {
            "exhausted"
        } else if grant.readiness_revision != current_readiness_revision {
            "stale_readiness"
        } else {
            "active"
        };
        let approved_exact = (state == "active")
            .then(|| approved_exact_input(grant, permission_requests))
            .flatten();
        if state == "active"
            && grant.canonical_input_digest_sha256.is_some()
            && approved_exact.is_none()
        {
            // An exact grant without a recoverable current-schema input cannot
            // be called safely. Keep it out of the model's active authority
            // set instead of encouraging the model to reconstruct old wire
            // shapes or guess fields from a digest.
            state = "schema_incompatible";
        }
        if state != "active" {
            let existing = entries.iter_mut().find(|entry: &&mut serde_json::Value| {
                entry["tool_name"] == grant.tool_name && entry["state"] == state
            });
            if let Some(entry) = existing {
                entry["grant_count"] = json!(entry["grant_count"].as_u64().unwrap_or(0) + 1);
            } else {
                entries
                    .push(json!({"tool_name": grant.tool_name, "state": state, "grant_count": 1}));
            }
            continue;
        }
        let mut entry = json!({
            "provider_id": grant.provider_id,
            "capability_id": grant.capability_id,
            "tool_name": grant.tool_name,
            "effect": grant.effect,
            "risk_tier": grant.risk_tier,
            "state": state,
            "remaining_uses": grant.remaining_uses,
            "expires_at_unix_ms": grant.expires_at_unix_ms,
            "resource_scope": grant.resource_scope,
            "operation_scope": grant.operation_scope,
        });
        if grant.canonical_input_digest_sha256.is_none() {
            if let Some(mut scope) = permission_requests
                .iter()
                .filter(|r| {
                    matches!(
                        r.state,
                        PermissionRequestState::Approved
                            | PermissionRequestState::PartiallyApproved
                    )
                })
                .flat_map(|r| &r.items)
                .filter(|item| {
                    item.tool_name == grant.tool_name && item.resource_scope == grant.resource_scope
                })
                .find_map(|item| {
                    crate::application_ui::from_canonical(
                        &item.tool_name,
                        item.canonical_input_json.as_deref(),
                    )
                })
            {
                scope.actions.retain(|action| {
                    grant
                        .operation_scope
                        .contains(&crate::application_ui::operation_kind(*action))
                });
                entry["application_scope"] = json!(scope);
                approved_exact_input_expires_at_unix_ms = Some(
                    approved_exact_input_expires_at_unix_ms.map_or(grant.expires_at_unix_ms, |v| {
                        v.min(grant.expires_at_unix_ms)
                    }),
                );
            }
        }
        if state == "active"
            && let Some((canonical_input, digest)) = approved_exact
        {
            entry["canonical_input_digest_sha256"] = json!(digest);
            entry["approved_exact_input"] = canonical_input;
            approved_exact_input_expires_at_unix_ms = Some(
                approved_exact_input_expires_at_unix_ms
                    .map_or(grant.expires_at_unix_ms, |current| {
                        current.min(grant.expires_at_unix_ms)
                    }),
            );
        }
        entries.push(entry);
    }
    CapabilityAuthorizationPrompt {
        text: format!(
            "The following JSON authorization snapshot is server-authored for this run and supersedes any older assistant statement that a permission request is still pending. It does not widen the current tool list and does not itself dispatch anything. When a tool is present in the current tool list and has state=active here, do not refuse it based on stale permission text in conversation history; call it when the user requested it and let the server authorizer perform the final match. For any active grant bound to an exact input, approved_exact_input is the immutable server-canonicalized JSON the owner approved: use it as that tool's arguments without adding, removing, or changing any field, never repeat it in prose, and never reuse it beyond remaining_uses. Exact input is deliberately omitted for every non-active or non-exact grant. For an active application_scope, use its application_id in the approved UI or background-input tool, use current observed target IDs and an approved action; do not request another exact permission for each control within that scope. Never invent or reveal a grant id.\n<capability_authorization>{}</capability_authorization>",
            serde_json::to_string(&entries).expect("authorization projection is serializable")
        ),
        approved_exact_input_expires_at_unix_ms,
    }
}

/// Exact Provider tools whose approved canonical input can be recovered for a
/// permission-decision continuation right now. The orchestration loop uses
/// this server-owned set to temporarily hide research / preview tools until
/// the model proposes the already-approved mutation, preventing a fresh
/// observation from invalidating an ephemeral object reference before use.
pub fn active_exact_authorized_tool_names(
    grants: &[CapabilityGrant],
    permission_requests: &[PermissionRequest],
    now_unix_ms: u64,
    _current_input_revision: u64,
    current_readiness_revision: u64,
) -> Vec<String> {
    grants
        .iter()
        .filter(|grant| {
            grant.revoked_at_unix_ms.is_none()
                && grant.expires_at_unix_ms > now_unix_ms
                && grant.remaining_uses > 0
                && grant.readiness_revision == current_readiness_revision
                && grant.canonical_input_digest_sha256.is_some()
                && approved_exact_input(grant, permission_requests).is_some()
        })
        .map(|grant| grant.tool_name.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn approved_exact_input(
    grant: &CapabilityGrant,
    permission_requests: &[PermissionRequest],
) -> Option<(serde_json::Value, String)> {
    let digest = grant.canonical_input_digest_sha256.as_deref()?;
    permission_requests
        .iter()
        .filter(|request| {
            request.input_revision == grant.input_revision
                && matches!(
                    request.state,
                    PermissionRequestState::Approved | PermissionRequestState::PartiallyApproved
                )
        })
        .flat_map(|request| &request.items)
        .find_map(|item| {
            if item.provider_id != grant.provider_id
                || item.tool_name != grant.tool_name
                || item.canonical_input_digest_sha256.as_deref() != Some(digest)
            {
                return None;
            }
            let canonical = item.canonical_input_json.as_deref()?;
            (format!("{:x}", Sha256::digest(canonical.as_bytes())) == digest)
                .then(|| serde_json::from_str(canonical).ok())
                .flatten()
                .filter(|value| exact_input_matches_current_contract(&grant.tool_name, value))
                .map(|value| (value, digest.to_string()))
        })
}

fn exact_input_matches_current_contract(tool_name: &str, value: &serde_json::Value) -> bool {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct BrowserOpenInput {
        target: BrowserNavigationTarget,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct BrowserNavigateInput {
        page: BrowserPageRef,
        target: BrowserNavigationTarget,
    }

    match tool_name {
        crate::device_assistant::EXECUTE_CONFIRMED_UI_ACTION_TOOL
        | crate::device_assistant::EXECUTE_BACKGROUND_INPUT_TOOL => false,
        "browser_open_page" => serde_json::from_value::<BrowserOpenInput>(value.clone())
            .is_ok_and(|input| input.target.validate().is_ok()),
        "browser_navigate_page" => serde_json::from_value::<BrowserNavigateInput>(value.clone())
            .is_ok_and(|input| input.page.validate().is_ok() && input.target.validate().is_ok()),
        _ => true,
    }
}

/// Build the exact server-owned capability catalog shown to the model. The
/// compiled descriptor supplies identity/schema/effect while the live inventory
/// supplies edge readiness. `callable_now` is derived from the final per-turn
/// model registry, so an installed-but-unselected capability is never presented
/// as directly callable.
#[cfg(test)]
pub fn discoverable_catalog_prompt(
    registry: &ProviderRegistry,
    inventory: &[CapabilityAvailability],
    callable_tools: &[RegisteredTool],
) -> String {
    discoverable_catalog_prompt_with_permission_candidates(registry, inventory, callable_tools, &[])
}

/// Build the capability catalog while distinguishing tools that are currently
/// callable from tools whose already-selected context permits a bounded owner
/// permission request. Runtime readiness alone is deliberately insufficient:
/// an installed Provider outside the current context must not be bundled into
/// a speculative permission request.
#[cfg(test)]
pub fn discoverable_catalog_prompt_with_permission_candidates(
    registry: &ProviderRegistry,
    inventory: &[CapabilityAvailability],
    callable_tools: &[RegisteredTool],
    permission_candidates: &[RegisteredTool],
) -> String {
    let entries =
        capability_catalog_entries(registry, inventory, callable_tools, permission_candidates);
    format!(
        "The following JSON capability catalog is server-authored. Treat it as authority for what is compiled, runtime-ready, callable, and permission-requestable in this turn. Never invent provider ids, tool names, effects, scopes, or readiness. A capability with runtime_ready=false cannot be made usable by asking for permission. A Provider tool with callable_now=false cannot be invoked in this turn. Include a tool in request_capability_grants only when permission_requestable_now=true; false means its current context or other trusted prerequisites are absent. The permission request does not execute or widen the current tool list.\n<capability_catalog>{}</capability_catalog>",
        serde_json::to_string(&entries).expect("catalog contains only serializable descriptors")
    )
}

pub(crate) fn capability_catalog_entries(
    registry: &ProviderRegistry,
    inventory: &[CapabilityAvailability],
    callable_tools: &[RegisteredTool],
    permission_candidates: &[RegisteredTool],
) -> Vec<serde_json::Value> {
    let callable = callable_tools
        .iter()
        .map(|tool| tool.name())
        .collect::<BTreeSet<_>>();
    let permission_requestable = permission_candidates
        .iter()
        .map(|tool| tool.name())
        .collect::<BTreeSet<_>>();
    registry
        .providers()
        .flat_map(|provider| {
            provider.capabilities.iter().map(|capability| {
                let availability = inventory.iter().find(|item| {
                    item.provider_id == provider.wire.provider_id
                        && item.capability_id == capability.wire.capability_id
                });
                let runtime_ready = availability.is_some_and(CapabilityAvailability::callable);
                let callable_now =
                    runtime_ready && callable.contains(capability.tool_spec.name.as_str());
                let mut entry = if runtime_ready {
                    json!({
                        "provider_id": provider.wire.provider_id,
                        "capability_id": capability.wire.capability_id,
                        "tool_name": capability.tool_spec.name,
                        "effect": capability.wire.effect,
                        "execution_locality": capability.wire.execution_locality,
                        "runtime_ready": true,
                        "callable_now": callable_now,
                        "permission_requestable_now": !callable_now
                            && permission_requestable.contains(capability.tool_spec.name.as_str()),
                        "blocked_reason": null,
                    })
                } else {
                    // An unavailable capability cannot be called or requested.
                    // Keep only the identity and reason; detailed policy/schema
                    // is loaded on demand after readiness changes.
                    json!({
                        "provider_id": provider.wire.provider_id,
                        "capability_id": capability.wire.capability_id,
                        "tool_name": capability.tool_spec.name,
                        "runtime_ready": false,
                        "blocked_reason": availability.and_then(|item| item.reason),
                    })
                };
                if runtime_ready {
                    let object = entry
                        .as_object_mut()
                        .expect("catalog entry is a JSON object");
                    object.insert(
                        "execution_policy".into(),
                        json!(capability.wire.execution_policy),
                    );
                    object.insert(
                        "supports_progress".into(),
                        json!(capability.wire.supports_progress),
                    );
                    object.insert(
                        "supports_cancel".into(),
                        json!(capability.wire.supports_cancel),
                    );
                    let mut model_spec = capability.tool_spec.clone();
                    crate::ui_model_ids::project_tool(&mut model_spec);
                    object.insert("description".into(), json!(model_spec.description));
                    object.insert("input_schema".into(), model_spec.parameters_schema);
                }
                entry
            })
        })
        .collect()
}

/// Measure the exact current catalog serializer without retaining descriptor or
/// tool content in the returned value.
pub fn capability_catalog_metrics(
    registry: &ProviderRegistry,
    inventory: &[CapabilityAvailability],
    callable_tools: &[RegisteredTool],
    permission_candidates: &[RegisteredTool],
) -> CapabilityCatalogMetrics {
    let entries =
        capability_catalog_entries(registry, inventory, callable_tools, permission_candidates);
    let mut detail_sizes = entries
        .iter()
        .filter(|entry| {
            entry
                .get("runtime_ready")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
        .map(|entry| {
            u64::try_from(
                serde_json::to_vec(entry)
                    .expect("capability detail is serializable")
                    .len(),
            )
            .unwrap_or(u64::MAX)
        })
        .collect::<Vec<_>>();
    detail_sizes.sort_unstable();
    let percentile = |numerator: usize, denominator: usize| {
        if detail_sizes.is_empty() {
            return 0;
        }
        let last = detail_sizes.len() - 1;
        detail_sizes[last.saturating_mul(numerator) / denominator]
    };
    CapabilityCatalogMetrics {
        registry_count: u64::try_from(entries.len()).unwrap_or(u64::MAX),
        runtime_ready_count: u64::try_from(detail_sizes.len()).unwrap_or(u64::MAX),
        callable_count: u64::try_from(callable_tools.len()).unwrap_or(u64::MAX),
        permission_candidate_count: u64::try_from(permission_candidates.len()).unwrap_or(u64::MAX),
        // The production prompt no longer contains the legacy full catalog.
        catalog_utf8_bytes: 0,
        detail_min_utf8_bytes: detail_sizes.first().copied().unwrap_or(0),
        detail_p50_utf8_bytes: percentile(50, 100),
        detail_p95_utf8_bytes: percentile(95, 100),
        detail_max_utf8_bytes: detail_sizes.last().copied().unwrap_or(0),
    }
}

/// Permission requests must be grounded in the same fresh edge readiness used
/// for discovery. Compiled-but-disconnected Office/UI/file adapters cannot
/// produce a user approval prompt that would be impossible to honor.
pub fn validate_request_availability(
    request: &PermissionRequest,
    inventory: &[CapabilityAvailability],
    callable_tools: &[RegisteredTool],
) -> Result<(), AgentError> {
    for item in &request.items {
        let available = inventory.iter().find(|availability| {
            availability.provider_id == item.provider_id && availability.tool_name == item.tool_name
        });
        if !available.is_some_and(CapabilityAvailability::callable) {
            return Err(invalid(format!(
                "tool `{}` is not runtime-ready on the target",
                item.tool_name
            )));
        }
        if !callable_tools
            .iter()
            .any(|tool| tool.name() == item.tool_name)
        {
            return Err(invalid(format!(
                "tool `{}` is not callable in this turn; select its required context first",
                item.tool_name
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestParams {
    items: Vec<RequestItem>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestItem {
    item_id: String,
    #[serde(default)]
    provider_id: Option<String>,
    tool_name: String,
    #[serde(default)]
    expected_effect: Option<CapabilityEffect>,
    #[serde(default)]
    resource_scope: Vec<String>,
    #[serde(default)]
    operation_scope: Vec<String>,
    #[serde(default)]
    export_destinations: Vec<desk_agent_protocol::data_lineage::DestinationIdentity>,
    #[serde(default)]
    exact_input: Option<serde_json::Value>,
    #[serde(default)]
    application_scope: Option<desk_agent_protocol::computer_use::UiApplicationScope>,
    suggested_ttl_seconds: u32,
    suggested_max_uses: u32,
    reason: String,
}

fn invalid(detail: impl std::fmt::Display) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: format!(
            "invalid request_capability_grants arguments: {detail}. The entire batch was rejected: no request or approval card was created, including otherwise valid items. Do not repeat unchanged arguments or tell the user a request exists. Fix the invalid item; if its target is not yet known, first request only the prerequisite read permission, inspect the target, then request native UI actions with application_scope, or other mutations with exact_input when required by their tool contract."
        ),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

/// Canonical JSON representation shared by permission planning and the runtime
/// call authorizer. Object member order is not semantically meaningful, so an
/// approved exact input must continue to match when a model serializes the same
/// object with a different key order. Arrays remain ordered and scalar values
/// remain unchanged.
pub fn canonical_permission_input_json(
    value: serde_json::Value,
) -> Result<String, serde_json::Error> {
    fn sort_json(value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Array(values) => {
                serde_json::Value::Array(values.into_iter().map(sort_json).collect())
            }
            serde_json::Value::Object(values) => {
                let mut entries = values.into_iter().collect::<Vec<_>>();
                entries.sort_by(|left, right| left.0.cmp(&right.0));
                serde_json::Value::Object(
                    entries
                        .into_iter()
                        .map(|(key, value)| (key, sort_json(value)))
                        .collect(),
                )
            }
            scalar => scalar,
        }
    }

    serde_json::to_string(&sort_json(value))
}

/// Canonicalize semantically equivalent defaults and server-resolved references
/// for exact-grant matching. Native reference validity is checked at dispatch.
/// Keep this list closed: adding an entry changes exact-grant matching and must
/// be backed by a matching runtime default and regression test.
pub fn canonical_tool_permission_input_json(
    tool_name: &str,
    mut value: serde_json::Value,
) -> Result<String, serde_json::Error> {
    crate::model_input::fill_versions(tool_name, &mut value);
    if tool_name == "search_public_web"
        && let serde_json::Value::Object(input) = &mut value
    {
        input
            .entry("max_results".to_string())
            .or_insert_with(|| serde_json::json!(5));
    }
    if tool_name == "read_current_screen"
        && let Some(input) = value.as_object_mut()
        && !input.contains_key("window_id")
        && let Some(window) = input.get("window").and_then(serde_json::Value::as_object)
        && window.len() == 4
        && let Ok(reference) = serde_json::from_value::<desk_agent_protocol::computer_use::ObjectRef>(
            serde_json::Value::Object(window.clone()),
        )
        && reference.object_kind == desk_agent_protocol::computer_use::ObjectKind::Window
        && !reference.token.is_empty()
    {
        // The model approves an observed ID; the server later expands it into
        // a native reference. Bind permission to the same ID on both paths.
        // Do not collapse conflicting selectors, wrong kinds or extra fields.
        input.remove("window");
        input.insert(
            "window_id".into(),
            serde_json::Value::String(reference.token),
        );
    }
    canonical_permission_input_json(value)
}

/// The placeholder capability is ignored by PermissionPlanning exposure. It
/// exists only because RegisteredTool deliberately requires a closed capability.
pub fn permission_planning_tool_registry() -> Vec<RegisteredTool> {
    vec![RegisteredTool {
        spec: ToolSpec {
            name: REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),
            description: "Create one bounded approval request by actually calling this tool. First load missing capability details with load_capability_details; loading alone creates no request. Only report an approval card as submitted after a successful result contains request_id and status=pending_user_decision. An error creates no card; correct the input and call again. Never invent a submitted request or tell the user to refresh to find one without a successful receipt. Identify capabilities by tool_name only; the server derives provider_id and effect, so do not supply them. This only creates a pending request: it does not grant, reserve, invoke, or retry any tool. Desktop UI and raw-input action batches automatically include separately reviewable desktop session and UI reads (up to 16 reads each, same requested duration, no screenshots). Leave two slots for these reads: at most 14 action items unless both reads are already included. Prefer one batch for all currently-known inputs, then request another only when intermediate results provide new exact inputs. Never supply an export destination: every destination is derived and fixed by the registered Provider on the server.".into(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_PERMISSION_REQUEST_ITEMS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "item_id": {"type": "string", "maxLength": 128},
                                "tool_name": {"type": "string", "maxLength": 128},
                                "resource_scope": {"type": "array", "maxItems": MAX_PERMISSION_SCOPE_VALUES, "items": {"type": "string", "maxLength": 512}},
                                "operation_scope": {"type": "array", "maxItems": MAX_PERMISSION_SCOPE_VALUES, "items": {"type": "string", "maxLength": 512}},
                                "application_scope": {"type":"object","description":"Required for every native UI permission in this conversation. Do not supply exact_input. Copy an observed application reference; approved actions are limited to this application and the owner-selected expiry/use count. The server resolves the application name. Actual calls pass application plus the current target and action.","properties":{"application":{"type":"object","properties":{"token":{"type":"string"},"snapshot_id":{"type":"string"},"object_kind":{"const":"application"},"expires_at":{"type":"string"}},"required":["token","snapshot_id","object_kind","expires_at"],"additionalProperties":false},"actions":{"type":"array","minItems":1,"maxItems":5,"uniqueItems":true,"items":{"type":"string","enum":["invoke","select","focus","toggle","set_value","click","double_click","scroll","type_text","key_press"]}}},"required":["application","actions"],"additionalProperties":false},
                                "exact_input": {"type": "object", "description": "First load the target tool with load_capability_details and copy its complete input shape. Do not supply fixed schema_version fields; the server supplies them. Required for write_external_draft, send_external, input_fallback, execute_command, formula-workbook creation, browser navigation, live/batch iWork semantic mutations, and update_text_file/delete_text_file (one exact use). For iWork mutations, first obtain the fresh target and destination references from the matching read tools, then request the mutation separately with the complete tool arguments as exact_input; never batch that mutation permission with its prerequisite read permission. Omit exact_input for ordinary read_file and write_artifact requests unless that tool description explicitly requires it."},
                                "suggested_ttl_seconds": {"type": "integer", "minimum": 1},
                                "suggested_max_uses": {"type": "integer", "minimum": 1},
                                "reason": {"type": "string", "maxLength": MAX_PERMISSION_REASON_BYTES}
                            },
                            "required": ["item_id", "tool_name", "suggested_ttl_seconds", "suggested_max_uses", "reason"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["items"],
                "additionalProperties": false
            }),
        },
        required_capability: Capability::SystemInfo,
        effect: ToolEffect::PermissionPlanning,
    }]
}

pub fn build_permission_request(
    call: &ToolCall,
    registry: &ProviderRegistry,
    request_id: String,
    input_revision: u64,
    created_at: String,
) -> Result<PermissionRequest, AgentError> {
    if call.name != REQUEST_CAPABILITY_GRANTS_TOOL_NAME {
        return Err(invalid(format!("unexpected tool `{}`", call.name)));
    }
    let params: RequestParams =
        serde_json::from_str(&call.arguments_json).map_err(|error| invalid(error.to_string()))?;
    if params.items.is_empty() || params.items.len() > MAX_PERMISSION_REQUEST_ITEMS {
        return Err(invalid("permission batch size is out of bounds"));
    }
    if params.items.iter().any(|item| {
        registry
            .capability_for_tool(&item.tool_name)
            .is_some_and(|capability| capability.wire.effect == CapabilityEffect::SendExternal)
    }) && params.items.len() != 1
    {
        return Err(invalid(
            "SendExternal must be requested separately as one exact one-shot confirmation",
        ));
    }

    let mut items = Vec::with_capacity(params.items.len());
    for item in params.items {
        let capability = registry
            .capability_for_tool(&item.tool_name)
            .ok_or_else(|| invalid(format!("unknown tool `{}`", item.tool_name)))?;
        let provider = registry
            .provider_for_capability(&capability.wire.capability_id)
            .expect("compiled capability has a provider");
        if item
            .provider_id
            .as_ref()
            .is_some_and(|id| id != &provider.wire.provider_id)
        {
            return Err(invalid(format!(
                "tool `{}` belongs to provider `{}`",
                item.tool_name, provider.wire.provider_id
            )));
        }
        if item
            .expected_effect
            .is_some_and(|effect| effect != capability.wire.effect)
        {
            return Err(invalid(format!(
                "tool `{}` effect does not match the compiled descriptor",
                item.tool_name
            )));
        }
        if !matches!(
            capability.wire.effect,
            CapabilityEffect::ExportData
                | CapabilityEffect::WriteExternalDraft
                | CapabilityEffect::SendExternal
        ) && !item.export_destinations.is_empty()
        {
            return Err(invalid(
                "export_destinations are only valid for external egress effects",
            ));
        }
        let application_scope = item.application_scope;
        if matches!(
            capability.required_capability,
            Capability::DesktopUiActionConfirmed | Capability::DesktopBackgroundInputConfirmed
        ) && (application_scope.is_none() || item.exact_input.is_some())
        {
            return Err(invalid(format!(
                r#"item_id={} tool_name={}: native UI permission requires application_scope, never exact_input. Required shape: {{"application_scope":{{"application":{{"token":"<observed application token>","snapshot_id":"<observed snapshot>","object_kind":"application","expires_at":"<observed expiry>"}},"actions":["invoke","set_value"]}}}}. Load the tool details first. If no application reference is known, request prerequisite desktop reads and observe the application before requesting actions"#,
                item.item_id, item.tool_name
            )));
        }

        if let Some(scope) = &application_scope {
            if !crate::application_ui::supports(&item.tool_name) || item.exact_input.is_some() {
                return Err(invalid(
                    "application_scope is only valid for native UI actions and is mutually exclusive with exact_input",
                ));
            }
            crate::application_ui::validate_for_tool(&item.tool_name, scope)?;
        }
        let input_value = application_scope
            .as_ref()
            .map(|scope| serde_json::json!({"application_scope":scope}))
            .or(item.exact_input);
        let (canonical_input_json, canonical_input_digest_sha256) = match input_value {
            Some(mut input) => {
                if application_scope.is_none() {
                    crate::model_input::fill_versions(&item.tool_name, &mut input);
                    if !crate::ui_model_ids::needs_resolution(&item.tool_name) {
                        crate::model_input::validate_format_with_schema(
                            &item.tool_name,
                            &capability.tool_spec.parameters_schema,
                            &input,
                        )
                        .map_err(invalid)?;
                    }
                }
                let canonical = if application_scope.is_some() {
                    canonical_permission_input_json(input)
                } else {
                    canonical_tool_permission_input_json(&item.tool_name, input)
                }
                .map_err(|error| invalid(format!("canonicalize exact_input: {error}")))?;
                if canonical.len() > crate::dynamic_run::MAX_PERMISSION_EXACT_INPUT_BYTES {
                    return Err(invalid("exact_input exceeds the bounded storage limit"));
                }
                let digest = format!("{:x}", Sha256::digest(canonical.as_bytes()));
                (Some(canonical), Some(digest))
            }
            None => (None, None),
        };
        let inherently_r3 = matches!(
            capability.wire.effect,
            CapabilityEffect::SendExternal
                | CapabilityEffect::WriteExternalDraft
                | CapabilityEffect::InputFallback
                | CapabilityEffect::ExecuteCommand
        );
        let inherently_r3 = inherently_r3
            || crate::provider_preflight::text_file::TextMutationPreflight::supports(
                &item.tool_name,
            );
        if inherently_r3 && canonical_input_json.is_none() {
            return Err(invalid("inherently R3 tools require exact_input"));
        }
        if capability.required_capability == Capability::SpreadsheetFormulaWorkbookCreateConfirmed
            && canonical_input_json.is_none()
        {
            return Err(invalid(
                "formula workbook creation requires exact_input so the formula, target, preview, and output name are immutable",
            ));
        }
        let exact_external_url = capability.wire.authorization_hint.resources
            == [AuthorizationResourceKind::ExternalUrl];
        let exact_external_query = capability.wire.authorization_hint.resources
            == [AuthorizationResourceKind::ExternalQuery];
        let exact_command = capability.wire.authorization_hint.resources
            == [AuthorizationResourceKind::ExactCommand];
        let exact_outlook_handoff = capability.wire.capability_id
            == crate::device_assistant::OUTLOOK_NEW_HANDOFF_CAPABILITY_ID;
        let exact_gmail_handoff = capability.wire.capability_id
            == crate::device_assistant::GMAIL_WEB_HANDOFF_CAPABILITY_ID;
        let exact_slack_handoff = capability.wire.capability_id
            == crate::device_assistant::SLACK_WEB_HANDOFF_CAPABILITY_ID;
        let exact_gmail_send =
            capability.wire.capability_id == crate::device_assistant::GMAIL_WEB_SEND_CAPABILITY_ID;
        let exact_slack_send =
            capability.wire.capability_id == crate::device_assistant::SLACK_WEB_SEND_CAPABILITY_ID;
        let exact_browser_navigation = matches!(
            capability.wire.capability_id.as_str(),
            crate::device_assistant::BROWSER_OPEN_CAPABILITY_ID
                | crate::device_assistant::BROWSER_NAVIGATE_CAPABILITY_ID
        );
        if exact_browser_navigation && canonical_input_json.is_none() {
            return Err(invalid(
                "browser open/navigation permissions require exact_input so the approved origin and URL are immutable",
            ));
        }
        if exact_browser_navigation {
            let canonical = canonical_input_json
                .as_deref()
                .expect("browser navigation exact input was checked");
            let value: serde_json::Value = serde_json::from_str(canonical)
                .map_err(|error| invalid(format!("decode browser navigation input: {error}")))?;
            if !exact_input_matches_current_contract(&item.tool_name, &value) {
                return Err(invalid(
                    "browser open/navigation exact_input does not match the current closed tool contract",
                ));
            }
        }
        if exact_outlook_handoff {
            let canonical = canonical_input_json
                .as_deref()
                .ok_or_else(|| invalid("Outlook (new) handoff requires exact_input"))?;
            let input: desk_agent_protocol::communication::OutlookNewDraftHandoffInput =
                serde_json::from_str(canonical).map_err(|error| {
                    invalid(format!("decode Outlook (new) handoff input: {error}"))
                })?;
            input.validate().map_err(|error| {
                invalid(format!("validate Outlook (new) handoff input: {error}"))
            })?;
        }
        let exact_semantic_action = application_scope.is_none()
            && matches!(
                capability.wire.capability_id.as_str(),
                crate::device_assistant::DESKTOP_RAW_INPUT_CAPABILITY_ID
                    | crate::device_assistant::SPREADSHEET_LIVE_PATCH_CAPABILITY_ID
                    | crate::device_assistant::DOCUMENT_LIVE_PATCH_CAPABILITY_ID
                    | crate::device_assistant::PRESENTATION_LIVE_PATCH_CAPABILITY_ID
                    | crate::device_assistant::SPREADSHEET_BATCH_PATCH_CAPABILITY_ID
                    | crate::device_assistant::DOCUMENT_BATCH_PATCH_CAPABILITY_ID
                    | crate::device_assistant::PRESENTATION_BATCH_PATCH_CAPABILITY_ID
            );
        let exact_semantic_refs = if exact_semantic_action {
            #[derive(Deserialize)]
            struct SemanticActionInput {
                target: desk_agent_protocol::computer_use::ObjectRef,
                #[serde(default)]
                output: Option<desk_agent_protocol::computer_use::BatchDocumentOutput>,
            }
            let canonical = canonical_input_json
                .as_deref()
                .ok_or_else(|| invalid(format!("item_id={} tool_name={}: semantic mutations require exact_input containing the complete tool arguments and observed target", item.item_id, item.tool_name)))?;
            let input: SemanticActionInput = serde_json::from_str(canonical)
                .map_err(|error| invalid(format!("decode semantic action input: {error}")))?;
            let expected_kind = match capability.required_capability {
                Capability::DesktopInputFallbackConfirmed => {
                    desk_agent_protocol::computer_use::ObjectKind::Application
                }
                Capability::SpreadsheetLivePatchConfirmed => {
                    desk_agent_protocol::computer_use::ObjectKind::Range
                }
                Capability::DocumentLivePatchConfirmed => {
                    desk_agent_protocol::computer_use::ObjectKind::Document
                }
                Capability::PresentationLivePatchConfirmed => {
                    desk_agent_protocol::computer_use::ObjectKind::Slide
                }
                _ => unreachable!(),
            };
            if input.target.object_kind != expected_kind
                || input.target.token.is_empty()
                || input.target.snapshot_id.is_empty()
                || (!input.target.object_kind.is_lifecycle_bound()
                    && input.target.expires_at.is_empty())
            {
                return Err(invalid(
                    "semantic action requires one complete target reference of the expected kind",
                ));
            }
            if capability.required_capability == Capability::DesktopInputFallbackConfirmed {
                #[derive(Deserialize)]
                struct RawInputOnly {
                    action: desk_agent_protocol::computer_use::RawInputAction,
                }
                let action: RawInputOnly = serde_json::from_str(canonical)
                    .map_err(|error| invalid(format!("decode raw input action: {error}")))?;
                action
                    .action
                    .validate()
                    .map_err(|error| invalid(format!("validate raw input action: {error}")))?;
            }
            let is_batch = matches!(
                capability.wire.capability_id.as_str(),
                crate::device_assistant::SPREADSHEET_BATCH_PATCH_CAPABILITY_ID
                    | crate::device_assistant::DOCUMENT_BATCH_PATCH_CAPABILITY_ID
                    | crate::device_assistant::PRESENTATION_BATCH_PATCH_CAPABILITY_ID
            );
            let mut refs = vec![input.target];
            if is_batch {
                let output = input
                    .output
                    .ok_or_else(|| invalid("BatchDocument semantic action requires output"))?;
                if output.destination_parent.object_kind
                    != desk_agent_protocol::computer_use::ObjectKind::Directory
                    || output.destination_parent.token.is_empty()
                    || output.destination_parent.snapshot_id.is_empty()
                    || output.destination_parent.expires_at.is_empty()
                    || output.native_file_name.is_empty()
                {
                    return Err(invalid(
                        "BatchDocument semantic action requires a complete output directory and native leaf",
                    ));
                }
                refs.push(output.destination_parent);
            } else if input.output.is_some() {
                return Err(invalid(
                    "interactive semantic action cannot include a BatchDocument output",
                ));
            }
            Some(refs)
        } else {
            None
        };
        let mut gmail_account_id = None;
        let mut slack_account_id = None;
        if exact_gmail_handoff {
            let canonical = canonical_input_json
                .as_deref()
                .ok_or_else(|| invalid("Gmail Web handoff requires exact_input"))?;
            let input: desk_agent_protocol::communication::GmailWebDraftHandoffInput =
                serde_json::from_str(canonical)
                    .map_err(|error| invalid(format!("decode Gmail Web handoff input: {error}")))?;
            input
                .validate()
                .map_err(|error| invalid(format!("validate Gmail Web handoff input: {error}")))?;
            gmail_account_id = Some(
                crate::communication::gmail_web_account_id(&input.page)
                    .map_err(|error| invalid(format!("Gmail account unavailable: {error}")))?,
            );
        }
        if exact_slack_handoff {
            let canonical = canonical_input_json
                .as_deref()
                .ok_or_else(|| invalid("Slack Web handoff requires exact_input"))?;
            let input: desk_agent_protocol::communication::SlackWebDraftHandoffInput =
                serde_json::from_str(canonical)
                    .map_err(|error| invalid(format!("decode Slack Web handoff input: {error}")))?;
            input
                .validate()
                .map_err(|error| invalid(format!("validate Slack Web handoff input: {error}")))?;
            slack_account_id = Some(
                crate::communication::slack_web_account_id(&input.page)
                    .map_err(|error| invalid(format!("Slack account unavailable: {error}")))?,
            );
        }
        if exact_gmail_send {
            let canonical = canonical_input_json
                .as_deref()
                .ok_or_else(|| invalid("Gmail Web exact send requires exact_input"))?;
            let input: desk_agent_protocol::communication::GmailWebExactSendInput =
                serde_json::from_str(canonical).map_err(|error| {
                    invalid(format!("decode Gmail Web exact send input: {error}"))
                })?;
            crate::communication::verify_gmail_web_exact_send_input(&input).map_err(|error| {
                invalid(format!("validate Gmail Web exact send input: {error}"))
            })?;
            gmail_account_id = Some(
                crate::communication::gmail_web_account_id(&input.page)
                    .map_err(|error| invalid(format!("Gmail account unavailable: {error}")))?,
            );
        }
        if exact_slack_send {
            let canonical = canonical_input_json
                .as_deref()
                .ok_or_else(|| invalid("Slack Web exact send requires exact_input"))?;
            let input: desk_agent_protocol::communication::SlackWebExactSendInput =
                serde_json::from_str(canonical).map_err(|error| {
                    invalid(format!("decode Slack Web exact send input: {error}"))
                })?;
            crate::communication::verify_slack_web_exact_send_input(&input).map_err(|error| {
                invalid(format!("validate Slack Web exact send input: {error}"))
            })?;
            slack_account_id = Some(
                crate::communication::slack_web_account_id(&input.page)
                    .map_err(|error| invalid(format!("Slack account unavailable: {error}")))?,
            );
        }
        if (exact_external_url || exact_external_query || exact_command)
            && canonical_input_digest_sha256.is_none()
        {
            return Err(invalid(
                "exact URL/query/command permissions require exact_input so the approved input is immutable",
            ));
        }
        let command_confirmation = if exact_command {
            let canonical = canonical_input_json
                .as_deref()
                .expect("ExactCommand exact input was checked");
            Some(
                registry
                    .command_policy()
                    .ok_or_else(|| invalid("command execution has no trusted policy context"))?
                    .prepare(canonical, input_revision)?,
            )
        } else {
            None
        };
        let compiled_scope = canonical_compiled_scope(
            &capability.wire.authorization_hint.resources,
            capability.wire.effect,
        );
        let resource_scope = if let Some(scope) = &application_scope {
            crate::application_ui::resource(&scope.application)
        } else if let Some(targets) = exact_semantic_refs.as_ref() {
            fresh_object_resource_scope(targets)
        } else if exact_external_url {
            exact_external_url_resource_scope(
                canonical_input_digest_sha256
                    .as_deref()
                    .expect("ExternalUrl exact input was checked"),
            )
        } else if exact_external_query {
            registry
                .web_search_binding()
                .ok_or_else(|| invalid("Web Search is not configured"))?
                .resource_scope(
                    canonical_input_digest_sha256
                        .as_deref()
                        .expect("ExternalQuery exact input was checked"),
                )
        } else if exact_command {
            command_confirmation
                .as_ref()
                .expect("exact command was prepared")
                .resource_scope()?
        } else {
            compiled_scope.as_ref().map_or_else(
                || normalize_scope(item.resource_scope),
                |scope| scope.resources.clone(),
            )
        };
        items.push(GrantRequestItem {
            item_id: item.item_id.trim().to_string(),
            provider_id: provider.wire.provider_id.clone(),
            tool_name: item.tool_name,
            expected_effect: capability.wire.effect,
            resource_scope,
            operation_scope: if let Some(scope) = &application_scope {
                scope
                    .actions
                    .iter()
                    .copied()
                    .map(crate::application_ui::operation_kind)
                    .collect()
            } else {
                compiled_scope.map_or_else(
                    || normalize_scope(item.operation_scope),
                    |scope| scope.operations,
                )
            },
            export_destinations: if exact_external_query {
                vec![
                    registry
                        .web_search_binding()
                        .ok_or_else(|| invalid("Web Search is not configured"))?
                        .destination()?,
                ]
            } else if exact_outlook_handoff {
                vec![
                    desk_agent_protocol::data_lineage::DestinationIdentity::EmailAccount {
                        account_id: crate::device_assistant::OUTLOOK_NEW_UNVERIFIED_ACCOUNT_ID
                            .into(),
                    },
                ]
            } else if exact_gmail_handoff {
                vec![
                    desk_agent_protocol::data_lineage::DestinationIdentity::EmailAccount {
                        account_id: gmail_account_id
                            .clone()
                            .ok_or_else(|| invalid("Gmail account is missing"))?,
                    },
                ]
            } else if exact_slack_handoff {
                vec![
                    desk_agent_protocol::data_lineage::DestinationIdentity::ChatAccount {
                        account_id: slack_account_id
                            .clone()
                            .ok_or_else(|| invalid("Slack account is missing"))?,
                    },
                ]
            } else if exact_gmail_send {
                vec![
                    desk_agent_protocol::data_lineage::DestinationIdentity::EmailAccount {
                        account_id: gmail_account_id
                            .clone()
                            .ok_or_else(|| invalid("Gmail account is missing"))?,
                    },
                ]
            } else if exact_slack_send {
                vec![
                    desk_agent_protocol::data_lineage::DestinationIdentity::ChatAccount {
                        account_id: slack_account_id
                            .clone()
                            .ok_or_else(|| invalid("Slack account is missing"))?,
                    },
                ]
            } else {
                item.export_destinations
            },
            canonical_input_json,
            canonical_input_digest_sha256,
            command_confirmation,
            suggested_ttl_seconds: item.suggested_ttl_seconds.clamp(1, MAX_REQUEST_TTL_SECONDS),
            suggested_max_uses: if inherently_r3
                || exact_command
                || exact_outlook_handoff
                || exact_gmail_handoff
                || exact_slack_handoff
                || exact_semantic_action
            {
                1
            } else {
                item.suggested_max_uses.clamp(1, MAX_REQUEST_USES)
            },
            reason: item.reason.trim().to_string(),
        });
    }
    let request = PermissionRequest {
        schema_version: PERMISSION_REQUEST_SCHEMA_VERSION,
        request_id,
        input_revision,
        state: PermissionRequestState::Pending,
        items,
        created_at,
    };
    request
        .validate()
        .map_err(|error| invalid(error.to_string()))?;
    Ok(request)
}

/// Bundle observation with a desktop action as ordinary, independently reviewable
/// permission items. This prepares consent only; it never issues implicit grants.
pub fn include_desktop_action_reads(
    request: &mut PermissionRequest,
    registry: &ProviderRegistry,
) -> Result<(), AgentError> {
    let Some(ttl) = request
        .items
        .iter()
        .filter(|item| {
            matches!(
                item.tool_name.as_str(),
                "execute_ui_actions" | "execute_background_inputs" | "execute_confirmed_raw_input"
            )
        })
        .map(|item| item.suggested_ttl_seconds)
        .max()
    else {
        return Ok(());
    };
    let mut additions = Vec::new();
    for name in ["inspect_desktop_session", "inspect_desktop_ui"] {
        if request.items.iter().any(|item| item.tool_name == name) {
            continue;
        }
        let capability = registry
            .capability_for_tool(name)
            .ok_or_else(|| invalid("desktop observation is unavailable"))?;
        let provider = registry
            .provider_for_capability(&capability.wire.capability_id)
            .ok_or_else(|| invalid("desktop observation Provider is unavailable"))?;
        let item_id = format!("included-{name}");
        if request.items.iter().any(|item| item.item_id == item_id) {
            return Err(invalid(
                "permission item id conflicts with included desktop observation",
            ));
        }
        additions.push(serde_json::json!({
            "item_id": item_id,
            "provider_id": provider.wire.provider_id,
            "tool_name": name,
            "expected_effect": capability.wire.effect,
            "suggested_ttl_seconds": ttl,
            "suggested_max_uses": MAX_REQUEST_USES,
            "reason": "Read the current device UI to locate and verify the approved desktop action. This excludes screenshots."
        }));
    }
    if additions.is_empty() {
        return Ok(());
    }
    if request.items.len() + additions.len() > MAX_PERMISSION_REQUEST_ITEMS {
        return Err(invalid(
            "desktop actions need space for two included read permissions; request at most 14 actions per batch",
        ));
    }
    let reads = build_permission_request(
        &ToolCall {
            id: "included-desktop-reads".into(),
            name: REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),
            arguments_json: serde_json::json!({"items": additions}).to_string(),
        },
        registry,
        request.request_id.clone(),
        request.input_revision,
        request.created_at.clone(),
    )?;
    request.items.extend(reads.items);
    request.validate().map_err(invalid)?;
    Ok(())
}

/// Compare only the server-normalized authority and limits of two requests.
/// Model-chosen item ids and explanatory prose are deliberately excluded so a
/// denied/approved batch cannot be recreated by merely rewording its reason.
/// Item order is also non-authoritative.
pub(crate) fn equivalent_permission_request(
    left: &PermissionRequest,
    right: &PermissionRequest,
) -> bool {
    if left.input_revision != right.input_revision || left.items.len() != right.items.len() {
        return false;
    }
    let mut matched = vec![false; right.items.len()];
    left.items.iter().all(|left_item| {
        let Some((index, _)) = right.items.iter().enumerate().find(|(index, right_item)| {
            !matched[*index]
                && left_item.provider_id == right_item.provider_id
                && left_item.tool_name == right_item.tool_name
                && left_item.expected_effect == right_item.expected_effect
                && left_item.resource_scope == right_item.resource_scope
                && left_item.operation_scope == right_item.operation_scope
                && left_item.export_destinations == right_item.export_destinations
                && left_item.canonical_input_digest_sha256
                    == right_item.canonical_input_digest_sha256
                && left_item.suggested_ttl_seconds == right_item.suggested_ttl_seconds
                && left_item.suggested_max_uses == right_item.suggested_max_uses
        }) else {
            return false;
        };
        matched[index] = true;
        true
    })
}

fn normalize_scope(values: Vec<String>) -> Vec<String> {
    let mut values = values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::capability_provider::CapabilityBlockedReason;

    fn call(arguments_json: &str) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),
            arguments_json: arguments_json.into(),
        }
    }

    #[test]
    fn screenshot_exact_permission_matches_server_resolved_window_only() {
        let canonical =
            |value| canonical_tool_permission_input_json("read_current_screen", value).unwrap();
        let approved = canonical(json!({"window_id":"window-1"}));
        let reference = json!({"token":"window-1","snapshot_id":"identity-1","object_kind":"window","expires_at":""});
        assert_eq!(approved, canonical(json!({"window":reference.clone()})));
        let mut refreshed = reference.clone();
        refreshed["snapshot_id"] = json!("identity-2");
        assert_eq!(approved, canonical(json!({"window":refreshed})));
        let mut other = reference.clone();
        other["token"] = json!("window-2");
        let mut wrong_kind = reference.clone();
        wrong_kind["object_kind"] = json!("application");
        let mut extra = reference.clone();
        extra["unexpected"] = json!(true);
        for unapproved in [
            json!({}),
            json!({"window":other}),
            json!({"window":wrong_kind}),
            json!({"window":extra}),
            json!({"window":reference,"window_id":"window-1"}),
            json!({"window":reference,"display":"other-display"}),
        ] {
            assert_ne!(approved, canonical(unapproved));
        }
        assert_eq!(
            canonical(json!({"window_id":"window-1","display":"1"})),
            canonical(json!({"window":reference,"display":"1"}))
        );
        assert_ne!(
            canonical_tool_permission_input_json("another_tool", json!({"window":reference}))
                .unwrap(),
            approved
        );
    }

    #[test]
    fn server_resolves_descriptor_and_narrows_limits() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let request = build_permission_request(
            &call(
                r#"{"items":[{"item_id":" inspect ","provider_id":"desktop.session","tool_name":"inspect_desktop_session","expected_effect":"read_device","resource_scope":[" target:device ","target:device"],"suggested_ttl_seconds":999999,"suggested_max_uses":999,"reason":" inspect the selected target "}]}"#,
            ),
            &registry,
            "permission-1".into(),
            3,
            "2026-08-26T00:00:00Z".into(),
        )
        .unwrap();
        assert_eq!(request.state, PermissionRequestState::Pending);
        assert_eq!(request.items[0].item_id, "inspect");
        assert_eq!(
            request.items[0].resource_scope,
            vec!["target:current_device"]
        );
        assert_eq!(request.items[0].operation_scope, vec!["observe"]);
        assert_eq!(
            request.items[0].suggested_ttl_seconds,
            MAX_REQUEST_TTL_SECONDS
        );
        assert_eq!(request.items[0].suggested_max_uses, MAX_REQUEST_USES);
    }

    #[test]
    fn equivalent_request_ignores_model_labels_but_not_authority_or_limits() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let first = build_permission_request(
            &call(
                r#"{"items":[{"item_id":"open-a","provider_id":"browser.page.open","tool_name":"browser_open_page","expected_effect":"mutate_application","exact_input":{"target":{"origin":{"kind":"https","host_ascii":"app.slack.com","port":443},"url":"https://app.slack.com/"}},"suggested_ttl_seconds":300,"suggested_max_uses":1,"reason":"first wording"}]}"#,
            ),
            &registry,
            "permission-a".into(),
            7,
            "2026-08-29T00:00:00Z".into(),
        )
        .unwrap();
        let same = build_permission_request(
            &call(
                r#"{"items":[{"item_id":"open-b","provider_id":"browser.page.open","tool_name":"browser_open_page","expected_effect":"mutate_application","exact_input":{"target":{"url":"https://app.slack.com/","origin":{"port":443,"host_ascii":"app.slack.com","kind":"https"}}},"suggested_ttl_seconds":300,"suggested_max_uses":1,"reason":"different wording"}]}"#,
            ),
            &registry,
            "permission-b".into(),
            7,
            "2026-08-29T00:01:00Z".into(),
        )
        .unwrap();
        let mut different_limit = same.clone();
        different_limit.items[0].suggested_ttl_seconds = 301;

        assert!(equivalent_permission_request(&first, &same));
        assert!(!equivalent_permission_request(&first, &different_limit));
    }

    #[test]
    fn browser_navigation_permission_requires_exact_input() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let missing = call(
            r#"{"items":[{"item_id":"open","provider_id":"browser.page.open","tool_name":"browser_open_page","expected_effect":"mutate_application","suggested_ttl_seconds":300,"suggested_max_uses":1,"reason":"open Slack"}]}"#,
        );
        let error = build_permission_request(
            &missing,
            &registry,
            "permission-open".into(),
            1,
            "2026-08-29T00:00:00Z".into(),
        )
        .unwrap_err();
        assert!(error.message.contains("require exact_input"));
    }

    #[test]
    fn permission_request_derives_provider_and_effect_from_tool_name() {
        let request = build_permission_request(
            &call(r#"{"items":[{"item_id":"read","tool_name":"inspect_desktop_session","suggested_ttl_seconds":60,"suggested_max_uses":2,"reason":"Inspect windows"}]}"#),
            &crate::device_assistant::device_assistant_provider_registry(), "permission-derived".into(),
            1, "2026-09-08T00:00:00Z".into()).unwrap();
        assert_eq!(request.items[0].provider_id, "desktop.session");
        assert_eq!(
            request.items[0].expected_effect,
            CapabilityEffect::ReadDevice
        );
    }

    #[test]
    fn model_cannot_invent_provider_effect_or_grant_fields() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        for arguments in [
            r#"{"items":[{"item_id":"x","provider_id":"invented","tool_name":"inspect_desktop_session","expected_effect":"read_device","suggested_ttl_seconds":10,"suggested_max_uses":1,"reason":"x"}]}"#,
            r#"{"items":[{"item_id":"x","provider_id":"desktop.session","tool_name":"inspect_desktop_session","expected_effect":"send_external","suggested_ttl_seconds":10,"suggested_max_uses":1,"reason":"x"}]}"#,
            r#"{"items":[{"item_id":"x","provider_id":"desktop.session","tool_name":"inspect_desktop_session","expected_effect":"read_device","suggested_ttl_seconds":10,"suggested_max_uses":1,"reason":"x","grant_id":"forged"}]}"#,
        ] {
            assert!(
                build_permission_request(
                    &call(arguments),
                    &registry,
                    "permission-1".into(),
                    1,
                    "2026-08-26T00:00:00Z".into(),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn external_url_permission_requires_and_binds_exact_input() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let missing = r#"{"items":[{"item_id":"web","provider_id":"web.research","tool_name":"fetch_public_web_page","expected_effect":"read_external","resource_scope":["model:chosen"],"suggested_ttl_seconds":60,"suggested_max_uses":1,"reason":"Read the cited page"}]}"#;
        assert!(
            build_permission_request(
                &call(missing),
                &registry,
                "permission-web".into(),
                1,
                "2026-08-26T00:00:00Z".into(),
            )
            .is_err()
        );

        let exact = r#"{"items":[{"item_id":"web","provider_id":"web.research","tool_name":"fetch_public_web_page","expected_effect":"read_external","resource_scope":["model:chosen"],"operation_scope":["anything"],"exact_input":{"url":"https://example.com/report"},"suggested_ttl_seconds":60,"suggested_max_uses":1,"reason":"Read the cited page"}]}"#;
        let request = build_permission_request(
            &call(exact),
            &registry,
            "permission-web".into(),
            1,
            "2026-08-26T00:00:00Z".into(),
        )
        .unwrap();
        let item = &request.items[0];
        assert_eq!(item.operation_scope, vec!["fetch_public_https"]);
        assert!(item.resource_scope[0].starts_with("external_url_input:sha256:"));
        assert!(
            !item
                .resource_scope
                .iter()
                .any(|scope| scope == "model:chosen")
        );
        assert_eq!(
            item.canonical_input_json.as_deref(),
            Some(r#"{"url":"https://example.com/report"}"#)
        );
    }

    #[test]
    fn exact_input_digest_ignores_nested_object_member_order() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let first = r#"{"items":[{"item_id":"browser","provider_id":"browser.page.open","tool_name":"browser_open_page","expected_effect":"mutate_application","exact_input":{"target":{"url":"http://127.0.0.1:5174/user/login","origin":{"kind":"http_loopback","host_ascii":"127.0.0.1","port":5174}}},"suggested_ttl_seconds":60,"suggested_max_uses":1,"reason":"Open the selected local development page"}]}"#;
        let reordered = r#"{"items":[{"item_id":"browser","provider_id":"browser.page.open","tool_name":"browser_open_page","expected_effect":"mutate_application","exact_input":{"target":{"origin":{"port":5174,"host_ascii":"127.0.0.1","kind":"http_loopback"},"url":"http://127.0.0.1:5174/user/login"}},"suggested_ttl_seconds":60,"suggested_max_uses":1,"reason":"Open the selected local development page"}]}"#;

        let first = build_permission_request(
            &call(first),
            &registry,
            "permission-browser-a".into(),
            1,
            "2026-08-27T00:00:00Z".into(),
        )
        .unwrap();
        let reordered = build_permission_request(
            &call(reordered),
            &registry,
            "permission-browser-b".into(),
            1,
            "2026-08-27T00:00:00Z".into(),
        )
        .unwrap();

        assert_eq!(
            first.items[0].canonical_input_json,
            reordered.items[0].canonical_input_json
        );
        assert_eq!(
            first.items[0].canonical_input_digest_sha256,
            reordered.items[0].canonical_input_digest_sha256
        );
        assert_eq!(
            first.items[0].canonical_input_json.as_deref(),
            Some(
                r#"{"target":{"origin":{"host_ascii":"127.0.0.1","kind":"http_loopback","port":5174},"url":"http://127.0.0.1:5174/user/login"}}"#
            )
        );
    }

    #[test]
    fn browser_navigation_permission_rejects_non_tool_wire_shape_before_approval() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let malformed = r#"{"items":[{"item_id":"browser","provider_id":"browser.page.open","tool_name":"browser_open_page","expected_effect":"mutate_application","exact_input":{"origin":{"kind":"https","host_ascii":"lcxl-remote.slack.com","port":443},"url":"https://lcxl-remote.slack.com/"},"suggested_ttl_seconds":60,"suggested_max_uses":1,"reason":"Open the selected Slack workspace"}]}"#;

        let error = build_permission_request(
            &call(malformed),
            &registry,
            "permission-browser-malformed".into(),
            1,
            "2026-08-29T00:00:00Z".into(),
        )
        .unwrap_err();

        assert!(error.message.contains("Structural example"));
    }

    #[test]
    fn external_query_permission_fixes_input_scope_and_connector_destination() {
        let registry = crate::device_assistant::device_assistant_provider_registry()
            .with_web_search_binding(Some(crate::web_research::SearchBinding {
                connector_id: "brave_web_v1".into(),
                revision: 3,
            }));
        let missing = r#"{"items":[{"item_id":"search","provider_id":"web.search","tool_name":"search_public_web","expected_effect":"export_data","suggested_ttl_seconds":60,"suggested_max_uses":1,"reason":"Search public sources"}]}"#;
        assert!(
            build_permission_request(
                &call(missing),
                &registry,
                "permission-search".into(),
                1,
                "2026-08-26T00:00:00Z".into(),
            )
            .is_err()
        );

        let exact = r#"{"items":[{"item_id":"search","provider_id":"web.search","tool_name":"search_public_web","expected_effect":"export_data","resource_scope":["model:chosen"],"operation_scope":["anything"],"export_destinations":[{"kind":"web_research","connector_id":"model-chosen"}],"exact_input":{"query":"Rust language","max_results":5},"suggested_ttl_seconds":60,"suggested_max_uses":1,"reason":"Search public sources"}]}"#;
        let request = build_permission_request(
            &call(exact),
            &registry,
            "permission-search".into(),
            1,
            "2026-08-26T00:00:00Z".into(),
        )
        .unwrap();
        let item = &request.items[0];
        assert_eq!(item.operation_scope, vec!["search_public_web"]);
        assert!(item.resource_scope[0].starts_with("external_query_input:sha256:"));
        assert!(
            item.resource_scope
                .contains(&"web_search_config:brave_web_v1:3".into())
        );
        assert_eq!(
            item.export_destinations,
            vec![
                desk_agent_protocol::data_lineage::DestinationIdentity::WebResearch {
                    connector_id: crate::device_assistant::BRAVE_WEB_SEARCH_CONNECTOR_ID.into(),
                }
            ]
        );
        assert_eq!(
            item.canonical_input_json.as_deref(),
            Some(r#"{"max_results":5,"query":"Rust language"}"#)
        );

        let defaulted = exact.replace(r#",\"max_results\":5"#, "");
        let defaulted_request = build_permission_request(
            &call(&defaulted),
            &registry,
            "permission-search-defaulted".into(),
            1,
            "2026-08-26T00:00:00Z".into(),
        )
        .unwrap();
        assert_eq!(
            defaulted_request.items[0].canonical_input_json,
            item.canonical_input_json
        );
        assert_eq!(
            defaulted_request.items[0].canonical_input_digest_sha256,
            item.canonical_input_digest_sha256
        );
    }

    #[test]
    fn permission_tool_keeps_export_destinations_server_owned() {
        let tool = permission_planning_tool_registry().remove(0);
        let item_properties =
            &tool.spec.parameters_schema["properties"]["items"]["items"]["properties"];
        assert!(item_properties.get("export_destinations").is_none());
        assert!(
            tool.spec
                .description
                .contains("Never supply an export destination")
        );
        assert!(
            item_properties["exact_input"]["description"]
                .as_str()
                .unwrap()
                .contains("never batch that mutation permission with its prerequisite read")
        );
        assert!(
            item_properties["exact_input"]["description"]
                .as_str()
                .unwrap()
                .contains("Omit exact_input for ordinary read_file and write_artifact")
        );
    }

    #[test]
    fn command_permission_requires_exact_input_and_forces_one_shot_scope() {
        let mut policy = crate::command_confirmation::test_policy();
        policy.admission_policy = desk_agent_protocol::authz::ExecAdmissionPolicy::TemplateOnly;
        let registry = crate::device_assistant::device_assistant_provider_registry()
            .with_command_policy(policy);
        let missing = r#"{"items":[{"item_id":"command","provider_id":"system.command","tool_name":"execute_confirmed_command","expected_effect":"execute_command","suggested_ttl_seconds":60,"suggested_max_uses":9,"reason":"Restart the requested service"}]}"#;
        assert!(
            build_permission_request(
                &call(missing),
                &registry,
                "permission-command".into(),
                1,
                "2026-08-26T00:00:00Z".into(),
            )
            .is_err()
        );

        let exact = r#"{"items":[{"item_id":"command","provider_id":"system.command","tool_name":"execute_confirmed_command","expected_effect":"execute_command","resource_scope":["model:chosen"],"operation_scope":["anything"],"exact_input":{"schema_version":1,"shell":"powershell","command":"Restart-Service -Name Spooler","timeout_ms":10000},"suggested_ttl_seconds":60,"suggested_max_uses":9,"reason":"Restart the requested service"}]}"#;
        let request = build_permission_request(
            &call(exact),
            &registry,
            "permission-command".into(),
            1,
            "2026-08-26T00:00:00Z".into(),
        )
        .unwrap();
        let item = &request.items[0];
        assert_eq!(item.operation_scope, vec!["execute_confirmed_command"]);
        assert!(item.resource_scope[0].starts_with("command_input:sha256:"));
        assert_eq!(item.suggested_max_uses, 1);
        assert!(
            !item
                .resource_scope
                .iter()
                .any(|scope| scope == "model:chosen")
        );

        let off_template = r#"{"items":[{"item_id":"command","provider_id":"system.command","tool_name":"execute_confirmed_command","expected_effect":"execute_command","exact_input":{"schema_version":1,"shell":"powershell","command":"Remove-Item C:\\temp\\anything","timeout_ms":10000},"suggested_ttl_seconds":60,"suggested_max_uses":1,"reason":"Delete files"}]}"#;
        let error = build_permission_request(
            &call(off_template),
            &registry,
            "permission-command-off-template".into(),
            1,
            "2026-08-26T00:00:00Z".into(),
        )
        .unwrap_err();
        assert!(error.message.contains("command rejected by current policy"));
    }

    #[test]
    fn input_fallback_permission_is_narrowed_to_one_shot_before_pending() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let page = serde_json::json!({
            "schema_version": desk_agent_protocol::browser_control::BROWSER_CONTROL_SCHEMA_VERSION,
            "adapter": {
                "engine": "chrome_extension",
                "device_id": "device-1",
                "os_session_id": "session-1",
                "browser_major_version": 151,
                "browser_version": "151.0.0.0",
                "adapter_id": "lcxl-browser-extension",
                "adapter_version": "1.7.0",
                "profile_incarnation": "profile-1",
                "connection_revision": 7
            },
            "account_id": "gmail-web:alice@example.com",
            "page_id": "page-1",
            "page_incarnation": "page-incarnation-1",
            "origin": {"kind": "https", "host_ascii": "mail.google.com", "port": 443},
            "document_revision": 2,
            "url_sha256": "a".repeat(64),
            "observed_at_unix_ms": 42
        });
        let field = |element_id: &str, accessible_name: &str, role: &str| {
            serde_json::json!({
                "page_id": "page-1",
                "page_incarnation": "page-incarnation-1",
                "document_revision": 2,
                "element_id": element_id,
                "role": role,
                "accessible_name": accessible_name,
                "value": null,
                "element_revision": 1
            })
        };
        let arguments = serde_json::json!({"items":[{"item_id":"activate","provider_id":"browser.element.activate","tool_name":"browser_activate_element","expected_effect":"input_fallback","exact_input":{"page":page,"element":field("button-1", "Open", "button")},"suggested_ttl_seconds":300,"suggested_max_uses":2,"reason":"Activate the selected semantic element once"}]}).to_string();
        let request = build_permission_request(
            &call(&arguments),
            &registry,
            "permission-browser-activate".into(),
            1,
            "2026-08-27T00:00:00Z".into(),
        )
        .unwrap();

        assert_eq!(request.items[0].suggested_max_uses, 1);
        assert!(request.items[0].canonical_input_digest_sha256.is_some());
    }

    #[test]
    fn semantic_ui_permission_requires_application_scope_and_includes_reads() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let missing = r#"{"items":[{"item_id":"ui","provider_id":"desktop.ui.action","tool_name":"execute_ui_actions","expected_effect":"mutate_application","suggested_ttl_seconds":60,"suggested_max_uses":4,"reason":"Update the selected control"}]}"#;
        let error = build_permission_request(
            &call(missing),
            &registry,
            "permission-ui-missing".into(),
            1,
            "2026-08-28T00:00:00Z".into(),
        )
        .unwrap_err();
        assert!(error.message.contains("requires application_scope"));

        let exact = r#"{"items":[{"item_id":"ui","tool_name":"execute_ui_actions","resource_scope":["model:chosen"],"application_scope":{"application":{"token":"app","snapshot_id":"snapshot-1","object_kind":"application","expires_at":"2026-08-28T00:01:00Z"},"actions":["set_value"]},"suggested_ttl_seconds":60,"suggested_max_uses":4,"reason":"Update controls"}]}"#;
        let request = build_permission_request(
            &call(exact),
            &registry,
            "permission-ui".into(),
            1,
            "2026-08-28T00:00:00Z".into(),
        )
        .unwrap();
        let item = &request.items[0];
        assert_eq!(item.suggested_max_uses, 4);
        assert_eq!(item.operation_scope, vec!["ui:set_value"]);
        assert_eq!(item.resource_scope.len(), 1);
        assert!(item.resource_scope[0].starts_with("ui_application:sha256:"));
        assert!(item.canonical_input_digest_sha256.is_some());
        assert!(
            !item
                .resource_scope
                .iter()
                .any(|scope| scope == "model:chosen")
        );

        let mut bundled = request.clone();
        include_desktop_action_reads(&mut bundled, &registry).unwrap();
        assert_eq!(bundled.items.len(), 3);
        assert_eq!(bundled.items[0], request.items[0]);
        for read in &bundled.items[1..] {
            assert_eq!(read.expected_effect, CapabilityEffect::ReadDevice);
            assert_eq!(read.resource_scope, vec!["target:current_device"]);
            assert_eq!(read.operation_scope, vec!["observe"]);
            assert_eq!(read.suggested_ttl_seconds, 60);
            assert_eq!(read.suggested_max_uses, MAX_REQUEST_USES);
            assert!(read.canonical_input_json.is_none());
            assert!(read.export_destinations.is_empty());
        }
        let before = bundled.clone();
        include_desktop_action_reads(&mut bundled, &registry).unwrap();
        assert_eq!(before, bundled);
        // Explicitly requested narrower read limits are never widened.
        bundled.items[1].suggested_ttl_seconds = 10;
        bundled.items[1].suggested_max_uses = 1;
        include_desktop_action_reads(&mut bundled, &registry).unwrap();
        assert_eq!(bundled.items[1].suggested_ttl_seconds, 10);
        assert_eq!(bundled.items[1].suggested_max_uses, 1);
        let mut crowded = request.clone();
        crowded.items = (0..15)
            .map(|i| {
                let mut item = request.items[0].clone();
                item.item_id = format!("action-{i}");
                item
            })
            .collect();
        let original = crowded.clone();
        assert!(include_desktop_action_reads(&mut crowded, &registry).is_err());
        assert_eq!(crowded, original);

        let invalid = exact.replace("application_scope", "exact_input");
        assert!(
            build_permission_request(
                &call(&invalid),
                &registry,
                "invalid".into(),
                1,
                "2026-08-28T00:00:00Z".into()
            )
            .is_err()
        );
    }

    #[test]
    fn raw_input_permission_is_r3_one_shot_and_binds_application_screen_and_step() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let exact = r#"{"items":[{"item_id":"raw","provider_id":"desktop.input.fallback","tool_name":"execute_confirmed_raw_input","expected_effect":"input_fallback","resource_scope":["model:chosen"],"operation_scope":["anything"],"exact_input":{"target":{"token":"application-token","snapshot_id":"snapshot-1","object_kind":"application","expires_at":"2026-08-28T00:01:00Z"},"action":{"screen":{"display":"\\\\.\\DISPLAY1","width":1920,"height":1080,"dpi_x":96,"dpi_y":96},"step":{"kind":"click","params":{"x":100,"y":200,"button":"primary"}}}},"suggested_ttl_seconds":300,"suggested_max_uses":9,"reason":"Last-resort click after semantic controls were unavailable"}]}"#;
        let request = build_permission_request(
            &call(exact),
            &registry,
            "permission-raw-input".into(),
            1,
            "2026-08-28T00:00:00Z".into(),
        )
        .unwrap();
        let item = &request.items[0];
        assert_eq!(item.suggested_max_uses, 1);
        assert_eq!(item.operation_scope, vec!["use_selected_object"]);
        assert_eq!(item.resource_scope.len(), 1);
        assert!(item.resource_scope[0].starts_with("selected:sha256:"));
        assert!(item.canonical_input_digest_sha256.is_some());

        let out_of_bounds = exact.replace(r#""x":100"#, r#""x":1920"#);
        let error = build_permission_request(
            &call(&out_of_bounds),
            &registry,
            "permission-raw-input-out-of-bounds".into(),
            1,
            "2026-08-28T00:00:00Z".into(),
        )
        .unwrap_err();
        assert!(error.message.contains("outside the observed display"));
    }

    #[test]
    fn batch_iwork_permission_binds_target_and_selected_output_directory() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let missing = r#"{"items":[{"item_id":"numbers-batch","provider_id":"spreadsheet.live","tool_name":"patch_selected_numbers_copy","expected_effect":"mutate_application","suggested_ttl_seconds":60,"suggested_max_uses":3,"reason":"Create the requested Numbers copy"}]}"#;
        assert!(
            build_permission_request(
                &call(missing),
                &registry,
                "permission-numbers-batch-missing".into(),
                1,
                "2026-08-28T00:00:00Z".into(),
            )
            .unwrap_err()
            .message
            .contains("require exact_input")
        );

        let exact = r#"{"items":[{"item_id":"numbers-batch","provider_id":"spreadsheet.live","tool_name":"patch_selected_numbers_copy","expected_effect":"mutate_application","resource_scope":["model:chosen"],"operation_scope":["anything"],"exact_input":{"target":{"token":"cell-token","snapshot_id":"batch-snapshot","object_kind":"range","expires_at":"2026-08-28T00:01:00Z"},"output":{"destination_parent":{"token":"directory-token","snapshot_id":"directory-snapshot","object_kind":"directory","expires_at":"2026-08-28T00:01:00Z"},"native_file_name":"reviewed-copy.numbers"},"action":{"kind":"set_cell_value","params":{"value":"42"}}},"suggested_ttl_seconds":60,"suggested_max_uses":3,"reason":"Create the requested Numbers copy"}]}"#;
        let request = build_permission_request(
            &call(exact),
            &registry,
            "permission-numbers-batch".into(),
            1,
            "2026-08-28T00:00:00Z".into(),
        )
        .unwrap();
        let item = &request.items[0];
        assert_eq!(item.suggested_max_uses, 1);
        assert_eq!(item.operation_scope, vec!["use_selected_object"]);
        assert_eq!(item.resource_scope.len(), 2);
        assert!(
            item.resource_scope
                .iter()
                .all(|scope| scope.starts_with("selected:sha256:"))
        );
        assert!(item.canonical_input_digest_sha256.is_some());
    }

    #[test]
    fn formula_workbook_permission_requires_and_binds_exact_input() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let missing = r#"{"items":[{"item_id":"formula","provider_id":"spreadsheet.formula_artifact","tool_name":"create_formula_workbook_from_merge_preview","expected_effect":"write_artifact","suggested_ttl_seconds":60,"suggested_max_uses":1,"reason":"Create the requested formula workbook copy"}]}"#;
        let error = build_permission_request(
            &call(missing),
            &registry,
            "permission-formula".into(),
            1,
            "2026-08-26T00:00:00Z".into(),
        )
        .unwrap_err();
        assert!(
            error
                .message
                .contains("formula workbook creation requires exact_input")
        );

        let exact = r#"{"items":[{"item_id":"formula","provider_id":"spreadsheet.formula_artifact","tool_name":"create_formula_workbook_from_merge_preview","expected_effect":"write_artifact","resource_scope":["directory:current"],"operation_scope":["create_new_artifact"],"exact_input":{"preview_id":"preview-1","file_name":"regional-formula.xlsx","target_cell":"Merged!C2","formula":"=B2*1.1","locale":"en-US-a1"},"suggested_ttl_seconds":60,"suggested_max_uses":1,"reason":"Create the requested formula workbook copy"}]}"#;
        let request = build_permission_request(
            &call(exact),
            &registry,
            "permission-formula".into(),
            1,
            "2026-08-26T00:00:00Z".into(),
        )
        .unwrap();
        let item = &request.items[0];
        assert!(item.canonical_input_digest_sha256.is_some());
        assert_eq!(
            item.canonical_input_json.as_deref(),
            Some(
                r#"{"file_name":"regional-formula.xlsx","formula":"=B2*1.1","locale":"en-US-a1","preview_id":"preview-1","target_cell":"Merged!C2"}"#
            )
        );
    }

    #[test]
    fn catalog_and_request_validation_share_live_edge_readiness() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let desktop = registry
            .capability(crate::device_assistant::DESKTOP_SESSION_CAPABILITY_ID)
            .unwrap();
        let office = registry
            .capability(crate::device_assistant::OFFICE_DOCUMENT_CAPABILITY_ID)
            .unwrap();
        let inventory = vec![
            CapabilityAvailability {
                provider_id: crate::device_assistant::DESKTOP_SESSION_PROVIDER_ID.into(),
                capability_id: desktop.wire.capability_id.clone(),
                tool_name: desktop.tool_spec.name.clone(),
                compiled: true,
                enabled: true,
                connected: true,
                ready: true,
                reason: None,
            },
            CapabilityAvailability {
                provider_id: crate::device_assistant::OFFICE_DOCUMENT_PROVIDER_ID.into(),
                capability_id: office.wire.capability_id.clone(),
                tool_name: office.tool_spec.name.clone(),
                compiled: true,
                enabled: true,
                connected: true,
                ready: false,
                reason: Some(CapabilityBlockedReason::OfficeBridgeNotPaired),
            },
        ];
        let catalog =
            discoverable_catalog_prompt(&registry, &inventory, &[desktop.registered_tool()]);
        assert!(catalog.contains("\"tool_name\":\"inspect_desktop_session\""));
        assert!(catalog.contains("\"callable_now\":true"));
        assert!(catalog.contains("\"tool_name\":\"inspect_office_selection\""));
        assert!(catalog.contains("\"blocked_reason\":\"office_bridge_not_paired\""));
        let serialized = catalog
            .split_once("<capability_catalog>")
            .unwrap()
            .1
            .split_once("</capability_catalog>")
            .unwrap()
            .0;
        let entries: Vec<serde_json::Value> = serde_json::from_str(serialized).unwrap();
        let desktop_entry = entries
            .iter()
            .find(|entry| entry["tool_name"] == "inspect_desktop_session")
            .unwrap();
        let office_entry = entries
            .iter()
            .find(|entry| entry["tool_name"] == "inspect_office_selection")
            .unwrap();
        assert!(desktop_entry.get("input_schema").is_some());
        assert!(office_entry.get("input_schema").is_none());
        assert!(office_entry.get("description").is_none());

        let mut requestable_inventory = inventory.clone();
        requestable_inventory[1].ready = true;
        requestable_inventory[1].reason = None;
        let requestable_catalog = discoverable_catalog_prompt_with_permission_candidates(
            &registry,
            &requestable_inventory,
            &[],
            &[desktop.registered_tool()],
        );
        let serialized = requestable_catalog
            .split_once("<capability_catalog>")
            .unwrap()
            .1
            .split_once("</capability_catalog>")
            .unwrap()
            .0;
        let entries: Vec<serde_json::Value> = serde_json::from_str(serialized).unwrap();
        let desktop_entry = entries
            .iter()
            .find(|entry| entry["tool_name"] == "inspect_desktop_session")
            .unwrap();
        let office_entry = entries
            .iter()
            .find(|entry| entry["tool_name"] == "inspect_office_selection")
            .unwrap();
        assert_eq!(desktop_entry["callable_now"], false);
        assert_eq!(desktop_entry["permission_requestable_now"], true);
        assert_eq!(office_entry["callable_now"], false);
        assert_eq!(office_entry["permission_requestable_now"], false);
        assert!(requestable_catalog.contains(
            "Include a tool in request_capability_grants only when permission_requestable_now=true"
        ));

        let request = build_permission_request(
            &call(
                r#"{"items":[{"item_id":"office","provider_id":"office.document","tool_name":"inspect_office_selection","expected_effect":"read_device","suggested_ttl_seconds":300,"suggested_max_uses":1,"reason":"Inspect the active workbook"}]}"#,
            ),
            &registry,
            "permission-office".into(),
            1,
            "2026-08-26T00:00:00Z".into(),
        )
        .unwrap();
        let error =
            validate_request_availability(&request, &inventory, &[desktop.registered_tool()])
                .unwrap_err();
        assert!(error.message.contains("not runtime-ready"));

        let file = registry
            .capability(crate::device_assistant::SPREADSHEET_FILE_CAPABILITY_ID)
            .unwrap();
        let file_inventory = vec![CapabilityAvailability {
            provider_id: crate::device_assistant::SPREADSHEET_FILE_PROVIDER_ID.into(),
            capability_id: file.wire.capability_id.clone(),
            tool_name: file.tool_spec.name.clone(),
            compiled: true,
            enabled: true,
            connected: true,
            ready: true,
            reason: None,
        }];
        let request = build_permission_request(
            &call(
                r#"{"items":[{"item_id":"file","provider_id":"spreadsheet.file","tool_name":"inspect_selected_spreadsheets","expected_effect":"read_file","suggested_ttl_seconds":300,"suggested_max_uses":1,"reason":"Inspect the selected workbook"}]}"#,
            ),
            &registry,
            "permission-file".into(),
            1,
            "2026-08-26T00:00:00Z".into(),
        )
        .unwrap();
        let error =
            validate_request_availability(&request, &file_inventory, &[desktop.registered_tool()])
                .unwrap_err();
        assert!(error.message.contains("not callable in this turn"));
    }

    #[test]
    fn manager_catalog_keeps_unavailable_schemas_out_and_stays_within_budget() {
        use crate::capability_availability::{
            CentralCapabilityReadiness, project_capability_availability,
        };
        use crate::device_assistant::ACTION_PREVIEW_CAPABILITY_ID;

        let registry = crate::device_assistant::device_assistant_provider_registry();
        let inventory = project_capability_availability(
            &registry,
            desk_agent_protocol::capability_provider::ProductSurface::ManagerPersonalOwner,
            1,
            [CentralCapabilityReadiness::ready(
                ACTION_PREVIEW_CAPABILITY_ID,
            )],
            Vec::new(),
        )
        .unwrap();
        let callable =
            crate::capability_availability::callable_tools(&registry, &inventory).unwrap();
        let catalog = discoverable_catalog_prompt(&registry, &inventory, &callable);
        assert!(
            catalog.len() <= MAX_CAPABILITY_CATALOG_PROMPT_BYTES,
            "catalog bytes {} exceed {}",
            catalog.len(),
            MAX_CAPABILITY_CATALOG_PROMPT_BYTES
        );

        let serialized = catalog
            .split_once("<capability_catalog>")
            .unwrap()
            .1
            .split_once("</capability_catalog>")
            .unwrap()
            .0;
        let entries: Vec<serde_json::Value> = serde_json::from_str(serialized).unwrap();
        let unavailable = entries
            .iter()
            .filter(|entry| entry["runtime_ready"] == false)
            .collect::<Vec<_>>();
        assert!(!unavailable.is_empty());
        assert!(unavailable.iter().all(|entry| {
            entry.get("input_schema").is_none() && entry.get("description").is_none()
        }));
    }

    #[test]
    fn catalog_metrics_measure_the_real_serializer_without_content() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
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
        let permission_candidates = callable
            .iter()
            .filter(|tool| tool.effect != ToolEffect::ReadOnly)
            .cloned()
            .collect::<Vec<_>>();
        let metrics =
            capability_catalog_metrics(&registry, &inventory, &callable, &permission_candidates);
        assert_eq!(metrics.registry_count, inventory.len() as u64);
        assert_eq!(metrics.runtime_ready_count, inventory.len() as u64);
        assert_eq!(metrics.callable_count, callable.len() as u64);
        assert_eq!(
            metrics.permission_candidate_count,
            permission_candidates.len() as u64
        );
        assert_eq!(metrics.catalog_utf8_bytes, 0);
        assert!(metrics.detail_min_utf8_bytes > 0);
        assert!(metrics.detail_min_utf8_bytes <= metrics.detail_p50_utf8_bytes);
        assert!(metrics.detail_p50_utf8_bytes <= metrics.detail_p95_utf8_bytes);
        assert!(metrics.detail_p95_utf8_bytes <= metrics.detail_max_utf8_bytes);
        let debug = format!("{metrics:?}");
        assert!(!debug.contains("inspect_desktop_session"));
        assert!(!debug.contains("input_schema"));
    }

    #[test]
    fn authorization_projection_overrides_stale_pending_history_without_grant_ids() {
        use desk_agent_protocol::capability_grant::{
            CAPABILITY_GRANT_SCHEMA_VERSION, CapabilityGrantIssuer, CapabilityGrantLimits,
            CapabilityGrantUsePolicy, CapabilityRiskTier,
        };
        use desk_agent_protocol::capability_provider::ProductSurface;

        let grant = CapabilityGrant {
            schema_version: CAPABILITY_GRANT_SCHEMA_VERSION,
            grant_id: "secret-grant-id".into(),
            actor_id: "owner".into(),
            run_id: "run".into(),
            input_revision: 1,
            surface: ProductSurface::OssPersonalOwner,
            target_device_id: "device".into(),
            target_session_id: None,
            provider_id: crate::device_assistant::OFFICE_DOCUMENT_PROVIDER_ID.into(),
            capability_id: crate::device_assistant::OFFICE_DOCUMENT_CAPABILITY_ID.into(),
            tool_name: "inspect_office_selection".into(),
            tool_schema_version: 1,
            effect: CapabilityEffect::ReadDevice,
            risk_tier: CapabilityRiskTier::R1,
            resource_scope: vec!["target:current_device".into()],
            operation_scope: vec!["observe".into()],
            export_destinations: Vec::new(),
            allowed_envelope_ids: Vec::new(),
            allowed_content_digests_sha256: Vec::new(),
            use_policy: CapabilityGrantUsePolicy::Reusable,
            canonical_input_digest_sha256: None,
            issued_by: CapabilityGrantIssuer::UserDecision,
            issued_at_unix_ms: 100,
            expires_at_unix_ms: 1_000,
            remaining_uses: 1,
            limits: CapabilityGrantLimits {
                max_bytes_per_call: 1024,
                max_items_per_call: 1,
                max_calls: 1,
            },
            policy_revision: 1,
            readiness_revision: 1,
            revoked_at_unix_ms: None,
            revoked_reason: None,
        };
        let prompt = capability_authorization_prompt(std::slice::from_ref(&grant), &[], 500, 1, 1);
        assert!(prompt.text.contains("\"state\":\"active\""));
        assert!(prompt.text.contains("inspect_office_selection"));
        assert!(!prompt.text.contains("secret-grant-id"));
        assert!(
            prompt
                .text
                .contains("supersedes any older assistant statement")
        );
        assert_eq!(prompt.approved_exact_input_expires_at_unix_ms, None);

        let mut expired = grant.clone();
        expired.expires_at_unix_ms = 200;
        let compact = capability_authorization_prompt(&[expired.clone(), expired], &[], 500, 1, 1);
        assert!(compact.text.contains("\"grant_count\":2"));
        assert!(!compact.text.contains("target:current_device"));
        assert!(!compact.text.contains("expires_at_unix_ms"));
        let stale_focus = capability_authorization_prompt(&[grant], &[], 500, 2, 1);
        assert!(stale_focus.text.contains("inspect_office_selection"));
        assert_eq!(stale_focus.approved_exact_input_expires_at_unix_ms, None);
    }

    #[test]
    fn authorization_projection_recovers_only_active_approved_exact_input() {
        use desk_agent_protocol::capability_grant::{
            CAPABILITY_GRANT_SCHEMA_VERSION, CapabilityGrantIssuer, CapabilityGrantLimits,
            CapabilityGrantUsePolicy, CapabilityRiskTier,
        };
        use desk_agent_protocol::capability_provider::ProductSurface;

        let canonical_input = r#"{"element":{"element_id":"element-1"},"value":"approved"}"#;
        let digest = format!("{:x}", Sha256::digest(canonical_input.as_bytes()));
        let grant = CapabilityGrant {
            schema_version: CAPABILITY_GRANT_SCHEMA_VERSION,
            grant_id: "secret-exact-grant-id".into(),
            actor_id: "owner".into(),
            run_id: "run".into(),
            input_revision: 1,
            surface: ProductSurface::OssPersonalOwner,
            target_device_id: "device".into(),
            target_session_id: None,
            provider_id: "browser.extension_mcp".into(),
            capability_id: "browser.activate".into(),
            tool_name: "browser_activate_element".into(),
            tool_schema_version: 1,
            effect: CapabilityEffect::InputFallback,
            risk_tier: CapabilityRiskTier::R3,
            resource_scope: vec!["browser:current_profile".into()],
            operation_scope: vec!["activate_element".into()],
            export_destinations: Vec::new(),
            allowed_envelope_ids: Vec::new(),
            allowed_content_digests_sha256: Vec::new(),
            use_policy: CapabilityGrantUsePolicy::OneShotExact,
            canonical_input_digest_sha256: Some(digest.clone()),
            issued_by: CapabilityGrantIssuer::UserDecision,
            issued_at_unix_ms: 100,
            expires_at_unix_ms: 1_000,
            remaining_uses: 1,
            limits: CapabilityGrantLimits {
                max_bytes_per_call: 1024,
                max_items_per_call: 1,
                max_calls: 1,
            },
            policy_revision: 1,
            readiness_revision: 1,
            revoked_at_unix_ms: None,
            revoked_reason: None,
        };
        let request = PermissionRequest {
            schema_version: PERMISSION_REQUEST_SCHEMA_VERSION,
            request_id: "permission-exact".into(),
            input_revision: 1,
            state: PermissionRequestState::Approved,
            items: vec![GrantRequestItem {
                command_confirmation: None,
                item_id: "activate".into(),
                provider_id: grant.provider_id.clone(),
                tool_name: grant.tool_name.clone(),
                expected_effect: grant.effect,
                resource_scope: grant.resource_scope.clone(),
                operation_scope: grant.operation_scope.clone(),
                export_destinations: Vec::new(),
                canonical_input_json: Some(canonical_input.into()),
                canonical_input_digest_sha256: Some(digest),
                suggested_ttl_seconds: 300,
                suggested_max_uses: 1,
                reason: "Activate the approved element".into(),
            }],
            created_at: "2026-08-28T00:00:00Z".into(),
        };

        let active = capability_authorization_prompt(
            std::slice::from_ref(&grant),
            std::slice::from_ref(&request),
            500,
            1,
            1,
        );
        assert!(active.text.contains("\"approved_exact_input\""));
        assert!(active.text.contains("\"value\":\"approved\""));
        assert!(!active.text.contains("secret-exact-grant-id"));
        assert_eq!(active.approved_exact_input_expires_at_unix_ms, Some(1_000));
        assert_eq!(
            active_exact_authorized_tool_names(
                std::slice::from_ref(&grant),
                std::slice::from_ref(&request),
                500,
                1,
                1,
            ),
            vec!["browser_activate_element"]
        );

        assert!(
            !active_exact_authorized_tool_names(
                std::slice::from_ref(&grant),
                std::slice::from_ref(&request),
                500,
                2,
                1,
            )
            .is_empty()
        );

        let mut reusable_exact = grant.clone();
        reusable_exact.use_policy = CapabilityGrantUsePolicy::Reusable;
        let reusable = capability_authorization_prompt(
            &[reusable_exact],
            std::slice::from_ref(&request),
            500,
            1,
            1,
        );
        assert!(reusable.text.contains("\"approved_exact_input\""));
        assert!(reusable.text.contains("\"value\":\"approved\""));
        assert_eq!(
            reusable.approved_exact_input_expires_at_unix_ms,
            Some(1_000)
        );

        for inactive in [
            {
                let mut value = grant.clone();
                value.remaining_uses = 0;
                value
            },
            {
                let mut value = grant.clone();
                value.revoked_at_unix_ms = Some(400);
                value.revoked_reason = Some("owner revoked".into());
                value
            },
            {
                let mut value = grant.clone();
                value.expires_at_unix_ms = 500;
                value
            },
        ] {
            // Keep the stored contract internally valid for every state fixture.
            inactive.validate().unwrap();
            let projection = capability_authorization_prompt(
                std::slice::from_ref(&inactive),
                std::slice::from_ref(&request),
                500,
                1,
                1,
            );
            assert!(!projection.text.contains("\"approved_exact_input\""));
            assert_eq!(projection.approved_exact_input_expires_at_unix_ms, None);
            assert!(
                active_exact_authorized_tool_names(
                    &[inactive],
                    std::slice::from_ref(&request),
                    500,
                    1,
                    1,
                )
                .is_empty()
            );
        }

        let mut stale_readiness = grant.clone();
        stale_readiness.readiness_revision = 2;
        let stale = capability_authorization_prompt(
            &[stale_readiness],
            std::slice::from_ref(&request),
            500,
            1,
            1,
        );
        assert!(stale.text.contains("\"state\":\"stale_readiness\""));
        assert!(!stale.text.contains("\"approved_exact_input\""));
        assert_eq!(stale.approved_exact_input_expires_at_unix_ms, None);

        let legacy_flat = r#"{"target":{"host_ascii":"app.slack.com","kind":"https","port":443,"url":"https://app.slack.com/"}}"#;
        let legacy_digest = format!("{:x}", Sha256::digest(legacy_flat.as_bytes()));
        let mut legacy_grant = grant;
        legacy_grant.provider_id = "browser.page.open".into();
        legacy_grant.capability_id = "browser.page.open".into();
        legacy_grant.tool_name = "browser_open_page".into();
        legacy_grant.canonical_input_digest_sha256 = Some(legacy_digest.clone());
        let mut legacy_request = request;
        legacy_request.items[0].provider_id = legacy_grant.provider_id.clone();
        legacy_request.items[0].tool_name = legacy_grant.tool_name.clone();
        legacy_request.items[0].canonical_input_json = Some(legacy_flat.into());
        legacy_request.items[0].canonical_input_digest_sha256 = Some(legacy_digest);
        let incompatible =
            capability_authorization_prompt(&[legacy_grant], &[legacy_request], 500, 1, 1);
        assert!(
            incompatible
                .text
                .contains("\"state\":\"schema_incompatible\"")
        );
        assert!(!incompatible.text.contains("\"approved_exact_input\""));
        assert_eq!(incompatible.approved_exact_input_expires_at_unix_ms, None);
    }

    #[test]
    fn outlook_external_draft_permission_is_exact_one_shot_and_destination_bound() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let request = build_permission_request(
            &call(
                r#"{"items":[{"item_id":"outlook","provider_id":"communication.outlook_new.handoff","tool_name":"prepare_outlook_new_draft_handoff","expected_effect":"write_external_draft","resource_scope":[],"operation_scope":[],"export_destinations":[],"exact_input":{"draft":{"schema_version":4,"recipients":[{"role":"to","address":"review@example.invalid","display_name":null}],"subject":"Review","body_plain_text":"Please review","attachment_labels":[]}},"suggested_ttl_seconds":300,"suggested_max_uses":5,"reason":"Prepare a manual Outlook draft"}]}"#,
            ),
            &registry,
            "permission-outlook".into(),
            1,
            "2026-08-27T00:00:00Z".into(),
        )
        .unwrap();
        let item = &request.items[0];
        assert_eq!(item.suggested_max_uses, 1);
        assert!(item.canonical_input_digest_sha256.is_some());
        assert_eq!(
            item.export_destinations,
            vec![
                desk_agent_protocol::data_lineage::DestinationIdentity::EmailAccount {
                    account_id: crate::device_assistant::OUTLOOK_NEW_UNVERIFIED_ACCOUNT_ID.into(),
                }
            ]
        );

        let invalid = build_permission_request(
            &call(
                r#"{"items":[{"item_id":"outlook","provider_id":"communication.outlook_new.handoff","tool_name":"prepare_outlook_new_draft_handoff","expected_effect":"write_external_draft","exact_input":{"schema_version":4,"draft":{"schema_version":4,"recipients":[{"role":"to","address":"review@example.invalid","display_name":null}],"subject":"Review","body_plain_text":"Please review","attachment_labels":[]}},"suggested_ttl_seconds":300,"suggested_max_uses":1,"reason":"Prepare a manual Outlook draft"}]}"#,
            ),
            &registry,
            "permission-outlook-invalid".into(),
            1,
            "2026-08-27T00:00:00Z".into(),
        )
        .unwrap_err();
        assert!(invalid.message.contains("Structural example"));
    }

    #[test]
    fn slack_external_draft_permission_validates_site_and_fixes_destination() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let exact_input = serde_json::json!({
            "schema_version": desk_agent_protocol::communication::COMMUNICATION_SCHEMA_VERSION,
            "page": {
                "schema_version": desk_agent_protocol::browser_control::BROWSER_CONTROL_SCHEMA_VERSION,
                "adapter": {
                    "engine": "chrome_extension",
                    "device_id": "device-1",
                    "os_session_id": "session-1",
                    "browser_major_version": 151,
                    "browser_version": "151.0.0.0",
                    "adapter_id": "lcxl-browser-extension",
                    "adapter_version": "1.7.0",
                    "profile_incarnation": "profile-1",
                    "connection_revision": 7
                },
                "page_id": "page-1",
                "page_incarnation": "page-incarnation-1",
                "origin": {"kind": "https", "host_ascii": "app.slack.com", "port": 443},
                "document_revision": 2,
                "url_sha256": "a".repeat(64),
                "observed_at_unix_ms": 42,
                "account_id": "slack-web:T123:U456"
            },
            "composer": {
                "page_id": "page-1",
                "page_incarnation": "page-incarnation-1",
                "document_revision": 2,
                "element_id": "composer-1",
                "role": "textbox",
                "accessible_name": "Message #test",
                "value": null,
                "element_revision": 1
            },
            "body_plain_text": "Stage 5 draft verification"
        });
        let arguments = serde_json::json!({
            "items": [{
                "item_id": "slack",
                "provider_id": crate::device_assistant::SLACK_WEB_HANDOFF_PROVIDER_ID,
                "tool_name": "prepare_slack_web_message_handoff",
                "expected_effect": "write_external_draft",
                "resource_scope": ["model:chosen"],
                "operation_scope": ["anything"],
                "export_destinations": [],
                "exact_input": exact_input,
                "suggested_ttl_seconds": 300,
                "suggested_max_uses": 5,
                "reason": "Prepare a manual Slack Web draft"
            }]
        })
        .to_string();
        let request = build_permission_request(
            &call(&arguments),
            &registry,
            "permission-slack".into(),
            1,
            "2026-08-27T00:00:00Z".into(),
        )
        .unwrap();
        let item = &request.items[0];
        assert_eq!(item.suggested_max_uses, 1);
        assert!(item.canonical_input_digest_sha256.is_some());
        assert_eq!(
            item.export_destinations,
            vec![
                desk_agent_protocol::data_lineage::DestinationIdentity::ChatAccount {
                    account_id: "slack-web:T123:U456".into(),
                }
            ]
        );

        let mut invalid = serde_json::from_str::<serde_json::Value>(&arguments).unwrap();
        invalid["items"][0]["exact_input"]["page"]["origin"]["host_ascii"] =
            serde_json::Value::String("example.com".into());
        assert!(
            build_permission_request(
                &call(&invalid.to_string()),
                &registry,
                "permission-slack-invalid".into(),
                1,
                "2026-08-27T00:00:00Z".into(),
            )
            .is_err()
        );
    }

    #[test]
    fn gmail_external_draft_permission_validates_fields_and_fixes_destination() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let page = serde_json::json!({
            "schema_version": desk_agent_protocol::browser_control::BROWSER_CONTROL_SCHEMA_VERSION,
            "adapter": {
                "engine": "chrome_extension",
                "device_id": "device-1",
                "os_session_id": "session-1",
                "browser_major_version": 151,
                "browser_version": "151.0.0.0",
                "adapter_id": "lcxl-browser-extension",
                "adapter_version": "1.7.0",
                "profile_incarnation": "profile-1",
                "connection_revision": 7
            },
            "account_id": "gmail-web:alice@example.com",
            "page_id": "page-1",
            "page_incarnation": "page-incarnation-1",
            "origin": {"kind": "https", "host_ascii": "mail.google.com", "port": 443},
            "document_revision": 2,
            "url_sha256": "a".repeat(64),
            "observed_at_unix_ms": 42
        });
        let field = |element_id: &str, accessible_name: &str, role: &str| {
            serde_json::json!({
                "page_id": "page-1",
                "page_incarnation": "page-incarnation-1",
                "document_revision": 2,
                "element_id": element_id,
                "role": role,
                "accessible_name": accessible_name,
                "value": null,
                "element_revision": 1
            })
        };
        let exact_input = serde_json::json!({
            "schema_version": desk_agent_protocol::communication::COMMUNICATION_SCHEMA_VERSION,
            "page": page,
            "to_field": field("to-1", "To recipients", "combobox"),
            "subject_field": field("subject-1", "Subject", "textbox"),
            "body_field": field("body-1", "Message Body", "textbox"),
            "draft": {
                "schema_version": desk_agent_protocol::communication::COMMUNICATION_SCHEMA_VERSION,
                "recipients": [{"role": "to", "address": "alice@example.com", "display_name": null}],
                "subject": "Stage 5 Gmail verification",
                "body_plain_text": "Semantic draft only; do not send.",
                "attachment_labels": []
            }
        });
        let arguments = serde_json::json!({
            "items": [{
                "item_id": "gmail",
                "provider_id": crate::device_assistant::GMAIL_WEB_HANDOFF_PROVIDER_ID,
                "tool_name": "prepare_gmail_web_draft_handoff",
                "expected_effect": "write_external_draft",
                "resource_scope": ["model:chosen"],
                "operation_scope": ["anything"],
                "export_destinations": [],
                "exact_input": exact_input,
                "suggested_ttl_seconds": 300,
                "suggested_max_uses": 5,
                "reason": "Prepare a manual Gmail Web draft"
            }]
        })
        .to_string();
        let request = build_permission_request(
            &call(&arguments),
            &registry,
            "permission-gmail".into(),
            1,
            "2026-08-27T00:00:00Z".into(),
        )
        .unwrap();
        let item = &request.items[0];
        assert_eq!(item.suggested_max_uses, 1);
        assert!(item.canonical_input_digest_sha256.is_some());
        assert_eq!(
            item.export_destinations,
            vec![
                desk_agent_protocol::data_lineage::DestinationIdentity::EmailAccount {
                    account_id: "gmail-web:alice@example.com".into(),
                }
            ]
        );

        let mut invalid = serde_json::from_str::<serde_json::Value>(&arguments).unwrap();
        invalid["items"][0]["exact_input"]["body_field"]["element_id"] =
            invalid["items"][0]["exact_input"]["subject_field"]["element_id"].clone();
        assert!(
            build_permission_request(
                &call(&invalid.to_string()),
                &registry,
                "permission-gmail-invalid".into(),
                1,
                "2026-08-27T00:00:00Z".into(),
            )
            .is_err()
        );
    }

    #[test]
    fn exact_external_send_permission_is_separate_exact_and_one_shot() {
        use crate::{
            communication::test_support::{gmail_exact_send_input, slack_exact_send_input},
            device_assistant::{GMAIL_WEB_SEND_PROVIDER_ID, SLACK_WEB_SEND_PROVIDER_ID},
        };
        use desk_agent_protocol::data_lineage::DestinationIdentity;

        let registry = crate::device_assistant::device_assistant_provider_registry();
        for (tool_name, provider_id, input, expected_destination) in [
            (
                "send_gmail_web_exact",
                GMAIL_WEB_SEND_PROVIDER_ID,
                serde_json::to_value(gmail_exact_send_input()).unwrap(),
                DestinationIdentity::EmailAccount {
                    account_id: "gmail-web:owner@example.test".into(),
                },
            ),
            (
                "send_slack_web_exact",
                SLACK_WEB_SEND_PROVIDER_ID,
                serde_json::to_value(slack_exact_send_input()).unwrap(),
                DestinationIdentity::ChatAccount {
                    account_id: "slack-web:T123:U456".into(),
                },
            ),
        ] {
            let item = serde_json::json!({
                "item_id": "send",
                "provider_id": provider_id,
                "tool_name": tool_name,
                "expected_effect": "send_external",
                "resource_scope": ["forged:resource"],
                "operation_scope": ["forged_operation"],
                "export_destinations": [{"kind":"email_account","account_id":"forged"}],
                "exact_input": input,
                "suggested_ttl_seconds": 600,
                "suggested_max_uses": 99,
                "reason": "Send the exact reviewed payload"
            });
            let request = build_permission_request(
                &call(&serde_json::json!({"items":[item.clone()]}).to_string()),
                &registry,
                format!("permission-{tool_name}"),
                1,
                "2026-09-03T00:00:00Z".into(),
            )
            .unwrap();
            let stored = &request.items[0];
            assert_eq!(stored.expected_effect, CapabilityEffect::SendExternal);
            assert_eq!(stored.suggested_max_uses, 1);
            assert_eq!(stored.operation_scope, ["use_selected_object"]);
            assert_eq!(stored.export_destinations, [expected_destination]);
            assert!(stored.canonical_input_json.is_some());
            assert!(stored.canonical_input_digest_sha256.is_some());

            for account in [
                serde_json::Value::Null,
                serde_json::json!("another-account"),
            ] {
                let mut changed = item.clone();
                changed["exact_input"]["page"]["account_id"] = account;
                assert!(
                    build_permission_request(
                        &call(&serde_json::json!({"items": [changed]}).to_string()),
                        &registry,
                        format!("permission-{tool_name}-wrong-account"),
                        1,
                        "2026-09-03T00:00:00Z".into(),
                    )
                    .is_err()
                );
            }

            let mut missing = item.clone();
            missing.as_object_mut().unwrap().remove("exact_input");
            assert!(
                build_permission_request(
                    &call(&serde_json::json!({"items":[missing]}).to_string()),
                    &registry,
                    "permission-missing".into(),
                    1,
                    "2026-09-03T00:00:00Z".into(),
                )
                .is_err()
            );

            let mixed = serde_json::json!({
                "items": [
                    item,
                    {
                        "item_id":"inspect",
                        "provider_id":"desktop.session",
                        "tool_name":"inspect_desktop_session",
                        "expected_effect":"read_device",
                        "suggested_ttl_seconds":60,
                        "suggested_max_uses":1,
                        "reason":"Inspect"
                    }
                ]
            });
            let error = build_permission_request(
                &call(&mixed.to_string()),
                &registry,
                "permission-mixed".into(),
                1,
                "2026-09-03T00:00:00Z".into(),
            )
            .unwrap_err();
            assert!(error.message.contains("must be requested separately"));
        }
    }
}

/// Deterministic control text only; this is not a grant or current policy snapshot.
pub(crate) fn existing_request_result(request_id: &str, decision_state: &str) -> serde_json::Value {
    json!({"status":"existing_permission_request", "decision_state":decision_state,
        "request_id":request_id, "authority":"unchanged",
        "message":"An authority-equivalent permission batch already exists for this input revision. Do not request it again; use the current authorization snapshot or adapt to the recorded decision."})
}
