//! Read successful original dispatch evidence without consuming or renewing authority.
use super::*;
use desk_agent_protocol::{
    capability_grant::CapabilityGrantIssuer,
    capability_provider::{CapabilityEffect, ProductSurface},
    computer_use::ComputerActionResultClass,
};
use desk_diagnose_core::{
    action_result::ActionResultOrigin,
    communication_handoff::{
        SentWebMessageContext, SentWebMessageEvidence, project_sent_web_message,
    },
    provider_preflight::ObservedCapabilityAuthority,
};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ObservedProviderAction {
    pub work_id: i64,
    pub grant_id: String,
    pub browser_page: Option<desk_agent_protocol::browser_control::BrowserPageRef>,
    pub origin: ActionResultOrigin,
    pub authority: ObservedCapabilityAuthority,
    pub issued_by: CapabilityGrantIssuer,
    pub completed_at: u64,
    pub completion_sha256: String,
    pub output_sha256: String,
    pub receipt: ActionResultReceipt,
    pub canonical_input_json: String,
    pub created_artifact: Option<desk_agent_protocol::computer_use::CreatedFileArtifactOutput>,
    pub prepared_message:
        Option<desk_diagnose_core::communication_handoff::PreparedWebMessageEvidence>,
    pub sent_message: Option<SentWebMessageEvidence>,
}

impl SignalCapabilityGrantStore {
    /// The caller must bind this history to the owner and frozen rehearsal session.
    /// A later revocation remains visible but cannot erase an already observed effect.
    pub(crate) async fn observe_completed_provider_on(
        txn: &DatabaseTransaction,
        generation: &str,
    ) -> Result<Option<ObservedProviderAction>, DbErr> {
        Self::observe_provider_with_parent_on(txn, generation, None).await
    }

    pub(crate) async fn observe_completed_task_provider_on(
        txn: &DatabaseTransaction,
        generation: &str,
        parent: &desk_agent_protocol::capability_grant::TaskGrantProvenance,
    ) -> Result<Option<ObservedProviderAction>, DbErr> {
        Self::observe_provider_with_parent_on(txn, generation, Some(parent)).await
    }

    /// Historical receipt restoration accepts failure evidence without projecting
    /// it into successful-step or message-send evidence.
    pub(crate) async fn original_task_terminal_on(
        txn: &DatabaseTransaction,
        generation: &str,
        parent: &desk_agent_protocol::capability_grant::TaskGrantProvenance,
    ) -> Result<Option<OriginalResult>, DbErr> {
        let (outbox, work, payload) = original_on(txn, generation).await?;
        if work.manual_resolved_at.is_some() {
            return Err(invalid());
        }
        let Some(original) = terminal_result(&outbox, work.clone(), &payload)? else {
            return Ok(None);
        };
        Self::validate_original_authority_on(
            txn,
            &work,
            &outbox,
            &payload,
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
        let (outbox, work, payload) = original_on(txn, generation).await?;
        Self::validate_original_authority_on(txn, &work, &outbox, &payload, Some(parent), None)
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
                (CapabilityGrantIssuer::TaskAuthorization(issued_parent), Some(current)) => {
                    issued_parent != current
                }
                (CapabilityGrantIssuer::TaskAuthorization(_), None) | (_, Some(_)) => true,
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

    async fn observe_provider_with_parent_on(
        txn: &DatabaseTransaction,
        generation: &str,
        parent: Option<&desk_agent_protocol::capability_grant::TaskGrantProvenance>,
    ) -> Result<Option<ObservedProviderAction>, DbErr> {
        let (outbox, work, payload) = original_on(txn, generation).await?;
        if work.manual_resolved_at.is_some() {
            return Err(invalid());
        }
        let bound = binding(&outbox, &work, &payload)?;
        if work.result_json.is_none() {
            return Ok(None);
        }
        let Some(terminal) = decode(&outbox, &work, &payload, &bound)?.terminal else {
            return Ok(None);
        };
        if terminal.observation.native.result != ComputerActionResultClass::Verified {
            return Ok(None);
        }
        let issued = Self::validate_original_authority_on(
            txn,
            &work,
            &outbox,
            &payload,
            parent,
            Some(terminal.receipt.received_at_unix_ms),
        )
        .await?;
        let authority = &payload.observed_authority;
        let sent_message = if authority.effect == CapabilityEffect::SendExternal {
            let context = SentWebMessageContext {
                conversation_id: &work.conversation_id,
                provider_device_id: &bound.plan.device_id,
                received_at_unix_ms: terminal.receipt.received_at_unix_ms,
            };
            let Some(sent) = project_sent_web_message(
                &context,
                &payload.tool_name,
                &payload.canonical_input_json,
                &terminal.observation.native,
            )
            .map_err(|_| invalid())?
            else {
                return Ok(None);
            };
            Some(sent)
        } else {
            None
        };
        let prepared_message =
            desk_diagnose_core::communication_handoff::project_prepared_web_message(
                &SentWebMessageContext {
                    conversation_id: &work.conversation_id,
                    provider_device_id: &bound.plan.device_id,
                    received_at_unix_ms: terminal.receipt.received_at_unix_ms,
                },
                &payload.tool_name,
                &payload.canonical_input_json,
                &terminal.observation.native,
            )
            .map_err(|_| invalid())?;
        Ok(Some(ObservedProviderAction {
            work_id: work.id,
            browser_page: match &terminal.observation.native.output {
                Some(desk_agent_protocol::computer_use::ComputerActionOutput::Browser(result)) => {
                    Some(result.page.clone())
                }
                _ => None,
            },
            grant_id: issued.grant_id,
            origin: bound.origin,
            authority: payload.observed_authority,
            issued_by: issued.issued_by,
            completed_at: terminal.receipt.received_at_unix_ms,
            completion_sha256: format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&terminal).map_err(json_error)?)
            ),
            output_sha256: format!(
                "{:x}",
                Sha256::digest(
                    desk_diagnose_core::model_egress::message_payload_bytes(
                        &terminal.projection.content,
                        None,
                    )
                    .map_err(|_| invalid())?
                )
            ),
            canonical_input_json: payload.canonical_input_json.clone(),
            receipt: terminal.receipt,
            sent_message,
            created_artifact: match &terminal.observation.native.output {
                Some(desk_agent_protocol::computer_use::ComputerActionOutput::FileArtifact(
                    artifact,
                )) => {
                    desk_diagnose_core::schedule::source_graph::attachment::verify_text_artifact_output(
                        &payload.tool_name, &payload.canonical_input_json, artifact,
                    ).map_err(|_| invalid())?;
                    Some(artifact.clone())
                }
                _ => None,
            },
            prepared_message,
        }))
    }
}
