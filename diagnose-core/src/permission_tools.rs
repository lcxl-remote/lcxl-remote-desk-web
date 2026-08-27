//! Internal permission-planning control tool.
//!
//! The model may propose a bounded batch, but the server resolves every
//! provider/tool/effect against the compiled registry and only persists a
//! `PermissionRequest`. This module cannot create a grant or dispatch work.

use std::collections::BTreeSet;

use desk_agent_protocol::capability_grant::CapabilityGrant;
use desk_agent_protocol::capability_provider::{AuthorizationResourceKind, CapabilityEffect};
use desk_agent_protocol::{AgentError, AgentErrorKind, Capability};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::capability_availability::CapabilityAvailability;
use crate::capability_grant::{
    canonical_compiled_scope, exact_external_query_resource_scope,
    exact_external_url_resource_scope,
};
use crate::chat::{ToolCall, ToolSpec};
use crate::device_assistant::DUCKDUCKGO_HTML_CONNECTOR_ID;
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

/// Render the current server-owned authorization projection for the model.
///
/// Permission decisions are durable run events rather than conversation text,
/// so an older assistant message may still say that a request was pending.
/// Re-projecting grants on every turn gives the model current authority facts
/// without exposing grant ids or trusting model-maintained history. Actual
/// dispatch still has to pass the grant matcher and transactional reservation.
pub fn capability_authorization_prompt(grants: &[CapabilityGrant], now_unix_ms: u64) -> String {
    let entries = grants
        .iter()
        .map(|grant| {
            let state = if grant.revoked_at_unix_ms.is_some() {
                "revoked"
            } else if grant.expires_at_unix_ms <= now_unix_ms {
                "expired"
            } else if grant.remaining_uses == 0 {
                "exhausted"
            } else {
                "active"
            };
            json!({
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
            })
        })
        .collect::<Vec<_>>();
    format!(
        "The following JSON authorization snapshot is server-authored for this run and supersedes any older assistant statement that a permission request is still pending. It does not widen the current tool list and does not itself dispatch anything. When a tool is present in the current tool list and has state=active here, do not refuse it based on stale permission text in conversation history; call it when the user requested it and let the server authorizer perform the final match. Never invent or reveal a grant id.\n<capability_authorization>{}</capability_authorization>",
        serde_json::to_string(&entries).expect("authorization projection is serializable")
    )
}

/// Build the exact server-owned capability catalog shown to the model. The
/// compiled descriptor supplies identity/schema/effect while the live inventory
/// supplies edge readiness. `callable_now` is derived from the final per-turn
/// model registry, so an installed-but-unselected capability is never presented
/// as directly callable.
pub fn discoverable_catalog_prompt(
    registry: &ProviderRegistry,
    inventory: &[CapabilityAvailability],
    callable_tools: &[RegisteredTool],
) -> String {
    let callable = callable_tools
        .iter()
        .map(|tool| tool.name())
        .collect::<BTreeSet<_>>();
    let entries = registry
        .providers()
        .flat_map(|provider| {
            provider.capabilities.iter().map(|capability| {
                let availability = inventory.iter().find(|item| {
                    item.provider_id == provider.wire.provider_id
                        && item.capability_id == capability.wire.capability_id
                });
                let runtime_ready = availability.is_some_and(CapabilityAvailability::callable);
                json!({
                    "provider_id": provider.wire.provider_id,
                    "capability_id": capability.wire.capability_id,
                    "tool_name": capability.tool_spec.name,
                    "effect": capability.wire.effect,
                    "execution_locality": capability.wire.execution_locality,
                    "execution_policy": capability.wire.execution_policy,
                    "supports_progress": capability.wire.supports_progress,
                    "supports_cancel": capability.wire.supports_cancel,
                    "runtime_ready": runtime_ready,
                    "callable_now": runtime_ready && callable.contains(capability.tool_spec.name.as_str()),
                    "blocked_reason": availability.and_then(|item| item.reason),
                    "description": capability.tool_spec.description,
                    "input_schema": capability.tool_spec.parameters_schema,
                })
            })
        })
        .collect::<Vec<_>>();
    format!(
        "The following JSON capability catalog is server-authored. Treat it as authority for what is compiled, runtime-ready, and callable in this turn. Never invent provider ids, tool names, effects, scopes, or readiness. A capability with runtime_ready=false cannot be made usable by asking for permission. A capability with callable_now=false is not callable in this turn. request_capability_grants only records a pending user decision and does not execute or widen the current tool list.\n<capability_catalog>{}</capability_catalog>",
        serde_json::to_string(&entries).expect("catalog contains only serializable descriptors")
    )
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
    provider_id: String,
    tool_name: String,
    expected_effect: CapabilityEffect,
    #[serde(default)]
    resource_scope: Vec<String>,
    #[serde(default)]
    operation_scope: Vec<String>,
    #[serde(default)]
    export_destinations: Vec<desk_agent_protocol::data_lineage::DestinationIdentity>,
    #[serde(default)]
    exact_input: Option<serde_json::Value>,
    suggested_ttl_seconds: u32,
    suggested_max_uses: u32,
    reason: String,
}

fn invalid(detail: impl std::fmt::Display) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: format!("invalid request_capability_grants arguments: {detail}"),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

/// The placeholder capability is ignored by PermissionPlanning exposure. It
/// exists only because RegisteredTool deliberately requires a closed capability.
pub fn permission_planning_tool_registry() -> Vec<RegisteredTool> {
    vec![RegisteredTool {
        spec: ToolSpec {
            name: REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),
            description: "Ask the user for one bounded batch of tool permissions. This only creates a pending request: it does not grant, reserve, invoke, or retry any tool. Prefer one batch after read-only research; request additional items later only when new facts require them.".into(),
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
                                "provider_id": {"type": "string", "maxLength": 128},
                                "tool_name": {"type": "string", "maxLength": 128},
                                "expected_effect": {"type": "string", "enum": [
                                    "read_device", "read_file", "read_external", "export_data",
                                    "write_artifact", "mutate_application", "write_external_draft",
                                    "send_external", "capture_screen", "input_fallback"
                                ]},
                                "resource_scope": {"type": "array", "maxItems": MAX_PERMISSION_SCOPE_VALUES, "items": {"type": "string", "maxLength": 512}},
                                "operation_scope": {"type": "array", "maxItems": MAX_PERMISSION_SCOPE_VALUES, "items": {"type": "string", "maxLength": 512}},
                                "export_destinations": {"type": "array", "maxItems": MAX_PERMISSION_SCOPE_VALUES, "items": {"type": "object"}},
                                "exact_input": {"type": "object", "description": "Exact server-canonicalized tool input required for inherently R3 calls"},
                                "suggested_ttl_seconds": {"type": "integer", "minimum": 1},
                                "suggested_max_uses": {"type": "integer", "minimum": 1},
                                "reason": {"type": "string", "maxLength": MAX_PERMISSION_REASON_BYTES}
                            },
                            "required": ["item_id", "provider_id", "tool_name", "expected_effect", "suggested_ttl_seconds", "suggested_max_uses", "reason"],
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

    let mut items = Vec::with_capacity(params.items.len());
    for item in params.items {
        let capability = registry
            .capability_for_tool(&item.tool_name)
            .ok_or_else(|| invalid(format!("unknown tool `{}`", item.tool_name)))?;
        let provider = registry
            .provider_for_capability(&capability.wire.capability_id)
            .expect("compiled capability has a provider");
        if provider.wire.provider_id != item.provider_id {
            return Err(invalid(format!(
                "tool `{}` belongs to provider `{}`",
                item.tool_name, provider.wire.provider_id
            )));
        }
        if capability.wire.effect != item.expected_effect {
            return Err(invalid(format!(
                "tool `{}` effect does not match the compiled descriptor",
                item.tool_name
            )));
        }
        if item.expected_effect != CapabilityEffect::ExportData
            && !item.export_destinations.is_empty()
        {
            return Err(invalid("export_destinations are only valid for ExportData"));
        }
        let (canonical_input_json, canonical_input_digest_sha256) = match item.exact_input {
            Some(input) => {
                let canonical = serde_json::to_string(&input)
                    .map_err(|error| invalid(format!("canonicalize exact_input: {error}")))?;
                if canonical.len() > crate::dynamic_run::MAX_PERMISSION_EXACT_INPUT_BYTES {
                    return Err(invalid("exact_input exceeds the bounded storage limit"));
                }
                let digest = format!("{:x}", Sha256::digest(canonical.as_bytes()));
                (Some(canonical), Some(digest))
            }
            None => (None, None),
        };
        if matches!(
            item.expected_effect,
            CapabilityEffect::SendExternal | CapabilityEffect::InputFallback
        ) && canonical_input_json.is_none()
        {
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
        if (exact_external_url || exact_external_query) && canonical_input_digest_sha256.is_none() {
            return Err(invalid(
                "external URL/query permissions require exact_input so the approved input is immutable",
            ));
        }
        let compiled_scope = canonical_compiled_scope(
            &capability.wire.authorization_hint.resources,
            capability.wire.effect,
        );
        let resource_scope = if exact_external_url {
            exact_external_url_resource_scope(
                canonical_input_digest_sha256
                    .as_deref()
                    .expect("ExternalUrl exact input was checked"),
            )
        } else if exact_external_query {
            exact_external_query_resource_scope(
                canonical_input_digest_sha256
                    .as_deref()
                    .expect("ExternalQuery exact input was checked"),
            )
        } else {
            compiled_scope.as_ref().map_or_else(
                || normalize_scope(item.resource_scope),
                |scope| scope.resources.clone(),
            )
        };
        items.push(GrantRequestItem {
            item_id: item.item_id.trim().to_string(),
            provider_id: item.provider_id,
            tool_name: item.tool_name,
            expected_effect: item.expected_effect,
            resource_scope,
            operation_scope: compiled_scope.map_or_else(
                || normalize_scope(item.operation_scope),
                |scope| scope.operations,
            ),
            export_destinations: if exact_external_query {
                vec![
                    desk_agent_protocol::data_lineage::DestinationIdentity::WebResearch {
                        connector_id: DUCKDUCKGO_HTML_CONNECTOR_ID.into(),
                    },
                ]
            } else {
                item.export_destinations
            },
            canonical_input_json,
            canonical_input_digest_sha256,
            suggested_ttl_seconds: item.suggested_ttl_seconds.clamp(1, MAX_REQUEST_TTL_SECONDS),
            suggested_max_uses: item.suggested_max_uses.clamp(1, MAX_REQUEST_USES),
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
    fn external_query_permission_fixes_input_scope_and_connector_destination() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
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
        assert_eq!(
            item.export_destinations,
            vec![
                desk_agent_protocol::data_lineage::DestinationIdentity::WebResearch {
                    connector_id: crate::device_assistant::DUCKDUCKGO_HTML_CONNECTOR_ID.into(),
                }
            ]
        );
        assert_eq!(
            item.canonical_input_json.as_deref(),
            Some(r#"{"max_results":5,"query":"Rust language"}"#)
        );
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
        let prompt = capability_authorization_prompt(&[grant], 500);
        assert!(prompt.contains("\"state\":\"active\""));
        assert!(prompt.contains("inspect_office_selection"));
        assert!(!prompt.contains("secret-grant-id"));
        assert!(prompt.contains("supersedes any older assistant statement"));
    }
}
