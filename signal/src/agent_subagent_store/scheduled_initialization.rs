//! Bind the published occurrence before its first physical model call.

use super::*;
use desk_agent_protocol::data_lineage::DestinationIdentity;
use desk_diagnose_core::subagent::creation::CreationEnvelope;

impl SubAgentStore {
    /// Model selection happens before this short transaction. The original
    /// session holder and publication fence are checked again before commit.
    pub(crate) async fn initialize_scheduled_source(
        &self,
        held: &PersistedAgentSession,
        node: &str,
        run_epoch: i64,
        destination: &DestinationIdentity,
    ) -> Result<(PersistedAgentSession, CreationEnvelope), DbErr> {
        let txn = crate::db::begin_write(&self.db, session_row::Entity).await?;
        let row = crate::schedule_store::lock_action_session(&txn, &held.conversation_id)
            .await
            .map_err(|_| invalid())?
            .ok_or_else(invalid)?;
        let mut current =
            PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
        if current != *held || !current.agent_role.is_main() {
            return Err(invalid());
        }
        let authority = crate::schedule_store::fresh_action_authority_on(&txn, &current)
            .await
            .map_err(|_| invalid())?;
        if authority.run().lease_owner.as_deref() != Some(node)
            || authority.run().lease_epoch != run_epoch
        {
            return Err(invalid());
        }
        let creation = if current.version == 1 {
            if current.delegation_group_id.is_some() || authority.run().result_ref.is_some() {
                return Err(invalid());
            }
            let creation = CreationEnvelope::capture_scheduled(
                &current,
                authority.delegation_source().map_err(|_| invalid())?,
                destination.clone(),
            )
            .map_err(|_| invalid())?;
            super::initialization::persist_source_group_on(
                &txn,
                &mut current,
                &creation,
                None,
                authority.verified_at(),
            )
            .await?;
            current.conversation[0] = creation.owner_requirement.clone();
            current.version = current.version.checked_add(1).ok_or_else(invalid)?;
            current.updated_at = chrono::DateTime::from_timestamp_millis(authority.verified_at())
                .ok_or_else(invalid)?
                .to_rfc3339();
            let changed = session_row::Entity::update_many()
                .set(session_row::ActiveModel {
                    state_json: Set(current.encode_json_for_storage().map_err(|_| invalid())?),
                    version: Set(current.version),
                    updated_at: Set(chrono::DateTime::from_timestamp_millis(
                        authority.verified_at(),
                    )
                    .ok_or_else(invalid)?),
                    ..Default::default()
                })
                .filter(session_row::Column::Id.eq(row.id))
                .filter(session_row::Column::Version.eq(row.version))
                .filter(session_row::Column::LeaseToken.eq(row.lease_token))
                .exec(&txn)
                .await?;
            if changed.rows_affected != 1 {
                return Err(invalid());
            }
            creation
        } else {
            let id = current.delegation_group_id.as_deref().ok_or_else(invalid)?;
            let source = group_row::Entity::find()
                .filter(group_row::Column::GroupId.eq(id))
                .filter(group_row::Column::RootConversationId.eq(&current.conversation_id))
                .filter(group_row::Column::ActorId.eq(&current.actor_id))
                .filter(group_row::Column::DeviceId.eq(&current.device_id))
                .one(&txn)
                .await?
                .ok_or_else(invalid)?;
            let creation = decode_creation(&source)?;
            if creation.model_destination != *destination
                || creation.scheduled_source.as_ref()
                    != Some(&authority.delegation_source().map_err(|_| invalid())?)
                || !decode_group(&source)?
                    .can_interpret(current.input_revision, current.control_revision)
            {
                return Err(invalid());
            }
            creation
        };
        let source = group_row::Entity::find()
            .filter(
                group_row::Column::GroupId
                    .eq(current.delegation_group_id.as_deref().ok_or_else(invalid)?),
            )
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        super::scheduled_source::current_scheduled_source_on(&txn, &source)
            .await?
            .ok_or_else(invalid)?;
        let after = crate::schedule_store::fresh_action_authority_on(&txn, &current)
            .await
            .map_err(|_| invalid())?;
        if after.provenance() != authority.provenance()
            || after.run().lease_epoch != run_epoch
            || after.run().lease_owner.as_deref() != Some(node)
        {
            return Err(invalid());
        }
        txn.commit().await?;
        Ok((current, creation))
    }
}
