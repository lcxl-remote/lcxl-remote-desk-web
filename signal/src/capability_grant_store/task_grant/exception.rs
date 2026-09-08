//! Consume the original approval and link it to the exact task allocation.
use super::*;
use crate::entity::agent_capability_grant as grant_row;
use sea_orm::{QueryOrder, QuerySelect, Set};

pub(super) async fn consume(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    call: &CapabilityGrantCall<'_>,
    now: i64,
) -> Result<Option<(String, u64)>, DbErr> {
    let rows = grant_row::Entity::find()
        .filter(grant_row::Column::RunId.eq(&session.conversation_id))
        .filter(grant_row::Column::ActorId.eq(&session.actor_id))
        .filter(grant_row::Column::Status.eq("active"))
        .filter(grant_row::Column::RemainingUses.eq(1))
        .order_by_asc(grant_row::Column::Id)
        .lock_exclusive()
        .all(txn)
        .await?;
    let mut checked = call.clone();
    checked.now_unix_ms = u64::try_from(now).map_err(|_| invalid())?;
    for row in rows {
        let mut grant = crate::capability_grant_store::decode_grant(&row)?;
        if !desk_diagnose_core::schedule::contract::exception::matches_one_call_approval(
            session, &grant, &checked,
        ) {
            continue;
        }
        grant.remaining_uses = 0;
        grant.validate().map_err(|_| invalid())?;
        let timestamp = chrono::DateTime::from_timestamp_millis(now).ok_or_else(invalid)?;
        let changed = grant_row::Entity::update_many()
            .set(grant_row::ActiveModel {
                remaining_uses: Set(0),
                payload_json: Set(serde_json::to_string(&grant).map_err(|_| invalid())?),
                version: Set(row.version.checked_add(1).ok_or_else(invalid)?),
                updated_at: Set(timestamp),
                ..Default::default()
            })
            .filter(grant_row::Column::Id.eq(row.id))
            .filter(grant_row::Column::Version.eq(row.version))
            .filter(grant_row::Column::RemainingUses.eq(1))
            .filter(grant_row::Column::Status.eq("active"))
            .exec(txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(invalid());
        }
        return Ok(Some((grant.grant_id, grant.expires_at_unix_ms)));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{
        ConnectionTrait, Database, DbBackend, EntityTrait, PaginatorTrait, Schema, TransactionTrait,
    };

    #[tokio::test]
    async fn missing_approval_is_distinct_from_failed_storage_and_consumes_nothing() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let session = PersistedAgentSession::new(
            "run",
            "owner",
            "device",
            1,
            desk_agent_protocol::AgentScope {
                granted: vec![],
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-09-07T00:00:00Z",
        );
        let call = CapabilityGrantCall {
            actor_id: "owner",
            run_id: "run",
            input_revision: 1,
            surface: desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner,
            target_device_id: "device",
            target_session_id: None,
            provider_id: "device",
            capability_id: "inspect",
            tool_name: "inspect",
            tool_schema_version: 1,
            effect: desk_agent_protocol::capability_provider::CapabilityEffect::ReadDevice,
            risk_tier: desk_agent_protocol::capability_grant::CapabilityRiskTier::R0,
            resource_scope: &[],
            operation_scope: &[],
            export_destinations: &[],
            envelope_ids: &[],
            content_digests_sha256: &[],
            canonical_input_digest_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            byte_count: 1,
            item_count: 1,
            policy_revision: 1,
            readiness_revision: 1,
            now_unix_ms: 1000,
        };
        let txn = db.begin().await.unwrap();
        assert!(consume(&txn, &session, &call, 1000).await.is_err());
        txn.rollback().await.unwrap();
        let table = Schema::new(DbBackend::Sqlite).create_table_from_entity(grant_row::Entity);
        db.execute(&table).await.unwrap();
        let txn = db.begin().await.unwrap();
        assert_eq!(consume(&txn, &session, &call, 1000).await.unwrap(), None);
        assert_eq!(grant_row::Entity::find().count(&txn).await.unwrap(), 0);
        txn.rollback().await.unwrap();
    }
}
