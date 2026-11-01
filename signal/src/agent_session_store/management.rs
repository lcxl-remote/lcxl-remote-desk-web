//! Retirement of unsupported snapshots uses durable identity and native ledgers.
use super::*;
use desk_diagnose_core::{goal::GoalRemovalReason, session::management::SessionManagementMetadata};
use sea_orm::{DbErr, ExprTrait};

/// This does not repair the snapshot or settle unknown provider usage.
pub(crate) fn retire_unsupported_for_expiry<'a>(
    db: &'a DatabaseConnection,
    candidate: &'a agent_session::Model,
    cutoff: chrono::DateTime<chrono::Utc>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, DbErr>> + Send + 'a>> {
    desk_diagnose_core::future::send_phase(|| async move {
        if candidate.conversation_schema_version
            == Some(i32::from(
                desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION,
            ))
        {
            return Ok(false);
        }
        let Ok(metadata) = SessionManagementMetadata::decode(&candidate.state_json) else {
            return Ok(false);
        };
        if !metadata.matches_subject(
            &candidate.conversation_id,
            &candidate.actor_id,
            &candidate.device_id,
        ) || metadata.surface != AgentSessionSurface::AiAssistant
        {
            return Ok(false);
        }
        let txn = crate::db::begin_write(db, agent_session::Entity).await?;

        let now_ms = chrono::Utc::now().timestamp_millis();
        let now = chrono::DateTime::from_timestamp_millis(now_ms)
            .ok_or_else(|| DbErr::Custom("invalid clock".into()))?;
        let current = agent_session::Entity::find_by_id(candidate.id)
            .filter(agent_session::Column::ConversationId.eq(&candidate.conversation_id))
            .filter(agent_session::Column::ActorId.eq(&candidate.actor_id))
            .filter(agent_session::Column::DeviceId.eq(&candidate.device_id))
            .filter(agent_session::Column::Version.eq(candidate.version))
            .filter(agent_session::Column::LeaseToken.eq(candidate.lease_token))
            .filter(agent_session::Column::StateJson.eq(&candidate.state_json))
            .filter(agent_session::Column::UpdatedAt.lt(cutoff))
            .filter(
                sea_orm::Condition::any()
                    .add(agent_session::Column::LeaseDeadline.is_null())
                    .add(agent_session::Column::LeaseDeadline.lt(now)),
            )
            .one(&txn)
            .await?;
        let Some(row) = current else {
            return Ok(false);
        };
        let child = crate::entity::agent_subagent_run::Entity::find()
            .filter(
                crate::entity::agent_subagent_run::Column::ChildConversationId
                    .eq(&row.conversation_id),
            )
            .one(&txn)
            .await?;
        if let Some(child) = &child {
            if child.actor_id != row.actor_id
                || child.device_id != row.device_id
                || metadata.is_main()
            {
                return Ok(false);
            }
        } else if !metadata.is_main() {
            return Ok(false);
        }
        let ends = if child.is_none() {
            crate::agent_subagent_store::close_root_for_subject_on(
                &txn,
                &row.conversation_id,
                &row.actor_id,
                &row.device_id,
                now_ms,
            )
            .await?
        } else {
            crate::agent_subagent_store::tombstone_on(
                &txn,
                &row.conversation_id,
                &row.actor_id,
                &row.device_id,
                now_ms,
            )
            .await?;
            crate::agent_subagent_store::cancel_native_actions_for_subject_on(
                &txn,
                &row.conversation_id,
                &row.actor_id,
                &row.device_id,
                "conversation_expired",
                now_ms,
            )
            .await?;
            Default::default()
        };
        crate::agent_goal_store::cancel_conversation_goals_on(
            &txn,
            &row.conversation_id,
            GoalRemovalReason::ConversationRemoved,
            u64::try_from(now_ms).map_err(|_| DbErr::Custom("invalid clock".into()))?,
        )
        .await?;
        super::deletion::stop_removed_work_on(&txn, &row.conversation_id, &row.actor_id, now_ms)
            .await?;
        use crate::entity::agent_schedule as timer;
        timer::Entity::update_many()
            .col_expr(timer::Column::Status, Expr::value("expired"))
            .col_expr(
                timer::Column::Revision,
                Expr::col(timer::Column::Revision).add(1),
            )
            .col_expr(timer::Column::NextRunAt, Expr::value(Option::<i64>::None))
            .filter(timer::Column::SourceConversationId.eq(&row.conversation_id))
            .filter(timer::Column::Kind.eq("conversation_resume"))
            .filter(timer::Column::Status.is_in([
                "pending_review",
                "active",
                "triggered",
                "paused",
            ]))
            .exec(&txn)
            .await?;
        let changed = agent_session::Entity::update_many()
            .set(agent_session::ActiveModel {
                version: Set(row
                    .version
                    .checked_add(1)
                    .ok_or_else(|| DbErr::Custom("version overflow".into()))?),
                lease_token: Set(row
                    .lease_token
                    .checked_add(1)
                    .ok_or_else(|| DbErr::Custom("lease overflow".into()))?),
                lease_deadline: Set(None),

                ..Default::default()
            })
            .filter(agent_session::Column::Id.eq(row.id))
            .filter(agent_session::Column::Version.eq(row.version))
            .filter(agent_session::Column::LeaseToken.eq(row.lease_token))
            .filter(agent_session::Column::StateJson.eq(&row.state_json))
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(DbErr::Custom("expired conversation changed".into()));
        }
        txn.commit().await?;
        ends.submit(now_ms, crate::model_metrics::runtime::submit);
        Ok(true)
    })
}

/// The cursor is only a scan position. Each instance repeats all deletion fences in DB.
pub(crate) async fn retire_unsupported_sessions(
    db: &DatabaseConnection,
    cutoff: chrono::DateTime<chrono::Utc>,
    batches: usize,
    batch_rows: u64,
) -> Result<(), DbErr> {
    use std::sync::atomic::{AtomicI64, Ordering};
    static CURSOR: AtomicI64 = AtomicI64::new(0);
    let mut after = CURSOR.load(Ordering::Relaxed);
    for _ in 0..batches {
        let rows = agent_session::Entity::find()
            .filter(agent_session::Column::Id.gt(after))
            .filter(
                sea_orm::Condition::any()
                    .add(agent_session::Column::ConversationSchemaVersion.is_null())
                    .add(
                        agent_session::Column::ConversationSchemaVersion.ne(i32::from(
                            desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION,
                        )),
                    ),
            )
            .filter(agent_session::Column::UpdatedAt.lt(cutoff))
            .order_by_asc(agent_session::Column::Id)
            .limit(batch_rows)
            .all(db)
            .await?;
        for row in &rows {
            retire_unsupported_for_expiry(db, row, cutoff).await?;
            after = row.id;
        }
        if rows.len() < batch_rows as usize {
            after = 0;
            break;
        }
    }
    CURSOR.store(after, Ordering::Relaxed);
    Ok(())
}
