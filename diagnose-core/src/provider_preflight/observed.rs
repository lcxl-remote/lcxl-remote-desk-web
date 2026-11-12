/// Scope resolved by the trusted provider preflight for this specific call.
/// This is evidence metadata, never an execution grant or a reusable template.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedCapabilityAuthority {
    pub target_session_id: Option<String>,
    pub envelope_ids: Vec<String>,
    pub provider_id: String,
    pub capability_id: String,
    pub tool_name: String,
    pub tool_schema_version: u16,
    pub effect: desk_agent_protocol::capability_provider::CapabilityEffect,
    pub risk_tier: desk_agent_protocol::capability_grant::CapabilityRiskTier,
    pub canonical_input_sha256: String,
    pub resources: Vec<String>,
    pub operations: Vec<String>,
    pub export_destinations: Vec<desk_agent_protocol::data_lineage::DestinationIdentity>,
}

impl ObservedCapabilityAuthority {
    pub fn from_call(call: &crate::capability_grant::CapabilityGrantCall<'_>) -> Self {
        Self {
            target_session_id: call.target_session_id.map(str::to_owned),
            envelope_ids: call.envelope_ids.to_vec(),
            provider_id: call.provider_id.into(),
            capability_id: call.capability_id.into(),
            tool_name: call.tool_name.into(),
            tool_schema_version: call.tool_schema_version,
            effect: call.effect,
            risk_tier: call.risk_tier,
            canonical_input_sha256: call.canonical_input_digest_sha256.into(),
            resources: call.resource_scope.to_vec(),
            operations: call.operation_scope.to_vec(),
            export_destinations: call.export_destinations.to_vec(),
        }
    }
}

impl ObservedCapabilityAuthority {
    /// Compare historical scope to its issued grant; does not authorize execution.
    pub fn is_within_grant(
        &self,
        grant: &desk_agent_protocol::capability_grant::CapabilityGrant,
    ) -> bool {
        self.target_session_id == grant.target_session_id
            && self.provider_id == grant.provider_id
            && self.capability_id == grant.capability_id
            && self.tool_name == grant.tool_name
            && self.tool_schema_version == grant.tool_schema_version
            && self.effect == grant.effect
            && self.risk_tier == grant.risk_tier
            && self
                .resources
                .iter()
                .all(|value| grant.resource_scope.contains(value))
            && self
                .operations
                .iter()
                .all(|value| grant.operation_scope.contains(value))
            && self
                .export_destinations
                .iter()
                .all(|value| grant.export_destinations.contains(value))
            && self
                .envelope_ids
                .iter()
                .all(|value| grant.allowed_envelope_ids.contains(value))
            && grant
                .canonical_input_digest_sha256
                .as_ref()
                .is_none_or(|value| value == &self.canonical_input_sha256)
    }
}
