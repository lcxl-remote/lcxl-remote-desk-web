//! Reuse the original context selection for tool-free completion interpretation.

use super::computer_background::bound;
use super::computer_binding::original_on;
use super::*;
use desk_agent_protocol::data_lineage::DestinationIdentity;
use desk_diagnose_core::{model_egress::ModelEgressPolicy, session::WorkKind};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ComputerExportContext {
    schema_version: u16,
    pub destination: DestinationIdentity,
    pub selected_source_tools: BTreeSet<String>,
    pub export_authorization_id: String,
    attachments_sha256: String,
    captured_at_unix_ms: u64,
    expires_at_unix_ms: u64,
}

fn invalid() -> DbErr {
    DbErr::Custom("original completion model authorization is unavailable".into())
}

fn attachments_digest(session: &PersistedAgentSession) -> Result<String, DbErr> {
    Ok(format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                &session.context_attachments,
                session.policy_revision,
                &session.scope_snapshot
            ))
            .map_err(|_| invalid())?
        )
    ))
}

impl ComputerExportContext {
    pub(super) fn capture(
        policy: &ModelEgressPolicy,
        session: &PersistedAgentSession,
        captured_at: i64,
    ) -> Result<Self, DbErr> {
        let now = u64::try_from(captured_at).map_err(|_| invalid())?;
        let mut expires_at = now.saturating_add(5 * 60 * 1000);
        for attachment in &session.context_attachments {
            attachment.validate().map_err(|_| invalid())?;
            if attachment.is_active_at(now) {
                expires_at = expires_at.min(attachment.expires_at_unix_ms);
            }
        }
        if let Some(expiry) = &session.scope_snapshot.expires_at {
            let expiry = chrono::DateTime::parse_from_rfc3339(expiry)
                .map_err(|_| invalid())?
                .timestamp_millis();
            expires_at = expires_at.min(u64::try_from(expiry).map_err(|_| invalid())?);
        }
        let context = Self {
            schema_version: 1,
            destination: policy.destination.clone(),
            selected_source_tools: policy.selected_source_tools.clone(),
            export_authorization_id: policy.export_authorization_id.clone(),
            attachments_sha256: attachments_digest(session)?,
            captured_at_unix_ms: now,
            expires_at_unix_ms: expires_at,
        };
        context.validate()?;
        if expires_at <= now {
            return Err(invalid());
        }
        Ok(context)
    }

    pub(super) fn validate(&self) -> Result<(), DbErr> {
        self.destination.validate().map_err(|_| invalid())?;
        if self.schema_version != 1
            || self.captured_at_unix_ms == 0
            || self.expires_at_unix_ms <= self.captured_at_unix_ms
            || self.export_authorization_id.is_empty()
            || self.export_authorization_id.len() > 256
            || self.export_authorization_id.chars().any(char::is_control)
            || self.attachments_sha256.len() != 64
            || !self
                .attachments_sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
            || self.selected_source_tools.len() > 128
            || self.selected_source_tools.iter().any(|name| {
                name.is_empty() || name.len() > 128 || name.chars().any(char::is_control)
            })
        {
            return Err(invalid());
        }
        Ok(())
    }
}

impl SignalCapabilityGrantStore {
    /// Validate the persisted original completion, never synthesize export
    /// authority from a write grant or names found in a model conversation.
    pub(crate) async fn computer_completion_export(
        &self,
        session: &PersistedAgentSession,
        event_id: &str,
        destination: &DestinationIdentity,
    ) -> Result<ComputerExportContext, DbErr> {
        let pending = session
            .pending_auto_triggers
            .iter()
            .find(|pending| pending.event_id == event_id)
            .ok_or_else(invalid)?;
        if pending.kind != WorkKind::ComputerAction || pending.chain_id != session.chain_id {
            return Err(invalid());
        }
        let txn = self.db.begin().await?;
        let result = async {
            let (outbox, work, payload) = original_on(&txn, &pending.execution_id).await?;
            let binding = bound(&outbox, &work, &payload)?;
            if work.id != pending.work_id
                || work.conversation_id != session.conversation_id
                || work.actor_id != session.actor_id
                || work.target_device_id != session.device_id
                || work.completion_event_id != pending.event_id
                || binding.origin.tool_call_id != pending.tool_call_id
                || binding.origin.turn_fence.input_revision != session.input_revision
                || binding
                    .execution
                    .as_ref()
                    .is_none_or(|execution| execution.chain_id != session.chain_id)
            {
                return Err(invalid());
            }
            let export = binding.model_export.ok_or_else(invalid)?;
            export.validate()?;
            let now = u64::try_from(Utc::now().timestamp_millis()).map_err(|_| invalid())?;
            if &export.destination != destination
                || export.expires_at_unix_ms <= now
                || now < export.captured_at_unix_ms
                || export.attachments_sha256 != attachments_digest(session)?
                || !export
                    .selected_source_tools
                    .contains(&binding.origin.tool_name)
            {
                return Err(invalid());
            }
            let original = super::computer_completion::terminal_result(&outbox, work, &payload)?
                .ok_or_else(invalid)?;
            super::computer_delivery::validate_destination(session, &original)?;
            if !session
                .conversation
                .iter()
                .any(|message| message.message_id == pending.event_id)
            {
                return Err(invalid());
            }
            Ok(export)
        }
        .await;
        txn.rollback().await?;
        result
    }
}
