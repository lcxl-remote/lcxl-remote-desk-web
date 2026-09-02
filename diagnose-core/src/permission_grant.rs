//! Pure permission-to-grant compilation shared by OSS signal and Manager.
//!
//! This code has no database or dispatch effects. Callers must authorize the
//! owner and atomically persist the decision, grants, audit and durable resume
//! trigger under the current session/input fence before exposing success.

use crate::input_read_context::{
    ReadContextSelection,
    object_read::{ObjectReadBinding, requires_objects},
};
use crate::{
    capability_availability::CapabilityAvailability,
    capability_risk::{CapabilityRiskSignals, classify_capability_risk},
    context_attachment::{AttachmentState, ContextAttachment, ContextAttachmentKind},
    provider_registry::ProviderRegistry,
    session::PersistedAgentSession,
};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_agent_protocol::{
    capability_grant::{
        CAPABILITY_GRANT_SCHEMA_VERSION, CapabilityGrant, CapabilityGrantIssuer,
        CapabilityGrantLimits, CapabilityGrantUsePolicy,
    },
    capability_provider::{AuthorizationResourceKind, CapabilityDataCategory, ProductSurface},
    computer_use::{ObjectKind, ObjectRef},
    data_lineage::DestinationIdentity,
};
use sha2::{Digest, Sha256};

pub fn requires_original_read_context(
    request: &crate::dynamic_run::PermissionRequest,
    decisions: &[crate::dynamic_run::PermissionDecisionItem],
) -> bool {
    request.items.iter().any(|item| {
        requires_objects(&item.tool_name)
            && decisions.iter().any(|decision| {
                decision.item_id == item.item_id
                    && matches!(
                        decision.decision,
                        crate::dynamic_run::PermissionItemDecision::Approve { .. }
                    )
            })
    })
}

pub struct PermissionGrantIssuanceContext<'a> {
    pub surface: ProductSurface,
    pub registry: &'a ProviderRegistry,
    pub inventory: &'a [CapabilityAvailability],
    pub readiness_revision: u64,
    pub now_unix_ms: u64,
    /// Current server-issued, readiness-bound object references selected by
    /// capability rather than by a persisted user attachment. These resolve a
    /// non-exact request's `selected:server_resolved` placeholder. A persisted
    /// original live-read target or exact semantic-action target stays bound to
    /// its own unexpired reference; the edge re-resolves and revalidates that
    /// reference immediately before observing or mutating the application.
    pub implicit_fresh_object_refs: &'a [ObjectRef],
}

/// Compile a complete, non-expanding owner decision against current server facts.
pub fn build_permission_grants(
    session: &PersistedAgentSession,
    request: &crate::dynamic_run::PermissionRequest,
    decisions: &[crate::dynamic_run::PermissionDecisionItem],
    context: &PermissionGrantIssuanceContext<'_>,
    original_reads: Option<&ReadContextSelection>,
) -> Result<Vec<CapabilityGrant>, AgentError> {
    crate::assistant_policy::require_current_policy(session.policy_revision)?;
    request
        .validate()
        .map_err(|error| internal(format!("invalid permission request: {error}")))?;
    if request.input_revision != session.input_revision {
        return Err(internal("permission request needs revalidation"));
    }
    request
        .clone()
        .apply_user_decision(decisions)
        .map_err(|error| internal(format!("invalid permission decision: {error}")))?;
    if context.readiness_revision == 0 || context.now_unix_ms == 0 {
        return Err(internal("invalid grant issuance readiness or clock"));
    }
    let decisions = decisions
        .iter()
        .map(|item| (item.item_id.as_str(), &item.decision))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut grants = Vec::new();
    for requested in &request.items {
        let Some(crate::dynamic_run::PermissionItemDecision::Approve {
            resource_scope,
            operation_scope,
            export_destinations,
            ttl_seconds,
            max_uses,
        }) = decisions.get(requested.item_id.as_str()).copied()
        else {
            continue;
        };
        let provider = context
            .registry
            .provider(&requested.provider_id)
            .ok_or_else(|| internal("approved permission Provider is no longer registered"))?;
        let capability = provider
            .capabilities
            .iter()
            .find(|capability| capability.tool_spec.name == requested.tool_name)
            .ok_or_else(|| internal("approved permission tool is no longer registered"))?;
        if capability.wire.effect != requested.expected_effect
            || !capability.wire.surfaces.contains(&context.surface)
        {
            return Err(internal(
                "approved permission no longer matches the compiled capability contract",
            ));
        }
        let availability = context
            .inventory
            .iter()
            .find(|item| {
                item.provider_id == requested.provider_id
                    && item.capability_id == capability.wire.capability_id
                    && item.tool_name == requested.tool_name
            })
            .ok_or_else(|| internal("approved permission has no current readiness fact"))?;
        if !availability.callable() {
            return Err(internal(
                "approved permission capability is no longer ready; refresh and decide again",
            ));
        }
        let original_read =
            if crate::input_read_context::live_read::target_kind(&requested.tool_name).is_some() {
                let original = original_reads
                    .ok_or_else(|| internal("original live read selection is missing"))?;
                original.validate()?;
                let message =
                    crate::permission_resume::latest_user_requirement(&session.conversation)
                        .ok_or_else(|| internal("original live input is missing"))?;
                let destination = message
                    .data_envelope
                    .as_ref()
                    .and_then(|envelope| envelope.allowed_destinations.first())
                    .ok_or_else(|| internal("original live destination is missing"))?;
                crate::input_read_context::live_read::validate_input(
                    original,
                    message,
                    destination,
                    context.now_unix_ms,
                )?;
                let target = crate::input_read_context::live_read::target(
                    original,
                    &requested.tool_name,
                    context.now_unix_ms,
                )?;
                Some((
                    vec![target.object_ref.clone()],
                    crate::input_read_context::live_read::expiry(original, target)?,
                ))
            } else if requires_objects(&requested.tool_name) {
                let original = original_reads
                    .ok_or_else(|| internal("original object read selection is missing"))?;
                original.validate()?;
                let destination = original
                    .object_attachments
                    .first()
                    .and_then(|object| object.envelope.allowed_destinations.first())
                    .ok_or_else(|| internal("original object destination is missing"))?;
                crate::input_read_context::validate_current_objects(
                    session,
                    &original.object_attachments,
                    destination,
                    context.now_unix_ms,
                )?;
                let binding = ObjectReadBinding {
                    original,
                    destination,
                    now_unix_ms: context.now_unix_ms,
                };
                let call = crate::chat::ToolCall {
                    id: requested.item_id.clone(),
                    name: requested.tool_name.clone(),
                    arguments_json: "{}".into(),
                };
                let expiry = binding.expiry(&call)?;
                let references = binding
                    .selected(&call)?
                    .into_iter()
                    .map(|object| {
                        serde_json::from_str::<ObjectRef>(&object.object_ref.opaque_token)
                            .map_err(|_| internal("invalid original object reference"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Some((references, expiry))
            } else {
                None
            };
        let sensitive_content = capability.wire.data_policy.reads.iter().any(|category| {
            !matches!(
                category,
                CapabilityDataCategory::UserRequest
                    | CapabilityDataCategory::DesktopSessionMetadata
                    | CapabilityDataCategory::FileMetadata
            )
        });
        let exact_external_query = capability.wire.authorization_hint.resources
            == [AuthorizationResourceKind::ExternalQuery];
        let exact_ui_action = matches!(
            capability.required_capability,
            desk_agent_protocol::Capability::DesktopUiActionConfirmed
                | desk_agent_protocol::Capability::DesktopInputFallbackConfirmed
        );
        let resource_scope = if exact_ui_action {
            #[derive(serde::Deserialize)]
            struct DesktopActionTarget {
                target: ObjectRef,
            }
            let canonical = requested
                .canonical_input_json
                .as_deref()
                .ok_or_else(|| internal("approved semantic UI action has no exact input"))?;
            let input: DesktopActionTarget = serde_json::from_str(canonical)
                .map_err(|_| internal("approved desktop action input is invalid"))?;
            let expected_kind = match capability.required_capability {
                desk_agent_protocol::Capability::DesktopUiActionConfirmed => ObjectKind::UiElement,
                desk_agent_protocol::Capability::DesktopInputFallbackConfirmed => {
                    ObjectKind::Application
                }
                _ => unreachable!(),
            };
            if input.target.object_kind != expected_kind {
                return Err(internal("approved desktop action target kind is invalid"));
            }
            if capability.required_capability
                == desk_agent_protocol::Capability::DesktopUiActionConfirmed
            {
                crate::provider_preflight::ui_action_from_call(&crate::chat::ToolCall {
                    id: requested.item_id.clone(),
                    name: capability.wire.tool_name.clone(),
                    arguments_json: canonical.into(),
                })?;
            }
            if capability.required_capability
                == desk_agent_protocol::Capability::DesktopInputFallbackConfirmed
            {
                #[derive(serde::Deserialize)]
                struct RawInputOnly {
                    action: desk_agent_protocol::computer_use::RawInputAction,
                }
                let action: RawInputOnly = serde_json::from_str(canonical)
                    .map_err(|_| internal("approved raw input action is invalid"))?;
                action.action.validate().map_err(|_| {
                    internal("approved raw input action is outside the bounded allowlist")
                })?;
            }
            crate::capability_grant::fresh_object_resource_scope(&[input.target])
                .into_iter()
                .filter(|scope| resource_scope.contains(scope))
                .collect()
        } else if capability.wire.authorization_hint.resources
            == [AuthorizationResourceKind::FreshObjectReference]
        {
            let exact_requested_scope = requested
                .canonical_input_json
                .as_ref()
                .filter(|_| {
                    !requested.resource_scope.is_empty()
                        && requested
                            .resource_scope
                            .iter()
                            .all(|scope| scope.starts_with("selected:sha256:"))
                })
                .map(|_| requested.resource_scope.clone());
            let object_refs = if exact_requested_scope.is_some() {
                Vec::new()
            } else if let Some((references, _)) = &original_read {
                references.clone()
            } else {
                session
                    .context_attachments
                    .iter()
                    .filter(|attachment| {
                        matches!(attachment.state, AttachmentState::Active)
                            && attachment_matches_fresh_object_capability(
                                capability.required_capability,
                                attachment,
                            )
                    })
                    .filter_map(|attachment| {
                        serde_json::from_str::<desk_agent_protocol::computer_use::ObjectRef>(
                            &attachment.object_ref.opaque_token,
                        )
                        .ok()
                    })
                    .chain(
                        context
                            .implicit_fresh_object_refs
                            .iter()
                            .filter(|object_ref| {
                                object_ref_matches_fresh_object_capability(
                                    capability.required_capability,
                                    object_ref,
                                )
                            })
                            .cloned(),
                    )
                    .collect::<Vec<_>>()
            };
            let exact = exact_requested_scope.unwrap_or_else(|| {
                crate::capability_grant::fresh_object_resource_scope(&object_refs)
            });
            if exact.is_empty() {
                return Err(internal(
                    "approved permission has no exact active selected object",
                ));
            }
            // The placeholder selects current server-held objects as a group.
            // Exact references may be narrowed individually. Neither form can
            // restore a resource the owner removed from their decision.
            if resource_scope
                .iter()
                .any(|scope| scope == "selected:server_resolved")
            {
                exact
            } else {
                exact
                    .into_iter()
                    .filter(|scope| resource_scope.contains(scope))
                    .collect()
            }
        } else if capability.wire.authorization_hint.resources
            == [AuthorizationResourceKind::ExternalUrl]
            || exact_external_query
        {
            // External URL authority is server-derived from the immutable exact
            // input stored on the pending request. Decision payload labels are
            // display data and cannot widen or replace it.
            requested
                .resource_scope
                .iter()
                .filter(|scope| resource_scope.contains(scope))
                .cloned()
                .collect()
        } else {
            resource_scope.clone()
        };
        let operation_scope = operation_scope.clone();
        let risk_tier = classify_capability_risk(
            capability.wire.effect,
            CapabilityRiskSignals {
                sensitive_content,
                external_egress: capability.wire.data_policy.may_export_data,
                destructive_or_overwrite: false,
                unpredictable_input: false,
            },
        );
        let (use_policy, canonical_input_digest_sha256) = if risk_tier
            == desk_agent_protocol::capability_grant::CapabilityRiskTier::R3
            || exact_ui_action
        {
            if *max_uses != 1 || requested.canonical_input_json.is_none() {
                return Err(internal(
                    "exact permission requires one use and an exact canonical input contract",
                ));
            }
            (
                CapabilityGrantUsePolicy::OneShotExact,
                requested.canonical_input_digest_sha256.clone(),
            )
        } else {
            (
                CapabilityGrantUsePolicy::Reusable,
                requested.canonical_input_digest_sha256.clone(),
            )
        };
        let expires_at_unix_ms = context
            .now_unix_ms
            .checked_add(u64::from(*ttl_seconds).saturating_mul(1_000))
            .ok_or_else(|| internal("grant expiry exceeds timestamp range"))?;
        let expires_at_unix_ms = original_read.map_or(expires_at_unix_ms, |(_, expiry)| {
            expires_at_unix_ms.min(expiry)
        });
        let grant_id = format!(
            "grant-{:x}",
            Sha256::digest(
                format!(
                    "{}:{}:{}:{}",
                    session.conversation_id,
                    request.request_id,
                    requested.item_id,
                    request.input_revision
                )
                .as_bytes()
            )
        );
        let export_destinations = if exact_external_query {
            vec![DestinationIdentity::WebResearch {
                connector_id: crate::device_assistant::DUCKDUCKGO_HTML_CONNECTOR_ID.into(),
            }]
            .into_iter()
            .filter(|destination| export_destinations.contains(destination))
            .collect()
        } else if exact_ui_action {
            requested
                .export_destinations
                .iter()
                .filter(|destination| export_destinations.contains(destination))
                .cloned()
                .collect()
        } else {
            export_destinations.clone()
        };
        grants.push(CapabilityGrant {
            schema_version: CAPABILITY_GRANT_SCHEMA_VERSION,
            grant_id,
            actor_id: session.actor_id.clone(),
            run_id: session.conversation_id.clone(),
            surface: context.surface,
            target_device_id: session.device_id.clone(),
            target_session_id: None,
            provider_id: requested.provider_id.clone(),
            capability_id: capability.wire.capability_id.clone(),
            tool_name: requested.tool_name.clone(),
            tool_schema_version: capability.wire.input_schema_version,
            effect: capability.wire.effect,
            risk_tier,
            resource_scope,
            operation_scope,
            export_destinations,
            allowed_envelope_ids: Vec::new(),
            allowed_content_digests_sha256: Vec::new(),
            use_policy,
            canonical_input_digest_sha256,
            issued_by: CapabilityGrantIssuer::UserDecision,
            issued_at_unix_ms: context.now_unix_ms,
            expires_at_unix_ms,
            remaining_uses: *max_uses,
            limits: CapabilityGrantLimits {
                max_bytes_per_call: capability.wire.limits.max_output_bytes,
                max_items_per_call: capability.wire.limits.max_objects,
                max_calls: *max_uses,
            },
            policy_revision: session.policy_revision,
            readiness_revision: context.readiness_revision,
            revoked_at_unix_ms: None,
            revoked_reason: None,
        });
    }
    for grant in &grants {
        grant
            .validate()
            .map_err(|error| internal(format!("invalid compiled grant: {error}")))?;
    }
    Ok(grants)
}

fn attachment_matches_fresh_object_capability(
    capability: desk_agent_protocol::Capability,
    attachment: &ContextAttachment,
) -> bool {
    match capability {
        desk_agent_protocol::Capability::FileArtifactCreateConfirmed
        | desk_agent_protocol::Capability::CommunicationLocalDraftCreateConfirmed
        | desk_agent_protocol::Capability::SpreadsheetWorkbookCreateConfirmed
        | desk_agent_protocol::Capability::SpreadsheetFormulaWorkbookCreateConfirmed
        | desk_agent_protocol::Capability::WordDocumentCreateConfirmed => {
            attachment.kind == ContextAttachmentKind::DirectorySelection
        }
        _ => false,
    }
}

fn object_ref_matches_fresh_object_capability(
    capability: desk_agent_protocol::Capability,
    object_ref: &ObjectRef,
) -> bool {
    (matches!(
        capability,
        desk_agent_protocol::Capability::BrowserPageObserve
            | desk_agent_protocol::Capability::BrowserPageNavigateConfirmed
            | desk_agent_protocol::Capability::BrowserInputFallbackConfirmed
            | desk_agent_protocol::Capability::BrowserExternalDraftWriteConfirmed
            | desk_agent_protocol::Capability::BrowserExternalSendConfirmed
    ) && object_ref.object_kind == ObjectKind::BrowserSurface)
        || (capability == desk_agent_protocol::Capability::CommunicationOutlookNewHandoffConfirmed
            && object_ref.object_kind == ObjectKind::Application)
        || (matches!(
            capability,
            desk_agent_protocol::Capability::SpreadsheetLiveInspect
                | desk_agent_protocol::Capability::SpreadsheetLivePatchConfirmed
        ) && object_ref.object_kind == ObjectKind::Range)
        || (matches!(
            capability,
            desk_agent_protocol::Capability::DocumentLiveInspect
                | desk_agent_protocol::Capability::DocumentLivePatchConfirmed
        ) && object_ref.object_kind == ObjectKind::Document)
        || (matches!(
            capability,
            desk_agent_protocol::Capability::PresentationLiveInspect
                | desk_agent_protocol::Capability::PresentationLivePatchConfirmed
        ) && object_ref.object_kind == ObjectKind::Slide)
}

fn internal(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message: message.into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    }
}

#[cfg(test)]
mod tests;
