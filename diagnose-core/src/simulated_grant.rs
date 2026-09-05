//! Stage-2-only grant simulation.
//!
//! This module exercises permission-decision matching and bounded use counts
//! without creating a production authority token, work item, reservation, or
//! dispatch intent. Stage 3 replaces this with the durable authorizer.

use desk_agent_protocol::{
    capability_provider::CapabilityEffect, data_lineage::DestinationIdentity,
};

use crate::dynamic_run::{
    DynamicRunContractError, PermissionDecisionItem, PermissionItemDecision, PermissionRequest,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulatedCapabilityGrant {
    pub simulation_id: String,
    pub permission_request_id: String,
    pub permission_item_id: String,
    pub provider_id: String,
    pub tool_name: String,
    pub effect: CapabilityEffect,
    pub resource_scope: Vec<String>,
    pub operation_scope: Vec<String>,
    pub export_destinations: Vec<DestinationIdentity>,
    pub ttl_seconds: u32,
    pub remaining_uses: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulatedCapabilityCall<'a> {
    pub provider_id: &'a str,
    pub tool_name: &'a str,
    pub effect: CapabilityEffect,
    pub resource_scope: &'a [String],
    pub operation_scope: &'a [String],
    pub export_destinations: &'a [DestinationIdentity],
}

#[derive(Debug, Clone, Default)]
pub struct SimulatedGrantAuthorizer {
    grants: Vec<SimulatedCapabilityGrant>,
}

impl SimulatedGrantAuthorizer {
    pub fn from_decision(
        request: &PermissionRequest,
        decisions: &[PermissionDecisionItem],
    ) -> Result<Self, DynamicRunContractError> {
        let mut validated = request.clone();
        validated.apply_user_decision(decisions)?;
        let mut grants = Vec::new();
        for requested in &request.items {
            let Some(decision) = decisions
                .iter()
                .find(|decision| decision.item_id == requested.item_id)
            else {
                return Err(DynamicRunContractError::PermissionDecisionIncomplete);
            };
            let PermissionItemDecision::Approve {
                resource_scope,
                operation_scope,
                export_destinations,
                ttl_seconds,
                max_uses,
            } = &decision.decision
            else {
                continue;
            };
            grants.push(SimulatedCapabilityGrant {
                simulation_id: format!("simulated:{}:{}", request.request_id, requested.item_id),
                permission_request_id: request.request_id.clone(),
                permission_item_id: requested.item_id.clone(),
                provider_id: requested.provider_id.clone(),
                tool_name: requested.tool_name.clone(),
                effect: requested.expected_effect,
                resource_scope: resource_scope.clone(),
                operation_scope: operation_scope.clone(),
                export_destinations: export_destinations.clone(),
                ttl_seconds: *ttl_seconds,
                remaining_uses: *max_uses,
            });
        }
        Ok(Self { grants })
    }

    pub fn grants(&self) -> &[SimulatedCapabilityGrant] {
        &self.grants
    }

    /// Match and consume one simulated use. Returning `true` is only a Stage-2
    /// assertion: this type has no dispatch method and cannot authorize runtime I/O.
    pub fn match_and_consume(&mut self, call: &SimulatedCapabilityCall<'_>) -> bool {
        let Some(grant) = self.grants.iter_mut().find(|grant| {
            grant.remaining_uses > 0
                && grant.provider_id == call.provider_id
                && grant.tool_name == call.tool_name
                && grant.effect == call.effect
                && is_subset(call.resource_scope, &grant.resource_scope)
                && is_subset(call.operation_scope, &grant.operation_scope)
                && call
                    .export_destinations
                    .iter()
                    .all(|destination| grant.export_destinations.contains(destination))
        }) else {
            return false;
        };
        grant.remaining_uses -= 1;
        true
    }
}

fn is_subset<T: PartialEq>(values: &[T], allowed: &[T]) -> bool {
    values.iter().all(|value| allowed.contains(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dynamic_run::{
        GrantRequestItem, PERMISSION_REQUEST_SCHEMA_VERSION, PermissionRequestState,
    };

    fn request() -> PermissionRequest {
        PermissionRequest {
            schema_version: PERMISSION_REQUEST_SCHEMA_VERSION,
            request_id: "permission-1".into(),
            input_revision: 1,
            state: PermissionRequestState::Pending,
            items: vec![
                GrantRequestItem {
                    command_confirmation: None,
                    item_id: "session".into(),
                    provider_id: "desktop.session".into(),
                    tool_name: "inspect_desktop_session".into(),
                    expected_effect: CapabilityEffect::ReadDevice,
                    resource_scope: vec!["target:device".into(), "target:session".into()],
                    operation_scope: vec!["observe".into()],
                    export_destinations: Vec::new(),
                    canonical_input_json: None,
                    canonical_input_digest_sha256: None,
                    suggested_ttl_seconds: 120,
                    suggested_max_uses: 1,
                    reason: "inspect the selected session".into(),
                },
                GrantRequestItem {
                    command_confirmation: None,
                    item_id: "ui".into(),
                    provider_id: "desktop.ui".into(),
                    tool_name: "inspect_desktop_ui".into(),
                    expected_effect: CapabilityEffect::ReadDevice,
                    resource_scope: vec!["target:device".into()],
                    operation_scope: vec!["observe_ui".into()],
                    export_destinations: Vec::new(),
                    canonical_input_json: None,
                    canonical_input_digest_sha256: None,
                    suggested_ttl_seconds: 120,
                    suggested_max_uses: 1,
                    reason: "inspect the selected UI".into(),
                },
            ],
            created_at: "2026-08-26T00:00:00Z".into(),
        }
    }

    #[test]
    fn simulated_authorizer_only_consumes_the_approved_narrowed_item() {
        let decisions = vec![
            PermissionDecisionItem {
                item_id: "session".into(),
                decision: PermissionItemDecision::Approve {
                    resource_scope: vec!["target:session".into()],
                    operation_scope: vec!["observe".into()],
                    export_destinations: Vec::new(),
                    ttl_seconds: 60,
                    max_uses: 1,
                },
            },
            PermissionDecisionItem {
                item_id: "ui".into(),
                decision: PermissionItemDecision::Deny,
            },
        ];
        let mut authorizer = SimulatedGrantAuthorizer::from_decision(&request(), &decisions)
            .expect("a narrowed complete decision should simulate one grant");
        assert_eq!(authorizer.grants().len(), 1);
        assert_eq!(authorizer.grants()[0].ttl_seconds, 60);
        assert_eq!(authorizer.grants()[0].remaining_uses, 1);

        let resource_scope = vec!["target:session".into()];
        let operation_scope = vec!["observe".into()];
        let call = SimulatedCapabilityCall {
            provider_id: "desktop.session",
            tool_name: "inspect_desktop_session",
            effect: CapabilityEffect::ReadDevice,
            resource_scope: &resource_scope,
            operation_scope: &operation_scope,
            export_destinations: &[],
        };
        assert!(authorizer.match_and_consume(&call));
        assert!(!authorizer.match_and_consume(&call), "one use is exhausted");

        let denied_call = SimulatedCapabilityCall {
            provider_id: "desktop.ui",
            tool_name: "inspect_desktop_ui",
            effect: CapabilityEffect::ReadDevice,
            resource_scope: &resource_scope,
            operation_scope: &operation_scope,
            export_destinations: &[],
        };
        assert!(!authorizer.match_and_consume(&denied_call));
    }

    #[test]
    fn simulated_authorizer_rejects_a_widened_decision_before_signing() {
        let decisions = vec![
            PermissionDecisionItem {
                item_id: "session".into(),
                decision: PermissionItemDecision::Approve {
                    resource_scope: vec!["target:other".into()],
                    operation_scope: vec!["observe".into()],
                    export_destinations: Vec::new(),
                    ttl_seconds: 60,
                    max_uses: 1,
                },
            },
            PermissionDecisionItem {
                item_id: "ui".into(),
                decision: PermissionItemDecision::Deny,
            },
        ];
        assert_eq!(
            SimulatedGrantAuthorizer::from_decision(&request(), &decisions).unwrap_err(),
            DynamicRunContractError::PermissionDecisionWidensScope
        );
    }
}
