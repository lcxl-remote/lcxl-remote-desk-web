//! Conversation removal closes every source before retaining reconciliation evidence.
use super::*;
use crate::config::connection::DatabaseTransaction;
use crate::entity::{
    agent_delegation_reservation as reservation, agent_file_recovery_cleanup as tombstone,
};
use sea_orm::{QueryTrait, sea_query::OnConflict};

/// The cleanup queue is also the durable deletion tombstone. Completing file
/// cleanup does not permit reuse of the original conversation identity.
pub(crate) async fn deleted_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    conversation: &str,
) -> Result<bool, DbErr> {
    Ok(tombstone::Entity::find_by_id(conversation)
        .one(db)
        .await?
        .is_some())
}

pub(crate) async fn tombstone_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    conversation: &str,
    actor: &str,
    device: &str,
    now_ms: i64,
) -> Result<(), DbErr> {
    tombstone::Entity::insert(tombstone::ActiveModel {
        conversation_id: Set(conversation.into()),
        actor_id: Set(actor.into()),
        device_id: Set(device.into()),
        created_at_unix_ms: Set(now_ms),
        next_attempt_at_unix_ms: Set(now_ms),
        attempts: Set(0),
        lease_id: Set(None),
        lease_until_unix_ms: Set(None),
        completed_at_unix_ms: Set(None),
        last_error: Set(None),
    })
    .on_conflict(
        OnConflict::column(tombstone::Column::ConversationId)
            .do_nothing()
            .to_owned(),
    )
    .try_insert()
    .exec(db)
    .await?;
    let row = tombstone::Entity::find_by_id(conversation)
        .one(db)
        .await?
        .ok_or_else(invalid)?;
    if row.actor_id != actor || row.device_id != device {
        return Err(invalid());
    }
    Ok(())
}

/// The caller holds root and child control and source schedules. No external
/// I/O occurs here; dispatchers consume the committed original native stops.
pub(crate) async fn close_root_on(
    db: &DatabaseTransaction,
    parent: &PersistedAgentSession,
    now_ms: i64,
) -> Result<desk_diagnose_core::model_observability::permission::PendingEnds, DbErr> {
    if !parent.agent_role.is_main() {
        return Err(invalid());
    }
    tombstone_on(
        db,
        &parent.conversation_id,
        &parent.actor_id,
        &parent.device_id,
        now_ms,
    )
    .await?;
    // Diagnostic and terminal sessions have no delegation authority or native
    // assistant execution identity; retain their existing lifecycle.
    if parent.surface != AgentSessionSurface::AiAssistant {
        return Ok(Default::default());
    }
    let mut permission_ends =
        desk_diagnose_core::model_observability::permission::PendingEnds::waiting(
            parent,
            desk_diagnose_core::model_observability::PermissionOutcome::Cancelled,
        );
    let now = chrono::DateTime::from_timestamp_millis(now_ms)
        .ok_or_else(invalid)?
        .to_rfc3339();
    let mut after = 0;
    loop {
        let rows = group_row::Entity::find()
            .filter(group_row::Column::RootConversationId.eq(&parent.conversation_id))
            .filter(group_row::Column::ActorId.eq(&parent.actor_id))
            .filter(group_row::Column::DeviceId.eq(&parent.device_id))
            .filter(group_row::Column::Id.gt(after))
            .order_by_asc(group_row::Column::Id)
            .limit(100)
            .all(db)
            .await?;
        if rows.is_empty() {
            break;
        }
        after = rows.last().ok_or_else(invalid)?.id;
        for row in rows {
            let mut group = decode_group(&row)?;
            group.stop_parent().map_err(|_| invalid())?;
            if group.source_admission != SourceAdmission::Closed {
                group
                    .set_source_admission(SourceAdmission::Closed)
                    .map_err(|_| invalid())?;
            }
            replace_group_on(db, &row, &group, now_ms).await?;
            let children = run_row::Entity::find()
                .filter(run_row::Column::GroupId.eq(&group.group_id))
                .filter(run_row::Column::RootConversationId.eq(&parent.conversation_id))
                .filter(run_row::Column::ActorId.eq(&parent.actor_id))
                .filter(run_row::Column::DeviceId.eq(&parent.device_id))
                .order_by_asc(run_row::Column::Id)
                .all(db)
                .await?;
            for child in children {
                let mut run = decode_run(&child)?;
                tombstone_on(
                    db,
                    &run.child_conversation_id,
                    &parent.actor_id,
                    &parent.device_id,
                    now_ms,
                )
                .await?;
                if run.state.is_terminal() {
                    continue;
                }
                run.request_cancel(run.fence(), &now)
                    .map_err(|_| invalid())?;
                run.binding.source_epoch = group.source_epoch;
                run.settle_cancel(&now).map_err(|_| invalid())?;
                replace_run_on(db, &child, &run, now_ms).await?;
                permission_ends.extend(synchronize_control_on(db, &run, now_ms, false).await?);
                append_state_event_on(db, &group, &run, now_ms).await?;
            }
        }
    }
    super::native_cancel::cancel_native_actions_on(db, parent, "conversation_removed", now_ms)
        .await?;
    Ok(permission_ends)
}

/// Filter before LIMIT and recheck under control locks. A parent's idle state
/// does not imply its independent tasks or provider ledgers are settled.
pub(crate) fn reclaim_condition(cutoff_ms: i64) -> sea_orm::Condition {
    let live = || {
        run_row::Entity::find()
            .select_only()
            .column(run_row::Column::RootConversationId)
            .filter(
                sea_orm::Condition::any()
                    .add(run_row::Column::State.is_not_in(["completed", "failed", "cancelled"]))
                    .add(run_row::Column::UpdatedAt.gte(cutoff_ms)),
            )
            .into_query()
    };
    let unfinished = || {
        run_row::Entity::find()
            .select_only()
            .column(run_row::Column::ChildConversationId)
            .filter(run_row::Column::State.is_not_in(["completed", "failed", "cancelled"]))
            .into_query()
    };
    let unsettled_roots = || {
        reservation::Entity::find()
            .select_only()
            .column(reservation::Column::RootConversationId)
            .filter(
                sea_orm::Condition::any()
                    .add(reservation::Column::State.is_in(["reserved", "usage_unknown"]))
                    .add(
                        reservation::Column::ReservationId
                            .in_subquery(crate::agent_approval_usage::pinned_reservation_ids()),
                    ),
            )
            .into_query()
    };
    let unsettled_children = || {
        reservation::Entity::find()
            .select_only()
            .column(reservation::Column::ConversationId)
            .filter(
                sea_orm::Condition::any()
                    .add(reservation::Column::State.is_in(["reserved", "usage_unknown"]))
                    .add(
                        reservation::Column::ReservationId
                            .in_subquery(crate::agent_approval_usage::pinned_reservation_ids()),
                    ),
            )
            .into_query()
    };
    let native = || {
        crate::entity::agent_action_item::Entity::find()
            .select_only()
            .column(crate::entity::agent_action_item::Column::ConversationId)
            .filter(
                crate::entity::agent_action_item::Column::Status
                    .is_in(crate::usage_retention::UNRESOLVED_ACTION_STATES),
            )
            .into_query()
    };
    let native_roots = run_row::Entity::find()
        .select_only()
        .column(run_row::Column::RootConversationId)
        .filter(run_row::Column::ChildConversationId.in_subquery(native()))
        .into_query();
    let commands = crate::entity::agent_exec_task::Entity::find()
        .select_only()
        .column(crate::entity::agent_exec_task::Column::ConversationId)
        .filter(
            crate::entity::agent_exec_task::Column::Status
                .is_in(crate::usage_retention::UNRESOLVED_EXEC_STATES),
        )
        .into_query();
    let command_roots = run_row::Entity::find()
        .select_only()
        .column(run_row::Column::RootConversationId)
        .filter(run_row::Column::ChildConversationId.in_subquery(commands))
        .into_query();
    sea_orm::Condition::all()
        .add(session_row::Column::ConversationId.not_in_subquery(native_roots))
        .add(session_row::Column::ConversationId.not_in_subquery(command_roots))
        .add(session_row::Column::ConversationId.not_in_subquery(live()))
        .add(session_row::Column::ConversationId.not_in_subquery(unfinished()))
        .add(session_row::Column::ConversationId.not_in_subquery(unsettled_roots()))
        .add(session_row::Column::ConversationId.not_in_subquery(unsettled_children()))
}

/// Delete content only after the normal session sweep has reclaimed both sides
/// of every child. A retained session or unknown usage keeps original evidence.
pub(crate) async fn purge_groups(db: &DatabaseConnection, cutoff_ms: i64) -> Result<u64, DbErr> {
    let live_sessions = || {
        session_row::Entity::find()
            .select_only()
            .column(session_row::Column::ConversationId)
            .into_query()
    };
    let retained_groups = || {
        run_row::Entity::find()
            .select_only()
            .column(run_row::Column::GroupId)
            .filter(
                sea_orm::Condition::any()
                    .add(run_row::Column::State.is_not_in(["completed", "failed", "cancelled"]))
                    .add(run_row::Column::ChildConversationId.in_subquery(live_sessions())),
            )
            .into_query()
    };
    let reserved_groups = || {
        reservation::Entity::find()
            .select_only()
            .column(reservation::Column::GroupId)
            .filter(
                sea_orm::Condition::any()
                    .add(reservation::Column::State.is_in(["reserved", "usage_unknown"]))
                    .add(
                        reservation::Column::ReservationId
                            .in_subquery(crate::agent_approval_usage::pinned_reservation_ids()),
                    ),
            )
            .into_query()
    };
    let candidates = group_row::Entity::find()
        .filter(group_row::Column::UpdatedAt.lt(cutoff_ms))
        .filter(group_row::Column::SourceAdmission.eq("closed"))
        .filter(group_row::Column::RootConversationId.not_in_subquery(live_sessions()))
        .filter(group_row::Column::GroupId.not_in_subquery(retained_groups()))
        .filter(group_row::Column::GroupId.not_in_subquery(reserved_groups()))
        .order_by_asc(group_row::Column::Id)
        .limit(100)
        .all(db)
        .await?;
    let mut removed = 0;
    for candidate in candidates {
        let txn = crate::db::begin_write(db, session_row::Entity).await?;

        let row = group_row::Entity::find_by_id(candidate.id)
            .filter(group_row::Column::UpdatedAt.lt(cutoff_ms))
            .filter(group_row::Column::SourceAdmission.eq("closed"))
            .filter(group_row::Column::RootConversationId.not_in_subquery(live_sessions()))
            .filter(group_row::Column::GroupId.not_in_subquery(retained_groups()))
            .filter(group_row::Column::GroupId.not_in_subquery(reserved_groups()))
            .one(&txn)
            .await?;
        let Some(row) = row else {
            txn.commit().await?;
            continue;
        };
        decode_group(&row)?;
        crate::entity::agent_subagent_inbox::Entity::delete_many()
            .filter(crate::entity::agent_subagent_inbox::Column::GroupId.eq(&row.group_id))
            .exec(&txn)
            .await?;
        reservation::Entity::delete_many()
            .filter(reservation::Column::GroupId.eq(&row.group_id))
            .filter(reservation::Column::State.is_not_in(["reserved", "usage_unknown"]))
            .filter(
                reservation::Column::ReservationId
                    .not_in_subquery(crate::agent_approval_usage::pinned_reservation_ids()),
            )
            .exec(&txn)
            .await?;
        run_row::Entity::delete_many()
            .filter(run_row::Column::GroupId.eq(&row.group_id))
            .exec(&txn)
            .await?;
        removed += group_row::Entity::delete_by_id(row.id)
            .exec(&txn)
            .await?
            .rows_affected;
        txn.commit().await?;
    }
    Ok(removed)
}

/// Goal reservations may settle after deletion; retain their budget anchor
/// until their delegation ledger has been reclaimed as well.
pub(crate) fn pinned_goal_ids() -> sea_orm::sea_query::SelectStatement {
    group_row::Entity::find()
        .select_only()
        .column(group_row::Column::SourceGoalId)
        .filter(group_row::Column::SourceGoalId.is_not_null())
        .into_query()
}

/// Keep original approval provenance while a delegation group still anchors
/// descendants or unknown provider usage, even after the root session is gone.
pub(crate) fn pinned_conversation_ids() -> sea_orm::sea_query::SelectStatement {
    group_row::Entity::find()
        .select_only()
        .column(group_row::Column::RootConversationId)
        .into_query()
}

/// Sessionless authority is kept until both the native ledger and descendants
/// have settled. Tombstones themselves contain only immutable identity metadata.
pub(crate) async fn purge_authority(db: &DatabaseConnection, cutoff_ms: i64) -> Result<(), DbErr> {
    let live = || {
        session_row::Entity::find()
            .select_only()
            .column(session_row::Column::ConversationId)
            .into_query()
    };
    static CURSOR: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
    let after = CURSOR.lock().map_err(|_| invalid())?.clone();
    let candidates = tombstone::Entity::find()
        .filter(tombstone::Column::ConversationId.gt(after))
        .filter(tombstone::Column::CreatedAtUnixMs.lt(cutoff_ms))
        .filter(tombstone::Column::ConversationId.not_in_subquery(live()))
        .order_by_asc(tombstone::Column::ConversationId)
        .limit(100)
        .all(db)
        .await?;
    let next = if candidates.len() == 100 {
        candidates
            .last()
            .ok_or_else(invalid)?
            .conversation_id
            .clone()
    } else {
        String::new()
    };
    for candidate in candidates {
        let txn = crate::db::begin_write(db, session_row::Entity).await?;

        if session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq(&candidate.conversation_id))
            .one(&txn)
            .await?
            .is_some()
            || reservation::Entity::find()
                .filter(
                    sea_orm::Condition::any()
                        .add(reservation::Column::RootConversationId.eq(&candidate.conversation_id))
                        .add(reservation::Column::ConversationId.eq(&candidate.conversation_id)),
                )
                .filter(
                    sea_orm::Condition::any()
                        .add(reservation::Column::State.is_in(["reserved", "usage_unknown"]))
                        .add(
                            reservation::Column::ReservationId
                                .in_subquery(crate::agent_approval_usage::pinned_reservation_ids()),
                        ),
                )
                .one(&txn)
                .await?
                .is_some()
            || run_row::Entity::find()
                .filter(run_row::Column::RootConversationId.eq(&candidate.conversation_id))
                .filter(
                    sea_orm::Condition::any()
                        .add(run_row::Column::State.is_not_in(["completed", "failed", "cancelled"]))
                        .add(run_row::Column::ChildConversationId.in_subquery(live())),
                )
                .one(&txn)
                .await?
                .is_some()
            || crate::entity::agent_action_item::Entity::find()
                .filter(
                    crate::entity::agent_action_item::Column::ConversationId
                        .eq(&candidate.conversation_id),
                )
                .filter(
                    crate::entity::agent_action_item::Column::Status
                        .is_in(crate::usage_retention::UNRESOLVED_ACTION_STATES),
                )
                .one(&txn)
                .await?
                .is_some()
            || crate::entity::agent_exec_task::Entity::find()
                .filter(
                    crate::entity::agent_exec_task::Column::ConversationId
                        .eq(&candidate.conversation_id),
                )
                .filter(
                    crate::entity::agent_exec_task::Column::Status
                        .is_in(crate::usage_retention::UNRESOLVED_EXEC_STATES),
                )
                .one(&txn)
                .await?
                .is_some()
        {
            txn.commit().await?;
            continue;
        }
        crate::entity::agent_grant_reservation::Entity::delete_many()
            .filter(
                crate::entity::agent_grant_reservation::Column::RunId
                    .eq(&candidate.conversation_id),
            )
            .exec(&txn)
            .await?;
        crate::entity::agent_capability_grant::Entity::delete_many()
            .filter(
                crate::entity::agent_capability_grant::Column::RunId.eq(&candidate.conversation_id),
            )
            .exec(&txn)
            .await?;
        crate::entity::agent_permission_resume::Entity::delete_many()
            .filter(
                crate::entity::agent_permission_resume::Column::RunId
                    .eq(&candidate.conversation_id),
            )
            .exec(&txn)
            .await?;
        crate::entity::agent_run_event::Entity::delete_many()
            .filter(crate::entity::agent_run_event::Column::RunId.eq(&candidate.conversation_id))
            .exec(&txn)
            .await?;
        reservation::Entity::delete_many()
            .filter(reservation::Column::RootConversationId.eq(&candidate.conversation_id))
            .filter(reservation::Column::State.is_not_in(["reserved", "usage_unknown"]))
            .filter(
                reservation::Column::ReservationId
                    .not_in_subquery(crate::agent_approval_usage::pinned_reservation_ids()),
            )
            .filter(reservation::Column::CreatedAt.lt(cutoff_ms))
            .exec(&txn)
            .await?;
        txn.commit().await?;
    }
    *CURSOR.lock().map_err(|_| invalid())? = next;
    Ok(())
}

/// Unknown usage may retain budget anchors, but cannot retain chat/report bodies
/// indefinitely. Original native authority and receipt ledgers remain untouched.
pub(crate) async fn redact_deleted_content(
    db: &DatabaseConnection,
    cutoff_ms: i64,
) -> Result<u64, DbErr> {
    let live = || {
        session_row::Entity::find()
            .select_only()
            .column(session_row::Column::ConversationId)
            .into_query()
    };
    let expired_roots = || {
        tombstone::Entity::find()
            .select_only()
            .column(tombstone::Column::ConversationId)
            .filter(tombstone::Column::CreatedAtUnixMs.lt(cutoff_ms))
            .into_query()
    };
    let candidates = group_row::Entity::find()
        .filter(group_row::Column::SourceAdmission.eq("closed"))
        .filter(group_row::Column::RootConversationId.in_subquery(expired_roots()))
        .filter(group_row::Column::ContentRedactedAtMs.is_null())
        .filter(group_row::Column::RootConversationId.not_in_subquery(live()))
        .order_by_asc(group_row::Column::Id)
        .limit(100)
        .all(db)
        .await?;
    let mut redacted = 0;
    for candidate in candidates {
        let txn = crate::db::begin_write(db, session_row::Entity).await?;

        let row = group_row::Entity::find_by_id(candidate.id)
            .filter(group_row::Column::SourceAdmission.eq("closed"))
            .filter(group_row::Column::RootConversationId.in_subquery(expired_roots()))
            .filter(group_row::Column::ContentRedactedAtMs.is_null())
            .filter(group_row::Column::RootConversationId.not_in_subquery(live()))
            .one(&txn)
            .await?;
        let Some(row) = row else {
            txn.commit().await?;
            continue;
        };
        let group = decode_group(&row)?;
        let tasks = run_row::Entity::find()
            .filter(run_row::Column::GroupId.eq(&group.group_id))
            .order_by_asc(run_row::Column::Id)
            .all(&txn)
            .await?;
        if tasks
            .iter()
            .any(|task| !matches!(task.state.as_str(), "completed" | "failed" | "cancelled"))
        {
            txn.commit().await?;
            continue;
        }
        for task in tasks {
            let mut run = decode_run(&task)?;
            run.name = "Content expired".into();
            run.binding.objective = "Task content expired under conversation retention".into();
            run.binding.acceptance_criteria = vec!["Original criteria expired".into()];
            run.partial_report = None;
            if let Some(report) = &mut run.terminal_report {
                // Preserve the persisted assessment, never invent an outcome.
                report.summary = "Report content expired under conversation retention".into();
                report.findings.clear();
                report.delivered.clear();
                report.remaining.clear();
                report.reason = (report.assessment
                    != desk_diagnose_core::subagent::TaskAssessment::Complete)
                    .then(|| "Report content expired".into());
            }
            run.state_revision = run.state_revision.checked_add(1).ok_or_else(invalid)?;
            run.validate().map_err(|error| {
                DbErr::Custom(format!("invalid redacted delegated task: {error}"))
            })?;
            let child = session_row::Entity::find()
                .filter(session_row::Column::ConversationId.eq(&run.child_conversation_id))
                .filter(session_row::Column::ActorId.eq(&run.actor_id))
                .filter(session_row::Column::DeviceId.eq(&run.device_id))
                .one(&txn)
                .await?;
            if let Some(child) = child {
                let mut session =
                    PersistedAgentSession::decode_json(&child.state_json).map_err(|_| invalid())?;
                if session.turn_state.is_active() || session.agent_role.binding().is_none() {
                    return Err(invalid());
                }
                session.agent_role = desk_diagnose_core::subagent::AgentRole::SubAgent {
                    binding: Box::new(run.binding.clone()),
                };
                session.conversation.clear();
                session.ui_references = Default::default();
                session.thinking_prefix = Default::default();
                session.latest_model_context = None;
                session.delegated_owner_requirement = None;
                session.model_context_state = Default::default();
                session.context_usage_basis = None;
                session.cache_projection = None;
                session.context_notices.clear();
                session.context_attachments.clear();
                session.visual_evidence.clear();
                session.focus_epoch.selected_attachment_ids.clear();
                session.capability_disclosure = Default::default();
                session
                    .capability_disclosure
                    .reset_for_input(session.input_revision);
                session.pending_visual_verification = None;
                session.task_status_projection = None;
                session.version = child.version.checked_add(1).ok_or_else(invalid)?;
                let encoded = session.encode_json_for_storage().map_err(|_| invalid())?;
                PersistedAgentSession::decode_json(&encoded).map_err(|error| {
                    DbErr::Custom(format!("invalid redacted child session: {error:?}"))
                })?;
                let updated = session_row::Entity::update_many()
                    .set(session_row::ActiveModel {
                        state_json: Set(encoded),
                        version: Set(session.version),
                        ..Default::default()
                    })
                    .filter(session_row::Column::Id.eq(child.id))
                    .filter(session_row::Column::Version.eq(child.version))
                    .filter(session_row::Column::LeaseToken.eq(child.lease_token))
                    .exec(&txn)
                    .await?;
                if updated.rows_affected != 1 {
                    return Err(invalid());
                }
            }
            let changed = run_row::Entity::update_many()
                .set(run_row::ActiveModel {
                    state_json: Set(serde_json::to_string(&run).map_err(|_| invalid())?),
                    state_revision: Set(i64::try_from(run.state_revision).map_err(|_| invalid())?),
                    creation_envelope_json: Set(String::new()),
                    result_envelope_json: Set(None),
                    ..Default::default()
                })
                .filter(run_row::Column::Id.eq(task.id))
                .filter(run_row::Column::StateRevision.eq(task.state_revision))
                .exec(&txn)
                .await?;
            if changed.rows_affected != 1 {
                return Err(invalid());
            }
        }
        crate::entity::agent_subagent_inbox::Entity::delete_many()
            .filter(crate::entity::agent_subagent_inbox::Column::GroupId.eq(&group.group_id))
            .exec(&txn)
            .await?;
        // Control receipts contain task text; cost reservations contain only IDs,
        // digests and counters and remain available for immutable late settlement.
        reservation::Entity::delete_many()
            .filter(reservation::Column::GroupId.eq(&group.group_id))
            .filter(reservation::Column::State.is_not_in(["reserved", "usage_unknown", "settled"]))
            .exec(&txn)
            .await?;
        let changed = group_row::Entity::update_many()
            .set(group_row::ActiveModel {
                creation_envelope_json: Set(String::new()),
                content_redacted_at_ms: Set(Some(chrono::Utc::now().timestamp_millis())),
                ..Default::default()
            })
            .filter(group_row::Column::Id.eq(row.id))
            .filter(group_row::Column::Version.eq(row.version))
            .filter(group_row::Column::ContentRedactedAtMs.is_null())
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(invalid());
        }
        txn.commit().await?;
        redacted += 1;
    }
    Ok(redacted)
}
