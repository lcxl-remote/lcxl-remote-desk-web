//! Save history, task dependencies and repair allowance under the same child claim.
use super::*;
use crate::config::connection::DatabaseTransaction;

pub(crate) async fn save_child_session(
    db: &DatabaseConnection,
    session: &mut PersistedAgentSession,
) -> Result<(), DbErr> {
    desk_diagnose_core::image_input::retain_latest_session_image(&mut session.conversation)
        .map_err(|_| invalid())?;
    session.agent_role.binding().ok_or_else(invalid)?;
    let txn = crate::db::begin_write(db, session_row::Entity).await?;

    let now = chrono::Utc::now();
    let (row, mut run, group) =
        super::lifecycle::child_records_on(&txn, session, now.timestamp_millis()).await?;
    let previous_revision = run.state_revision;
    if run.report_corrections_used != session.subagent_report_corrections_used {
        run.report_corrections_used = session.subagent_report_corrections_used;
        run.state_revision = run.state_revision.checked_add(1).ok_or_else(invalid)?;
        run.updated_at = now.to_rfc3339();
    }
    let mut next = session.clone();
    let deadline_reached = now.timestamp_millis() >= run.binding.deadline_ms
        || now.timestamp_millis() >= group.limits.deadline_ms;
    if deadline_reached {
        run.fail("delegation_deadline_reached", &now.to_rfc3339())
            .map_err(|_| invalid())?;
        next.finish_turn(
            desk_diagnose_core::session::TurnState::Failed,
            now.to_rfc3339(),
        );
        next.pending_auto_triggers.clear();
    } else if session.turn_state == desk_diagnose_core::session::TurnState::Failed {
        run.fail("delegated_turn_failed", &now.to_rfc3339())
            .map_err(|_| invalid())?;
        next.pending_auto_triggers.clear();
    } else {
        let facts = super::facts::runtime_facts_on(&txn, session, now.timestamp_millis()).await?;
        run.synchronize_dependencies(
            facts.dependencies,
            session.turn_state.is_active(),
            &now.to_rfc3339(),
        )
        .map_err(|_| invalid())?;
    }
    if run.state_revision != previous_revision {
        replace_run_on(&txn, &row, &run, now.timestamp_millis()).await?;
        append_state_event_on(&txn, &group, &run, now.timestamp_millis()).await?;
    }
    let permission_ends =
        desk_diagnose_core::model_observability::permission::PendingEnds::task_control(
            session,
            &run,
            deadline_reached,
        );
    let version = write_child_session_on(&txn, &next, now).await?;
    txn.commit().await?;
    permission_ends.submit(
        now.timestamp_millis(),
        crate::model_metrics::runtime::submit,
    );
    next.version = version;
    *session = next;
    Ok(())
}

/// Writes only the held version and lease. Receipt paths keep their own
/// reconciliation transaction and do not call this planning-state writer.
pub(crate) async fn write_child_session_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<i64, DbErr> {
    if session.turn_state == desk_diagnose_core::session::TurnState::Failed
        && session.agent_role.binding().is_some()
    {
        let binding = session.agent_role.binding().ok_or_else(invalid)?;
        let operation = format!("terminal:{}:{}", binding.task_id, binding.control_revision);
        super::native_cancel::cancel_native_actions_on(
            txn,
            session,
            &operation,
            now.timestamp_millis(),
        )
        .await?;
    }
    let version = session.version.checked_add(1).ok_or_else(invalid)?;
    let mut stored = session.clone();
    desk_diagnose_core::image_input::retain_latest_session_image(&mut stored.conversation)
        .map_err(|_| invalid())?;
    desk_diagnose_core::image_input::strip_session_images(&mut stored.conversation);
    stored.version = version;
    let deadline = session
        .turn_state
        .is_active()
        .then_some(now + chrono::Duration::seconds(90));
    let changed = session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(stored.encode_json_for_storage().map_err(|_| invalid())?),
            version: Set(version),
            lease_deadline: Set(deadline),
            updated_at: Set(now),

            ..Default::default()
        })
        .filter(session_row::Column::ConversationId.eq(&session.conversation_id))
        .filter(session_row::Column::ActorId.eq(&session.actor_id))
        .filter(session_row::Column::DeviceId.eq(&session.device_id))
        .filter(session_row::Column::Version.eq(session.version))
        .filter(
            session_row::Column::LeaseToken
                .eq(i64::try_from(session.lease_token).map_err(|_| invalid())?),
        )
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    Ok(version)
}
