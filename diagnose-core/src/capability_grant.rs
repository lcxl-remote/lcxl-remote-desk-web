//! Pure Stage-3 CapabilityGrant matcher.
//!
//! Matching has no reserve or dispatch capability. The SQLite/Manager stores use
//! this decision inside their own transaction before consuming a grant use.

use desk_agent_protocol::{
    capability_grant::{CapabilityGrant, CapabilityGrantUsePolicy, CapabilityRiskTier},
    capability_provider::{AuthorizationResourceKind, CapabilityEffect, ProductSurface},
    data_lineage::DestinationIdentity,
};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalGrantScope {
    pub resources: Vec<String>,
    pub operations: Vec<String>,
}

/// Resolve scopes that are completely determined by the compiled descriptor.
/// Model-supplied prose is never authority. Resource kinds that require a
/// selected object/root/account must be resolved by their server-side adapter
/// and intentionally return `None` here.
pub fn canonical_compiled_scope(
    authorization_resources: &[AuthorizationResourceKind],
    effect: CapabilityEffect,
) -> Option<CanonicalGrantScope> {
    if authorization_resources == [AuthorizationResourceKind::TargetDevice]
        && matches!(
            effect,
            CapabilityEffect::ReadDevice | CapabilityEffect::CaptureScreen
        )
    {
        return Some(CanonicalGrantScope {
            resources: vec!["target:current_device".into()],
            operations: vec!["observe".into()],
        });
    }
    if authorization_resources == [AuthorizationResourceKind::FreshObjectReference] {
        return Some(CanonicalGrantScope {
            // This placeholder is safe for display/planning only. Grant issuance
            // and dispatch replace it with hashes of the exact server-held refs.
            resources: vec!["selected:server_resolved".into()],
            operations: vec![
                match effect {
                    CapabilityEffect::WriteArtifact => "create_new_artifact",
                    CapabilityEffect::ReadFile => "inspect_selected_object",
                    _ => "use_selected_object",
                }
                .into(),
            ],
        });
    }
    if authorization_resources == [AuthorizationResourceKind::ExternalUrl]
        && effect == CapabilityEffect::ReadExternal
    {
        return Some(CanonicalGrantScope {
            // The placeholder is replaced with a digest of the exact
            // server-canonicalized input before a permission request is stored,
            // a grant is issued, or dispatch is prepared.
            resources: vec!["external_url:exact_input_required".into()],
            operations: vec!["fetch_public_https".into()],
        });
    }
    if authorization_resources == [AuthorizationResourceKind::ExternalQuery]
        && effect == CapabilityEffect::ExportData
    {
        return Some(CanonicalGrantScope {
            resources: vec!["external_query:exact_input_required".into()],
            operations: vec!["search_public_web".into()],
        });
    }
    if authorization_resources == [AuthorizationResourceKind::ExactCommand]
        && effect == CapabilityEffect::ExecuteCommand
    {
        return Some(CanonicalGrantScope {
            resources: vec!["command:exact_input_required".into()],
            operations: vec!["execute_confirmed_command".into()],
        });
    }
    None
}

/// Stable authority label for an exact externally fetched URL. The canonical
/// input digest binds all tool arguments, while keeping the raw URL out of grant
/// rows and audit projections.
pub fn exact_external_url_resource_scope(canonical_input_digest_sha256: &str) -> Vec<String> {
    vec![format!(
        "external_url_input:sha256:{canonical_input_digest_sha256}"
    )]
}

/// Stable authority label for an exact query sent to a server-owned Web
/// Search connector. The raw query remains in the bounded pending request and
/// never becomes a grant/audit resource label.
pub fn exact_external_query_resource_scope(canonical_input_digest_sha256: &str) -> Vec<String> {
    vec![format!(
        "external_query_input:sha256:{canonical_input_digest_sha256}"
    )]
}

/// Stable authority label for one server-classified command. The raw command
/// and argv remain in the sealed execution record, not in grant/audit labels.
pub fn exact_command_resource_scope(canonical_input_digest_sha256: &str) -> Vec<String> {
    vec![format!(
        "command_input:sha256:{canonical_input_digest_sha256}"
    )]
}

/// Stable, non-reversible authority labels for edge-issued object references.
/// Raw tokens never enter a grant, permission UI, model prompt, or audit row.
pub fn fresh_object_resource_scope(
    object_refs: &[desk_agent_protocol::computer_use::ObjectRef],
) -> Vec<String> {
    let mut scope = object_refs
        .iter()
        .filter_map(|object_ref| serde_json::to_vec(object_ref).ok())
        .map(|encoded| format!("selected:sha256:{:x}", Sha256::digest(encoded)))
        .collect::<Vec<_>>();
    scope.sort();
    scope.dedup();
    scope
}

#[derive(Debug, Clone)]
pub struct CapabilityGrantCall<'a> {
    pub actor_id: &'a str,
    pub run_id: &'a str,
    pub surface: ProductSurface,
    pub target_device_id: &'a str,
    pub target_session_id: Option<&'a str>,
    pub provider_id: &'a str,
    pub capability_id: &'a str,
    pub tool_name: &'a str,
    pub tool_schema_version: u16,
    pub effect: CapabilityEffect,
    pub risk_tier: CapabilityRiskTier,
    pub resource_scope: &'a [String],
    pub operation_scope: &'a [String],
    pub export_destinations: &'a [DestinationIdentity],
    pub envelope_ids: &'a [String],
    pub content_digests_sha256: &'a [String],
    pub canonical_input_digest_sha256: &'a str,
    pub byte_count: u64,
    pub item_count: u32,
    pub policy_revision: i64,
    pub readiness_revision: u64,
    pub now_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantMismatch {
    InvalidGrant,
    Revoked,
    NotYetValid,
    Expired,
    Exhausted,
    Subject,
    Surface,
    Target,
    ProviderTool,
    Schema,
    EffectOrRisk,
    ResourceScope,
    OperationScope,
    Destination,
    DataEnvelope,
    CanonicalInput,
    Limits,
    PolicyRevision,
    ReadinessRevision,
}

pub fn match_capability_grant(
    grant: &CapabilityGrant,
    call: &CapabilityGrantCall<'_>,
) -> Result<(), GrantMismatch> {
    match_capability_grant_inner(grant, call, true)
}

/// Revalidate a call that already owns a durable reservation. The reservation
/// consumed the available use during Prepare, so zero unreserved uses is not an
/// error here; every other current policy/readiness/scope check is identical.
pub fn match_reserved_capability_grant(
    grant: &CapabilityGrant,
    call: &CapabilityGrantCall<'_>,
) -> Result<(), GrantMismatch> {
    match_capability_grant_inner(grant, call, false)
}

fn match_capability_grant_inner(
    grant: &CapabilityGrant,
    call: &CapabilityGrantCall<'_>,
    require_available_use: bool,
) -> Result<(), GrantMismatch> {
    grant.validate().map_err(|_| GrantMismatch::InvalidGrant)?;
    if grant.revoked_at_unix_ms.is_some() {
        return Err(GrantMismatch::Revoked);
    }
    if call.now_unix_ms < grant.issued_at_unix_ms {
        return Err(GrantMismatch::NotYetValid);
    }
    if call.now_unix_ms >= grant.expires_at_unix_ms {
        return Err(GrantMismatch::Expired);
    }
    if require_available_use && grant.remaining_uses == 0 {
        return Err(GrantMismatch::Exhausted);
    }
    if grant.actor_id != call.actor_id || grant.run_id != call.run_id {
        return Err(GrantMismatch::Subject);
    }
    if grant.surface != call.surface {
        return Err(GrantMismatch::Surface);
    }
    if grant.target_device_id != call.target_device_id
        || grant.target_session_id.as_deref() != call.target_session_id
    {
        return Err(GrantMismatch::Target);
    }
    if grant.provider_id != call.provider_id
        || grant.capability_id != call.capability_id
        || grant.tool_name != call.tool_name
    {
        return Err(GrantMismatch::ProviderTool);
    }
    if grant.tool_schema_version != call.tool_schema_version {
        return Err(GrantMismatch::Schema);
    }
    if grant.effect != call.effect || grant.risk_tier != call.risk_tier {
        return Err(GrantMismatch::EffectOrRisk);
    }
    if !is_subset(call.resource_scope, &grant.resource_scope) {
        return Err(GrantMismatch::ResourceScope);
    }
    if !is_subset(call.operation_scope, &grant.operation_scope) {
        return Err(GrantMismatch::OperationScope);
    }
    if !is_subset(call.export_destinations, &grant.export_destinations) {
        return Err(GrantMismatch::Destination);
    }
    if !is_subset(call.envelope_ids, &grant.allowed_envelope_ids)
        || !is_subset(
            call.content_digests_sha256,
            &grant.allowed_content_digests_sha256,
        )
    {
        return Err(GrantMismatch::DataEnvelope);
    }
    if grant
        .canonical_input_digest_sha256
        .as_deref()
        .is_some_and(|digest| digest != call.canonical_input_digest_sha256)
        || matches!(grant.use_policy, CapabilityGrantUsePolicy::OneShotExact)
            && grant.canonical_input_digest_sha256.as_deref()
                != Some(call.canonical_input_digest_sha256)
    {
        return Err(GrantMismatch::CanonicalInput);
    }
    if call.byte_count > grant.limits.max_bytes_per_call
        || call.item_count > grant.limits.max_items_per_call
    {
        return Err(GrantMismatch::Limits);
    }
    if grant.policy_revision != call.policy_revision {
        return Err(GrantMismatch::PolicyRevision);
    }
    if grant.readiness_revision != call.readiness_revision {
        return Err(GrantMismatch::ReadinessRevision);
    }
    Ok(())
}

fn is_subset<T: PartialEq>(values: &[T], allowed: &[T]) -> bool {
    values.iter().all(|value| allowed.contains(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::capability_grant::{
        CAPABILITY_GRANT_SCHEMA_VERSION, CapabilityGrantIssuer, CapabilityGrantLimits,
    };

    fn digest(byte: char) -> String {
        std::iter::repeat_n(byte, 64).collect()
    }

    fn grant() -> CapabilityGrant {
        CapabilityGrant {
            schema_version: CAPABILITY_GRANT_SCHEMA_VERSION,
            grant_id: "grant-1".into(),
            actor_id: "actor-1".into(),
            run_id: "run-1".into(),
            surface: ProductSurface::OssPersonalOwner,
            target_device_id: "device-1".into(),
            target_session_id: Some("session-1".into()),
            provider_id: "file.workspace".into(),
            capability_id: "file.artifact.create".into(),
            tool_name: "create_artifact".into(),
            tool_schema_version: 1,
            effect: CapabilityEffect::WriteArtifact,
            risk_tier: CapabilityRiskTier::R3,
            resource_scope: vec!["root:selected".into()],
            operation_scope: vec!["create_new".into()],
            export_destinations: Vec::new(),
            allowed_envelope_ids: vec!["envelope-1".into()],
            allowed_content_digests_sha256: vec![digest('b')],
            use_policy: CapabilityGrantUsePolicy::OneShotExact,
            canonical_input_digest_sha256: Some(digest('a')),
            issued_by: CapabilityGrantIssuer::UserDecision,
            issued_at_unix_ms: 100,
            expires_at_unix_ms: 200,
            remaining_uses: 1,
            limits: CapabilityGrantLimits {
                max_bytes_per_call: 1024,
                max_items_per_call: 1,
                max_calls: 1,
            },
            policy_revision: 7,
            readiness_revision: 9,
            revoked_at_unix_ms: None,
            revoked_reason: None,
        }
    }

    #[test]
    fn compiled_target_device_reads_have_one_server_owned_scope() {
        let scope = canonical_compiled_scope(
            &[AuthorizationResourceKind::TargetDevice],
            CapabilityEffect::ReadDevice,
        )
        .unwrap();
        assert_eq!(scope.resources, vec!["target:current_device"]);
        assert_eq!(scope.operations, vec!["observe"]);
        let selected = canonical_compiled_scope(
            &[AuthorizationResourceKind::FreshObjectReference],
            CapabilityEffect::ReadFile,
        )
        .unwrap();
        assert_eq!(selected.resources, vec!["selected:server_resolved"]);
        assert_eq!(selected.operations, vec!["inspect_selected_object"]);
        assert!(
            canonical_compiled_scope(
                &[AuthorizationResourceKind::TargetDevice],
                CapabilityEffect::MutateApplication,
            )
            .is_none()
        );
        let command = canonical_compiled_scope(
            &[AuthorizationResourceKind::ExactCommand],
            CapabilityEffect::ExecuteCommand,
        )
        .unwrap();
        assert_eq!(command.resources, vec!["command:exact_input_required"]);
        assert_eq!(command.operations, vec!["execute_confirmed_command"]);
        assert_eq!(
            exact_command_resource_scope(&digest('a')),
            vec![format!("command_input:sha256:{}", digest('a'))]
        );
    }

    fn call<'a>(
        resources: &'a [String],
        operations: &'a [String],
        envelopes: &'a [String],
        digests: &'a [String],
        canonical: &'a str,
    ) -> CapabilityGrantCall<'a> {
        CapabilityGrantCall {
            actor_id: "actor-1",
            run_id: "run-1",
            surface: ProductSurface::OssPersonalOwner,
            target_device_id: "device-1",
            target_session_id: Some("session-1"),
            provider_id: "file.workspace",
            capability_id: "file.artifact.create",
            tool_name: "create_artifact",
            tool_schema_version: 1,
            effect: CapabilityEffect::WriteArtifact,
            risk_tier: CapabilityRiskTier::R3,
            resource_scope: resources,
            operation_scope: operations,
            export_destinations: &[],
            envelope_ids: envelopes,
            content_digests_sha256: digests,
            canonical_input_digest_sha256: canonical,
            byte_count: 512,
            item_count: 1,
            policy_revision: 7,
            readiness_revision: 9,
            now_unix_ms: 150,
        }
    }

    #[test]
    fn exact_grant_matches_only_the_frozen_subject_scope_data_and_input() {
        let resources = vec!["root:selected".into()];
        let operations = vec!["create_new".into()];
        let envelopes = vec!["envelope-1".into()];
        let digests = vec![digest('b')];
        assert_eq!(
            match_capability_grant(
                &grant(),
                &call(&resources, &operations, &envelopes, &digests, &digest('a')),
            ),
            Ok(())
        );
    }

    #[test]
    fn exact_grant_rejects_one_byte_input_change_and_scope_widening() {
        let resources = vec!["root:selected".into()];
        let operations = vec!["create_new".into()];
        let envelopes = vec!["envelope-1".into()];
        let digests = vec![digest('b')];
        assert_eq!(
            match_capability_grant(
                &grant(),
                &call(&resources, &operations, &envelopes, &digests, &digest('c')),
            ),
            Err(GrantMismatch::CanonicalInput)
        );
        let widened = vec!["root:other".into()];
        assert_eq!(
            match_capability_grant(
                &grant(),
                &call(&widened, &operations, &envelopes, &digests, &digest('a')),
            ),
            Err(GrantMismatch::ResourceScope)
        );
    }

    #[test]
    fn current_policy_readiness_and_revocation_fail_closed() {
        let resources = vec!["root:selected".into()];
        let operations = vec!["create_new".into()];
        let envelopes = vec!["envelope-1".into()];
        let digests = vec![digest('b')];
        let canonical = digest('a');
        let mut current = call(&resources, &operations, &envelopes, &digests, &canonical);
        current.policy_revision = 8;
        assert_eq!(
            match_capability_grant(&grant(), &current),
            Err(GrantMismatch::PolicyRevision)
        );
        current.policy_revision = 7;
        current.readiness_revision = 10;
        assert_eq!(
            match_capability_grant(&grant(), &current),
            Err(GrantMismatch::ReadinessRevision)
        );
        current.readiness_revision = 9;
        let mut revoked = grant();
        revoked.revoked_at_unix_ms = Some(160);
        revoked.revoked_reason = Some("owner revoked".into());
        assert_eq!(
            match_capability_grant(&revoked, &current),
            Err(GrantMismatch::Revoked)
        );
    }
}
