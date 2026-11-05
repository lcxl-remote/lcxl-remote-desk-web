//! External reads retain the original native lease without requiring a device.

use super::*;
use desk_agent_protocol::data_lineage::DestinationIdentity;

impl SignalAgentRunEventStore {
    pub(crate) async fn checked_external_read_input(
        &self,
        held: &PersistedAgentSession,
        destination: &DestinationIdentity,
        call_id: Option<&str>,
    ) -> Result<ReadContextSelection, AgentError> {
        let txn = self
            .db
            .begin()
            .await
            .map_err(|_| internal("External read storage is unavailable"))?;
        let original =
            Self::checked_external_read_input_on(&txn, held, destination, call_id).await?;
        txn.commit()
            .await
            .map_err(|_| internal("External read storage is unavailable"))?;
        Ok(original)
    }

    pub(crate) async fn checked_external_read_input_on(
        txn: &crate::config::connection::DatabaseTransaction,
        held: &PersistedAgentSession,
        destination: &DestinationIdentity,
        call_id: Option<&str>,
    ) -> Result<ReadContextSelection, AgentError> {
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&held.conversation_id))
            .one(txn)
            .await
            .map_err(|_| internal("External read storage is unavailable"))?
            .ok_or_else(|| internal("External read session is unavailable"))?;
        let current = Box::new(input_context::decode_session(
            &row,
            InputSubject {
                run_id: &held.conversation_id,
                actor_id: &held.actor_id,
                device_id: &held.device_id,
                client_conversation_id: held.client_conversation_id.as_deref(),
            },
        )?);
        let now = chrono::Utc::now();
        if u64::try_from(row.lease_token).ok() != Some(current.lease_token)
            || row.lease_deadline.is_none_or(|until| until <= now)
            || row.conversation_schema_version
                != Some(i32::from(
                    desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION,
                ))
        {
            return Err(internal("External read session lease or subject changed"));
        }
        desk_diagnose_core::external_tools::validate_held_input(
            held,
            &current,
            destination,
            call_id,
            now.timestamp_millis(),
        )?;
        let original = input_context::original_on(txn, &current)
            .await?
            .ok_or_else(|| internal("Original external read input is unavailable"))?;
        Ok(original)
    }
}
