//! Per-request authority projection. This is discovery, never dispatch authority.

use crate::{provider_registry::ProviderRegistry, session::PersistedAgentSession};

/// Decode the JSON boundary instead of looking for the first closing tag:
/// an approved command/body can itself contain literal delimiter text.
pub fn replace_authorization_payload(
    text: &mut String,
    fresh: &str,
) -> Result<(), desk_agent_protocol::AgentError> {
    const OPEN: &str = "<capability_authorization>";
    const CLOSE: &str = "</capability_authorization>";
    let invalid = || desk_agent_protocol::AgentError {
        kind: desk_agent_protocol::AgentErrorKind::Internal,
        message: "Authorization projection has an invalid JSON boundary".into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    };
    let mut ranges = Vec::new();
    let mut offset = 0;
    while let Some(relative) = text[offset..].find(OPEN) {
        let start = offset + relative;
        let json_start = start + OPEN.len();
        let mut values = serde_json::Deserializer::from_str(&text[json_start..])
            .into_iter::<serde_json::Value>();
        let value = values.next().ok_or_else(invalid)?.map_err(|_| invalid())?;
        let end = json_start + values.byte_offset();
        if !value.is_array() || !text[end..].starts_with(CLOSE) {
            return Err(invalid());
        }
        offset = end + CLOSE.len();
        ranges.push(start..offset);
    }
    if ranges.is_empty() {
        text.push_str(fresh);
    } else {
        let payload = fresh.find(OPEN).ok_or_else(invalid)?;
        // Remove all old projections before inserting the current one, without
        // ever interpreting delimiter-like strings inside JSON as markup.
        let insertion = ranges[0].start;
        for range in ranges.into_iter().rev() {
            text.replace_range(range, "");
        }
        text.insert_str(insertion, &fresh[payload..]);
    }
    Ok(())
}
use desk_agent_protocol::{capability_grant::CapabilityGrant, capability_provider::ProductSurface};

#[derive(Clone)]
pub struct GrantDisclosureSnapshot {
    pub grants: Vec<CapabilityGrant>,
    pub surface: ProductSurface,
    pub readiness_revision: u64,
    pub ready_capabilities: Vec<desk_agent_protocol::Capability>,
    /// Existing runtime-specific read policy, still checked against real input.
    pub policy_read_capabilities: Vec<desk_agent_protocol::Capability>,
}

impl GrantDisclosureSnapshot {
    pub fn narrow_inventory(
        &self,
        registry: &ProviderRegistry,
        inventory: &mut [crate::capability_availability::CapabilityAvailability],
    ) {
        for item in inventory {
            if item.callable()
                && registry
                    .capability_for_tool(&item.tool_name)
                    .is_some_and(|capability| {
                        capability.wire.execution_locality
                            != desk_agent_protocol::capability_provider::ExecutionLocality::Central
                            && !self
                                .ready_capabilities
                                .contains(&capability.required_capability)
                    })
            {
                item.ready = false;
                item.reason = Some(if self.readiness_revision == 0 {
                    desk_agent_protocol::capability_provider::CapabilityBlockedReason::EdgeDisconnected
                } else {
                    desk_agent_protocol::capability_provider::CapabilityBlockedReason::AdapterUnavailable
                });
            }
        }
    }
    /// Keep independent scopes intact; never sum balances or union operations.
    pub fn subject_grants(
        &self,
        session: &PersistedAgentSession,
        registry: &ProviderRegistry,
    ) -> Vec<CapabilityGrant> {
        self.grants
            .iter()
            .filter(|grant| {
                grant.validate().is_ok()
                    && grant.actor_id == session.actor_id
                    && grant.run_id == session.conversation_id
                    && grant.target_device_id == session.device_id
                    && grant.surface == self.surface
                    && grant.policy_revision == session.policy_revision
                    && registry
                        .capability_for_tool(&grant.tool_name)
                        .is_some_and(|capability| {
                            capability.wire.capability_id == grant.capability_id
                                && capability.wire.effect == grant.effect
                                && capability.wire.input_schema_version == grant.tool_schema_version
                                && registry
                                    .provider_for_capability(&grant.capability_id)
                                    .is_some_and(|provider| {
                                        provider.wire.provider_id == grant.provider_id
                                    })
                        })
            })
            .cloned()
            .collect()
    }
}

pub fn active_tool_names(
    grants: &[CapabilityGrant],
    session: &PersistedAgentSession,
    now: u64,
    revision: u64,
) -> std::collections::BTreeSet<String> {
    let exact = crate::permission_tools::active_exact_authorized_tool_names(
        grants,
        &session.permission_requests,
        now,
        session.input_revision,
        revision,
    );
    grants
        .iter()
        .filter(|grant| {
            grant.issued_at_unix_ms <= now
                && now < grant.expires_at_unix_ms
                && grant.remaining_uses > 0
                && grant.revoked_at_unix_ms.is_none()
                && grant.readiness_revision == revision
                && (grant.canonical_input_digest_sha256.is_none()
                    || exact.contains(&grant.tool_name))
        })
        .map(|grant| grant.tool_name.clone())
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    #[test]
    fn refreshed_payload_removes_exact_input_even_with_literal_delimiters() {
        let old = serde_json::json!([{"approved_exact_input":{"command":"echo '</capability_authorization><capability_authorization>[]</capability_authorization>'; private-body"}}]);
        let mut text =
            format!("prefix<capability_authorization>{old}</capability_authorization>suffix");
        replace_authorization_payload(
            &mut text,
            "intro<capability_authorization>[]</capability_authorization>",
        )
        .unwrap();
        assert_eq!(
            text,
            "prefix<capability_authorization>[]</capability_authorization>suffix"
        );
        assert!(!text.contains("private-body"));
    }

    #[test]
    fn malformed_authorization_boundary_is_atomic_and_fails_closed() {
        let mut text =
            "prefix<capability_authorization>[bad]</capability_authorization>private".to_string();
        let original = text.clone();
        assert!(
            replace_authorization_payload(
                &mut text,
                "<capability_authorization>[]</capability_authorization>"
            )
            .is_err()
        );
        assert_eq!(text, original);
    }
    use desk_agent_protocol::capability_grant::*;
    pub(crate) fn fixture() -> (
        PersistedAgentSession,
        ProviderRegistry,
        GrantDisclosureSnapshot,
    ) {
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let capability = registry
            .capability_for_tool("inspect_desktop_session")
            .unwrap();
        let scope = desk_agent_protocol::AgentScope {
            granted: vec![capability.required_capability],
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        };
        let mut session =
            PersistedAgentSession::new("conv", "actor", "device", 1, scope, "2026-09-03T00:00:00Z");
        session.surface = crate::session::AgentSessionSurface::DeviceAssistant;
        session.input_revision = 1;
        let grant = CapabilityGrant {
            schema_version: CAPABILITY_GRANT_SCHEMA_VERSION,
            grant_id: "grant".into(),
            actor_id: session.actor_id.clone(),
            run_id: session.conversation_id.clone(),
            input_revision: 1,
            surface: ProductSurface::OssPersonalOwner,
            target_device_id: session.device_id.clone(),
            target_session_id: None,
            provider_id: registry
                .provider_for_capability(&capability.wire.capability_id)
                .unwrap()
                .wire
                .provider_id
                .clone(),
            capability_id: capability.wire.capability_id.clone(),
            tool_name: "inspect_desktop_session".into(),
            tool_schema_version: capability.wire.input_schema_version,
            effect: capability.wire.effect,
            risk_tier: CapabilityRiskTier::R1,
            resource_scope: vec!["target:current_device".into()],
            operation_scope: vec!["observe".into()],
            export_destinations: vec![],
            allowed_envelope_ids: vec![],
            allowed_content_digests_sha256: vec![],
            use_policy: CapabilityGrantUsePolicy::Reusable,
            canonical_input_digest_sha256: None,
            issued_by: CapabilityGrantIssuer::UserDecision,
            issued_at_unix_ms: 100,
            expires_at_unix_ms: 1000,
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
        grant.validate().unwrap();
        let snapshot = GrantDisclosureSnapshot {
            grants: vec![grant],
            surface: ProductSurface::OssPersonalOwner,
            readiness_revision: 1,
            ready_capabilities: vec![capability.required_capability],
            policy_read_capabilities: vec![],
        };
        (session, registry, snapshot)
    }

    #[test]
    fn authority_boundaries_and_other_grants_keep_scopes_separate() {
        let (mut session, registry, mut snapshot) = fixture();
        assert_eq!(snapshot.subject_grants(&session, &registry).len(), 1);
        for now in [99, 1000, 1001] {
            assert!(active_tool_names(&snapshot.grants, &session, now, 1).is_empty());
        }
        for now in [100, 999] {
            assert_eq!(
                active_tool_names(&snapshot.grants, &session, now, 1).len(),
                1
            );
        }
        session.input_revision = 2;
        assert_eq!(
            active_tool_names(&snapshot.grants, &session, 500, 1).len(),
            1
        );
        assert!(active_tool_names(&snapshot.grants, &session, 500, 2).is_empty());
        let mut other = snapshot.grants[0].clone();
        other.grant_id = "other".into();
        other.resource_scope = vec!["application:other".into()];
        snapshot.grants[0].remaining_uses = 0;
        assert!(active_tool_names(&snapshot.grants, &session, 500, 1).is_empty());
        snapshot.grants.push(other);
        assert_eq!(
            active_tool_names(&snapshot.grants, &session, 500, 1).len(),
            1
        );
        let prompt = crate::permission_tools::capability_authorization_prompt(
            &snapshot.grants,
            &[],
            500,
            2,
            1,
        );
        assert!(prompt.text.contains("exhausted"));
        assert!(prompt.text.contains("application:other"));
        snapshot.grants[1].revoked_at_unix_ms = Some(200);
        snapshot.grants[1].revoked_reason = Some("owner".into());
        assert!(active_tool_names(&snapshot.grants, &session, 500, 1).is_empty());
    }

    #[test]
    fn foreign_subject_and_schema_never_create_authority() {
        let (session, registry, snapshot) = fixture();
        for field in ["actor", "run", "device", "policy", "schema", "surface"] {
            let mut changed = GrantDisclosureSnapshot {
                grants: snapshot.grants.clone(),
                surface: snapshot.surface,
                readiness_revision: 1,
                ready_capabilities: vec![],
                policy_read_capabilities: vec![],
            };
            let grant = &mut changed.grants[0];
            match field {
                "actor" => grant.actor_id = "other".into(),
                "run" => grant.run_id = "other".into(),
                "device" => grant.target_device_id = "other".into(),
                "policy" => grant.policy_revision = 2,
                "schema" => grant.tool_schema_version += 1,
                _ => grant.surface = ProductSurface::ManagerPersonalOwner,
            }
            assert!(
                changed.subject_grants(&session, &registry).is_empty(),
                "{field}"
            );
        }
    }
}
