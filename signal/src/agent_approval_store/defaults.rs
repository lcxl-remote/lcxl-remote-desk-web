//! Discover default approval roots from native permission proposals.

use super::*;
use desk_diagnose_core::approval_review::pending_default_device_review;

pub(crate) async fn discover_default_review_roots(
    db: &DatabaseConnection,
    after_id: i64,
    limit: u64,
    now: u64,
) -> Result<(Vec<PendingPermissionReview>, Option<i64>), DbErr> {
    let reviewer_available = default_reviewer_available(db).await?;
    let mut unattended = Vec::new();
    let actor = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string();
    let page = limit.clamp(1, 64);
    let oldest = DateTime::<Utc>::from_timestamp_millis(as_i64(
        now.saturating_sub(desk_diagnose_core::approval_review::PERMISSION_REVIEW_MAX_AGE_MS),
    )?)
    .ok_or_else(invalid)?;
    let hints = agent_run_event::Entity::find()
        .select_only()
        .columns([
            agent_run_event::Column::Id,
            agent_run_event::Column::RunId,
            agent_run_event::Column::CorrelationId,
            agent_run_event::Column::PayloadJson,
        ])
        .filter(agent_run_event::Column::Kind.eq(AgentRunEventKind::PermissionRequested.as_str()))
        .filter(agent_run_event::Column::ActorId.eq(&actor))
        .filter(agent_run_event::Column::CreatedAt.gte(oldest))
        .filter(agent_run_event::Column::Id.gt(after_id))
        .order_by_asc(agent_run_event::Column::Id)
        .limit(page)
        .into_tuple::<(i64, String, Option<String>, String)>()
        .all(db)
        .await?;
    for (_, run, request, payload) in &hints {
        let Some(request) = request else { continue };
        let mode_revision = serde_json::from_str::<
            desk_diagnose_core::dynamic_run::PermissionRequestedEvent,
        >(payload)
        .ok()
        .and_then(|event| event.approval_mode_revision);
        if !reviewer_available && mode_revision.is_none() {
            continue;
        }
        if let Some(subject) = ensure_root(
            db,
            &actor,
            run,
            request,
            now,
            reviewer_available,
            mode_revision,
        )
        .await?
        {
            unattended.push(subject);
        }
    }
    let next = (hints.len() == page as usize)
        .then(|| hints.last().map(|row| row.0))
        .flatten();
    Ok((unattended, next))
}

async fn ensure_root(
    db: &DatabaseConnection,
    actor: &str,
    run: &str,
    request: &str,
    now: u64,
    reviewer_available: bool,
    mode_revision: Option<u64>,
) -> Result<Option<PendingPermissionReview>, DbErr> {
    let txn = crate::db::begin_write(db, agent_session::Entity).await?;
    let Some(row) = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(run))
        .filter(agent_session::Column::ActorId.eq(actor))
        .one(&txn)
        .await?
    else {
        return Ok(None);
    };
    let Ok(session) = PersistedAgentSession::decode_boxed_json(&row.state_json) else {
        return Ok(None);
    };
    let Some(request) = pending_default_device_review(&session, request, now) else {
        return Ok(None);
    };
    if session.conversation_id != run || session.check_subject(actor, &row.device_id).is_err() {
        return Ok(None);
    }
    let root = session.approval_delegation_conversation_id();
    if check_permission_target_on(&txn, &session, request)
        .await
        .is_err()
    {
        return Ok(None);
    }
    if let Some(revision) = mode_revision
        && preference_for_session_on(&txn, &session)
            .await?
            .and_then(|preference| preference.unattended_proposal_revision(&session, request))
            == Some(revision)
    {
        return Ok(Some(PendingPermissionReview {
            conversation_id: run.into(),
            owner_id: actor.into(),
            device_id: row.device_id,
            target_device_id: desk_diagnose_core::permission_grant::permission_device_id(
                &session, request,
            )
            .into(),
            request_id: request.request_id.clone(),
            kind: desk_diagnose_core::approval_review::PendingRequestKind::Permission,
        }));
    }
    if !reviewer_available
        || has_active_delegation(&txn, root, actor).await?
        || !crate::agent_subagent_store::shared_approval_current_on(&txn, &session, as_i64(now)?)
            .await?
    {
        return Ok(None);
    }
    let (row, mut root) = session_on(&txn, root, actor, &row.device_id).await?;
    if root.turn_state.is_active()
        || root.main_coordination.is_some()
        || crate::approval_model_provider::load(&txn)
            .await?
            .destination_identity()
            .is_err()
    {
        return Ok(None);
    }
    let delegation = ApprovalDelegation::new_default(
        format!("approval-delegation-{}", uuid::Uuid::new_v4()),
        root.conversation_id.clone(),
        actor.to_owned(),
        root.device_id.clone(),
        now,
        true,
    )
    .map_err(|_| invalid())?;
    project(&delegation)?.insert(&txn).await?;
    append_audit_on(
        &txn,
        &row,
        &mut root,
        &delegation,
        AgentRunEventKind::ApprovalDelegationOpened,
        delegation.issued_by.clone(),
        DateTime::<Utc>::from_timestamp_millis(as_i64(now)?).ok_or_else(invalid)?,
    )
    .await?;
    txn.commit().await?;
    Ok(None)
}

async fn has_active_delegation(
    txn: &DatabaseTransaction,
    root: &str,
    actor: &str,
) -> Result<bool, DbErr> {
    Ok(delegation_row::Entity::find()
        .select_only()
        .column(delegation_row::Column::Id)
        .filter(delegation_row::Column::ConversationId.eq(root))
        .filter(delegation_row::Column::ActorId.eq(actor))
        .filter(delegation_row::Column::Status.eq("active"))
        .into_tuple::<i64>()
        .one(txn)
        .await?
        .is_some())
}

/// Open the default AI review for a fresh-task proposal that waits in the
/// owner's own turn. The caller holds the session's version/lease fence in
/// `txn`; the audit event is saved into `session` before the proposal receipt.
/// Returns whether the session row changed.
pub(crate) async fn open_for_schedule_proposal_on(
    txn: &DatabaseTransaction,
    row: &agent_session::Model,
    session: &mut PersistedAgentSession,
    now: u64,
) -> Result<bool, DbErr> {
    let root = session.approval_delegation_conversation_id().to_owned();
    if root != session.conversation_id
        || !session.owner_direct_planning()
        || has_active_delegation(txn, &root, &session.actor_id).await?
        || crate::approval_model_provider::load(txn)
            .await?
            .destination_identity()
            .is_err()
    {
        return Ok(false);
    }
    let delegation = ApprovalDelegation::new_default(
        format!("approval-delegation-{}", uuid::Uuid::new_v4()),
        root,
        session.actor_id.clone(),
        session.device_id.clone(),
        now,
        true,
    )
    .map_err(|_| invalid())?;
    project(&delegation)?.insert(txn).await?;
    append_audit_on(
        txn,
        row,
        session,
        &delegation,
        AgentRunEventKind::ApprovalDelegationOpened,
        delegation.issued_by.clone(),
        DateTime::<Utc>::from_timestamp_millis(as_i64(now)?).ok_or_else(invalid)?,
    )
    .await?;
    Ok(true)
}
