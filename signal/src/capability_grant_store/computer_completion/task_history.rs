//! Verify original scheduled-run dispatches during recovery without consuming or
//! renewing authority.
use super::*;
use desk_agent_protocol::{
    capability_grant::CapabilityGrantIssuer, capability_provider::ProductSurface,
};

impl SignalCapabilityGrantStore {
    /// Historical receipt restoration accepts failure evidence without projecting
    /// it into successful-step or message-send evidence.
    pub(crate) async fn original_task_terminal_on(
        txn: &DatabaseTransaction,
        generation: &str,
        parent: &desk_agent_protocol::capability_grant::TaskGrantProvenance,
    ) -> Result<Option<OriginalResult>, DbErr> {
        let computer_original = original_on(txn, generation).await?;
        let (outbox, work, payload) = computer_original.as_ref();
        if work.manual_resolved_at.is_some() {
            return Err(invalid());
        }
        let Some(original) = terminal_result(outbox, work.clone(), payload)? else {
            return Ok(None);
        };
        Self::validate_original_authority_on(
            txn,
            work,
            outbox,
            payload,
            Some(parent),
            Some(original.receipt.received_at_unix_ms),
        )
        .await?;
        Ok(Some(original))
    }

    /// Verify dispatch provenance without manufacturing any completion timestamp.
    pub(crate) async fn validate_task_dispatch_history_on(
        txn: &DatabaseTransaction,
        generation: &str,
        parent: &desk_agent_protocol::capability_grant::TaskGrantProvenance,
    ) -> Result<(), DbErr> {
        let computer_original = original_on(txn, generation).await?;
        let (outbox, work, payload) = computer_original.as_ref();
        Self::validate_original_authority_on(txn, work, outbox, payload, Some(parent), None)
            .await?;
        Ok(())
    }

    async fn validate_original_authority_on(
        txn: &DatabaseTransaction,
        work: &agent_action_item::Model,
        outbox: &agent_capability_dispatch_outbox::Model,
        payload: &CapabilityDispatchPayload,
        parent: Option<&desk_agent_protocol::capability_grant::TaskGrantProvenance>,
        received_at_unix_ms: Option<u64>,
    ) -> Result<CapabilityGrant, DbErr> {
        let prepared = decode_prepared_payload(work)?;
        let row = agent_capability_grant::Entity::find()
            .filter(agent_capability_grant::Column::GrantId.eq(&payload.grant_id))
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        decode_grant(&row)?;
        let issued: CapabilityGrant =
            serde_json::from_str(&row.issued_payload_json).map_err(json_error)?;
        let at = work.dispatch_intent_at.ok_or_else(invalid)?;
        let at_ms = u64::try_from(at.timestamp_millis()).map_err(|_| invalid())?;
        let authority = &payload.observed_authority;
        if prepared.observed_authority != *authority
            || authority.canonical_input_sha256 != payload.canonical_input_digest_sha256
            || authority.provider_id != payload.provider_id
            || authority.capability_id != payload.capability_id
            || authority.tool_name != payload.tool_name
            || !authority.is_within_grant(&issued)
            || issued.actor_id != work.actor_id
            || issued.run_id != work.conversation_id
            || issued.target_device_id != work.target_device_id
            || issued.surface != ProductSurface::OssPersonalOwner
            || issued.policy_revision != work.policy_revision
            || match (&issued.issued_by, parent) {
                (CapabilityGrantIssuer::ScheduledRun(issued_parent), Some(current)) => {
                    issued_parent != current
                }
                (CapabilityGrantIssuer::ScheduledRun(_), None) | (_, Some(_)) => true,
                (_, None) => false,
            }
            || at_ms < issued.issued_at_unix_ms
            || at_ms >= issued.expires_at_unix_ms
            || issued
                .revoked_at_unix_ms
                .is_some_and(|revoked| revoked <= at_ms)
            || at < work.created_at
            || at > outbox.created_at
            || received_at_unix_ms.is_some_and(|received| received < at_ms)
            || received_at_unix_ms.is_some_and(|received| {
                u64::try_from(work.updated_at.timestamp_millis())
                    .ok()
                    .is_none_or(|updated| received > updated)
            })
        {
            return Err(invalid());
        }
        Ok(issued)
    }
}
