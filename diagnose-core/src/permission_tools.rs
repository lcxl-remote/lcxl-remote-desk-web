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
    canonical_compiled_scope, exact_command_resource_scope, exact_external_query_resource_scope,
    exact_external_url_resource_scope, fresh_object_resource_scope,
};
use crate::chat::{ToolCall, ToolSpec};
use crate::device_assistant::DUCKDUCKGO_HTML_CONNECTOR_ID;
use crate::dynamic_run::{
    GrantRequestItem, MAX_PERMISSION_REASON_BYTES, MAX_PERMISSION_REQUEST_ITEMS,
    MAX_PERMISSION_SCOPE_VALUES, PERMISSION_REQUEST_SCHEMA_VERSION, PermissionRequest,
    PermissionRequestState,
};
use crate::exec_classify::classify_command;
use crate::exec_tools::canonical_exec_shell;
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
    current_readiness_revision: u64,
) -> CapabilityAuthorizationPrompt {
    let mut approved_exact_input_expires_at_unix_ms: Option<u64> = None;
    let mut entries = Vec::with_capacity(grants.len());
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
            "The following JSON authorization snapshot is server-authored for this run and supersedes any older assistant statement that a permission request is still pending. It does not widen the current tool list and does not itself dispatch anything. When a tool is present in the current tool list and has state=active here, do not refuse it based on stale permission text in conversation history; call it when the user requested it and let the server authorizer perform the final match. For any active grant bound to an exact input, approved_exact_input is the immutable server-canonicalized JSON the owner approved: use it as that tool's arguments without adding, removing, or changing any field, never repeat it in prose, and never reuse it beyond remaining_uses. Exact input is deliberately omitted for every non-active or non-exact grant. Never invent or reveal a grant id.\n<capability_authorization>{}</capability_authorization>",
            serde_json::to_string(&entries).expect("authorization projection is serializable")
        ),
        approved_exact_input_expires_at_unix_ms,
    }
}

fn approved_exact_input(
    grant: &CapabilityGrant,
    permission_requests: &[PermissionRequest],
) -> Option<(serde_json::Value, String)> {
    let digest = grant.canonical_input_digest_sha256.as_deref()?;
    permission_requests
        .iter()
        .filter(|request| {
            matches!(
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

/// Canonicalize one tool input after expanding server-owned JSON-schema
/// defaults whose omission is semantically identical to the explicit value.
/// Keep this list closed: adding an entry changes exact-grant matching and must
/// be backed by a matching runtime default and regression test.
pub fn canonical_tool_permission_input_json(
    tool_name: &str,
    mut value: serde_json::Value,
) -> Result<String, serde_json::Error> {
    if tool_name == "search_public_web"
        && let serde_json::Value::Object(input) = &mut value
    {
        input
            .entry("max_results".to_string())
            .or_insert_with(|| serde_json::json!(5));
    }
    canonical_permission_input_json(value)
}

/// The placeholder capability is ignored by PermissionPlanning exposure. It
/// exists only because RegisteredTool deliberately requires a closed capability.
pub fn permission_planning_tool_registry() -> Vec<RegisteredTool> {
    vec![RegisteredTool {
        spec: ToolSpec {
            name: REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),
            description: "Ask the user for one bounded batch of tool permissions. This only creates a pending request: it does not grant, reserve, invoke, or retry any tool. Prefer one batch for all currently-known inputs, then request another only when intermediate results provide new exact inputs. Never supply an export destination: every destination is derived and fixed by the registered Provider on the server.".into(),
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
                                    "send_external", "capture_screen", "input_fallback", "execute_command"
                                ]},
                                "resource_scope": {"type": "array", "maxItems": MAX_PERMISSION_SCOPE_VALUES, "items": {"type": "string", "maxLength": 512}},
                                "operation_scope": {"type": "array", "maxItems": MAX_PERMISSION_SCOPE_VALUES, "items": {"type": "string", "maxLength": 512}},
                                "exact_input": {"type": "object", "description": "Required only for write_external_draft, send_external, input_fallback, execute_command, and formula-workbook creation. Omit it for ordinary read_file, write_artifact, and mutate_application requests unless that tool description explicitly requires it."},
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
        if !matches!(
            item.expected_effect,
            CapabilityEffect::ExportData
                | CapabilityEffect::WriteExternalDraft
                | CapabilityEffect::SendExternal
        ) && !item.export_destinations.is_empty()
        {
            return Err(invalid(
                "export_destinations are only valid for external egress effects",
            ));
        }
        let (canonical_input_json, canonical_input_digest_sha256) = match item.exact_input {
            Some(input) => {
                let canonical = canonical_tool_permission_input_json(&item.tool_name, input)
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
            item.expected_effect,
            CapabilityEffect::SendExternal
                | CapabilityEffect::WriteExternalDraft
                | CapabilityEffect::InputFallback
                | CapabilityEffect::ExecuteCommand
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
        let exact_semantic_action = matches!(
            capability.wire.capability_id.as_str(),
            crate::device_assistant::DESKTOP_UI_ACTION_CAPABILITY_ID
                | crate::device_assistant::DESKTOP_RAW_INPUT_CAPABILITY_ID
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
                .ok_or_else(|| invalid("semantic actions require exact_input"))?;
            let input: SemanticActionInput = serde_json::from_str(canonical)
                .map_err(|error| invalid(format!("decode semantic action input: {error}")))?;
            let expected_kind = match capability.required_capability {
                Capability::DesktopUiActionConfirmed => {
                    desk_agent_protocol::computer_use::ObjectKind::UiElement
                }
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
                || input.target.expires_at.is_empty()
            {
                return Err(invalid(
                    "semantic action requires one complete target reference of the expected kind",
                ));
            }
            if capability.required_capability == Capability::DesktopUiActionConfirmed {
                #[derive(Deserialize)]
                struct UiActionOnly {
                    action: desk_agent_protocol::computer_use::UiSemanticAction,
                }
                let action: UiActionOnly = serde_json::from_str(canonical).map_err(|error| {
                    invalid(format!("decode semantic UI action input: {error}"))
                })?;
                match action.action {
                    desk_agent_protocol::computer_use::UiSemanticAction::Invoke
                    | desk_agent_protocol::computer_use::UiSemanticAction::Toggle { .. }
                    | desk_agent_protocol::computer_use::UiSemanticAction::Select
                    | desk_agent_protocol::computer_use::UiSemanticAction::Focus => {}
                    desk_agent_protocol::computer_use::UiSemanticAction::SetValue { value }
                        if value.len() <= 16 * 1024 => {}
                    _ => {
                        return Err(invalid(
                            "semantic UI action is not in the bounded macOS action allowlist",
                        ));
                    }
                }
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
        }
        if (exact_external_url || exact_external_query || exact_command)
            && canonical_input_digest_sha256.is_none()
        {
            return Err(invalid(
                "exact URL/query/command permissions require exact_input so the approved input is immutable",
            ));
        }
        if exact_command {
            let canonical = canonical_input_json
                .as_deref()
                .expect("ExactCommand exact input was checked");
            let draft: desk_agent_protocol::exec::CommandDraft = serde_json::from_str(canonical)
                .map_err(|error| invalid(format!("decode exact command input: {error}")))?;
            draft
                .validate()
                .map_err(|error| invalid(format!("validate exact command input: {error}")))?;
            let shell = canonical_exec_shell(&draft.shell)
                .ok_or_else(|| invalid("exact command shell is not supported"))?;
            let input = desk_agent_protocol::ExecInput {
                target: desk_agent_protocol::ExecTarget::Shell {
                    shell: shell.to_string(),
                },
                command: draft.command,
                cwd: draft.cwd,
                timeout_ms: draft.timeout_ms,
                max_stdout_bytes: 65_536,
                max_stderr_bytes: 65_536,
            };
            let classified = classify_command(&input);
            if classified.classification.decision
                != desk_agent_protocol::exec::ExecDecision::ConfirmRequired
                || classified.draft.as_ref().is_none_or(|plan| {
                    plan.execution_basis != desk_agent_protocol::exec::ExecExecutionBasis::Template
                })
            {
                return Err(invalid(
                    "exact command does not match a server-owned safe template",
                ));
            }
        }
        let compiled_scope = canonical_compiled_scope(
            &capability.wire.authorization_hint.resources,
            capability.wire.effect,
        );
        let resource_scope = if let Some(targets) = exact_semantic_refs.as_ref() {
            fresh_object_resource_scope(targets)
        } else if exact_external_url {
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
        } else if exact_command {
            exact_command_resource_scope(
                canonical_input_digest_sha256
                    .as_deref()
                    .expect("ExactCommand exact input was checked"),
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
                        account_id: crate::device_assistant::GMAIL_WEB_CURRENT_PROFILE_ACCOUNT_ID
                            .into(),
                    },
                ]
            } else if exact_slack_handoff {
                vec![
                    desk_agent_protocol::data_lineage::DestinationIdentity::ChatAccount {
                        account_id: crate::device_assistant::SLACK_WEB_CURRENT_PROFILE_ACCOUNT_ID
                            .into(),
                    },
                ]
            } else {
                item.export_destinations
            },
            canonical_input_json,
            canonical_input_digest_sha256,
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

        assert!(
            error
                .message
                .contains("does not match the current closed tool contract")
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
                .contains("Omit it for ordinary read_file, write_artifact")
        );
    }

    #[test]
    fn command_permission_requires_exact_input_and_forces_one_shot_scope() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
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
        assert!(
            error
                .message
                .contains("does not match a server-owned safe template")
        );
    }

    #[test]
    fn input_fallback_permission_is_narrowed_to_one_shot_before_pending() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let request = build_permission_request(
            &call(
                r#"{"items":[{"item_id":"activate","provider_id":"browser.element.activate","tool_name":"browser_activate_element","expected_effect":"input_fallback","exact_input":{"page":"provider-owned-page","element":"provider-owned-element"},"suggested_ttl_seconds":300,"suggested_max_uses":2,"reason":"Activate the selected semantic element once"}]}"#,
            ),
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
    fn semantic_ui_permission_binds_one_fresh_target_and_exact_action() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let missing = r#"{"items":[{"item_id":"ui","provider_id":"desktop.ui.action","tool_name":"execute_confirmed_ui_action","expected_effect":"mutate_application","suggested_ttl_seconds":60,"suggested_max_uses":4,"reason":"Update the selected control"}]}"#;
        let error = build_permission_request(
            &call(missing),
            &registry,
            "permission-ui-missing".into(),
            1,
            "2026-08-28T00:00:00Z".into(),
        )
        .unwrap_err();
        assert!(error.message.contains("require exact_input"));

        let exact = r#"{"items":[{"item_id":"ui","provider_id":"desktop.ui.action","tool_name":"execute_confirmed_ui_action","expected_effect":"mutate_application","resource_scope":["model:chosen"],"operation_scope":["anything"],"exact_input":{"target":{"token":"opaque-token","snapshot_id":"snapshot-1","object_kind":"ui_element","expires_at":"2026-08-28T00:01:00Z"},"action":{"kind":"set_value","params":{"value":"Ready"}}},"suggested_ttl_seconds":60,"suggested_max_uses":4,"reason":"Update the selected control"}]}"#;
        let request = build_permission_request(
            &call(exact),
            &registry,
            "permission-ui".into(),
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
        assert!(
            !item
                .resource_scope
                .iter()
                .any(|scope| scope == "model:chosen")
        );

        let toggle = exact.replace(
            r#"{"kind":"set_value","params":{"value":"Ready"}}"#,
            r#"{"kind":"toggle","params":{"desired":true}}"#,
        );
        let toggle_request = build_permission_request(
            &call(&toggle),
            &registry,
            "permission-ui-toggle".into(),
            1,
            "2026-08-28T00:00:00Z".into(),
        )
        .unwrap();
        assert_eq!(toggle_request.items[0].suggested_max_uses, 1);
        assert_eq!(toggle_request.items[0].resource_scope, item.resource_scope);

        let unsupported = exact.replace(
            r#"{"kind":"set_value","params":{"value":"Ready"}}"#,
            r#"{"kind":"scroll","params":{"horizontal":0,"vertical":1}}"#,
        );
        let error = build_permission_request(
            &call(&unsupported),
            &registry,
            "permission-ui-scroll".into(),
            1,
            "2026-08-28T00:00:00Z".into(),
        )
        .unwrap_err();
        assert!(error.message.contains("bounded macOS action allowlist"));
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
        let prompt = capability_authorization_prompt(&[grant], &[], 500, 1);
        assert!(prompt.text.contains("\"state\":\"active\""));
        assert!(prompt.text.contains("inspect_office_selection"));
        assert!(!prompt.text.contains("secret-grant-id"));
        assert!(
            prompt
                .text
                .contains("supersedes any older assistant statement")
        );
        assert_eq!(prompt.approved_exact_input_expires_at_unix_ms, None);
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
            surface: ProductSurface::OssPersonalOwner,
            target_device_id: "device".into(),
            target_session_id: None,
            provider_id: "browser.devtools_mcp".into(),
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
        );
        assert!(active.text.contains("\"approved_exact_input\""));
        assert!(active.text.contains("\"value\":\"approved\""));
        assert!(!active.text.contains("secret-exact-grant-id"));
        assert_eq!(active.approved_exact_input_expires_at_unix_ms, Some(1_000));

        let mut reusable_exact = grant.clone();
        reusable_exact.use_policy = CapabilityGrantUsePolicy::Reusable;
        let reusable =
            capability_authorization_prompt(&[reusable_exact], &[request.clone()], 500, 1);
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
            let projection =
                capability_authorization_prompt(&[inactive], &[request.clone()], 500, 1);
            assert!(!projection.text.contains("\"approved_exact_input\""));
            assert_eq!(projection.approved_exact_input_expires_at_unix_ms, None);
        }

        let mut stale_readiness = grant.clone();
        stale_readiness.readiness_revision = 2;
        let stale = capability_authorization_prompt(&[stale_readiness], &[request.clone()], 500, 1);
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
            capability_authorization_prompt(&[legacy_grant], &[legacy_request], 500, 1);
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
                r#"{"items":[{"item_id":"outlook","provider_id":"communication.outlook_new.handoff","tool_name":"prepare_outlook_new_draft_handoff","expected_effect":"write_external_draft","resource_scope":[],"operation_scope":[],"export_destinations":[],"exact_input":{"draft":{"schema_version":3,"recipients":[{"role":"to","address":"review@example.invalid","display_name":null}],"subject":"Review","body_plain_text":"Please review","attachment_labels":[]}},"suggested_ttl_seconds":300,"suggested_max_uses":5,"reason":"Prepare a manual Outlook draft"}]}"#,
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
                r#"{"items":[{"item_id":"outlook","provider_id":"communication.outlook_new.handoff","tool_name":"prepare_outlook_new_draft_handoff","expected_effect":"write_external_draft","exact_input":{"schema_version":3,"draft":{"schema_version":3,"recipients":[{"role":"to","address":"review@example.invalid","display_name":null}],"subject":"Review","body_plain_text":"Please review","attachment_labels":[]}},"suggested_ttl_seconds":300,"suggested_max_uses":1,"reason":"Prepare a manual Outlook draft"}]}"#,
            ),
            &registry,
            "permission-outlook-invalid".into(),
            1,
            "2026-08-27T00:00:00Z".into(),
        )
        .unwrap_err();
        assert!(
            invalid
                .message
                .contains("decode Outlook (new) handoff input")
        );
    }

    #[test]
    fn slack_external_draft_permission_validates_site_and_fixes_destination() {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let exact_input = serde_json::json!({
            "schema_version": desk_agent_protocol::communication::COMMUNICATION_SCHEMA_VERSION,
            "page": {
                "schema_version": desk_agent_protocol::browser_control::BROWSER_CONTROL_SCHEMA_VERSION,
                "adapter": {
                    "engine": "chrome_devtools_mcp",
                    "device_id": "device-1",
                    "os_session_id": "session-1",
                    "browser_major_version": 151,
                    "browser_version": "151.0.0.0",
                    "adapter_id": "chrome-devtools-mcp",
                    "adapter_version": "1.7.0",
                    "profile_incarnation": "profile-1",
                    "connection_revision": 7
                },
                "page_id": "page-1",
                "page_incarnation": "page-incarnation-1",
                "origin": {"kind": "https", "host_ascii": "app.slack.com", "port": 443},
                "document_revision": 2,
                "url_sha256": "a".repeat(64),
                "observed_at_unix_ms": 42
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
                    account_id: crate::device_assistant::SLACK_WEB_CURRENT_PROFILE_ACCOUNT_ID
                        .into(),
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
                "engine": "chrome_devtools_mcp",
                "device_id": "device-1",
                "os_session_id": "session-1",
                "browser_major_version": 151,
                "browser_version": "151.0.0.0",
                "adapter_id": "chrome-devtools-mcp",
                "adapter_version": "1.7.0",
                "profile_incarnation": "profile-1",
                "connection_revision": 7
            },
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
                    account_id: crate::device_assistant::GMAIL_WEB_CURRENT_PROFILE_ACCOUNT_ID
                        .into(),
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
}
