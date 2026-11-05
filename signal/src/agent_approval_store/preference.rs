//! Owner preference updates do not replace the current turn snapshot.

use super::*;
use desk_agent_protocol::ai_assistant::approval_mode::{
    AssistantApprovalMode, UpdateAssistantApprovalMode,
};
use desk_diagnose_core::assistant_approval_mode::{
    AssistantApprovalPreference, preference_for_root,
};

/// A saved manual preference also stops already claimed AI review from granting an action.
pub(crate) async fn review_mode_current_on(
    db: &impl ConnectionTrait,
    session: &PersistedAgentSession,
) -> Result<bool, DbErr> {
    Ok(preference_for_session_on(db, session)
        .await?
        .is_some_and(|pref| pref.mode != AssistantApprovalMode::Manual))
}

pub(crate) async fn preference_for_session_on(
    db: &impl ConnectionTrait,
    session: &PersistedAgentSession,
) -> Result<Option<AssistantApprovalPreference>, DbErr> {
    let raw = agent_session::Entity::find()
        .select_only()
        .column(agent_session::Column::ApprovalPreferenceJson)
        .filter(agent_session::Column::ActorId.eq(&session.actor_id))
        .filter(agent_session::Column::DeviceId.eq(&session.device_id))
        .filter(
            agent_session::Column::ConversationId.eq(session.approval_delegation_conversation_id()),
        )
        .into_tuple::<Option<String>>()
        .one(db)
        .await?;
    Ok(raw.and_then(|raw| AssistantApprovalPreference::decode(raw.as_deref())))
}

pub(crate) async fn current_no_review_parent_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    grant: &desk_agent_protocol::capability_grant::CapabilityGrant,
) -> Result<desk_agent_protocol::capability_grant::NoReviewGrantProvenance, DbErr> {
    use desk_agent_protocol::capability_grant::CapabilityGrantIssuer;

    let CapabilityGrantIssuer::OwnerNoReview(parent) = &grant.issued_by else {
        return Err(invalid());
    };
    if crate::model_provider::load(txn).await?.execution_mode
        != desk_agent_protocol::ExecutionMode::Automated
    {
        return Err(invalid());
    }
    let preference = preference_for_session_on(txn, session)
        .await?
        .ok_or_else(invalid)?;
    let row = agent_run_event::Entity::find()
        .filter(agent_run_event::Column::EventId.eq(&parent.decision_event_id))
        .filter(agent_run_event::Column::RunId.eq(&grant.run_id))
        .filter(agent_run_event::Column::Kind.eq(AgentRunEventKind::PermissionDecided.as_str()))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let decision = serde_json::from_str(&row.payload_json).map_err(|_| invalid())?;
    if !preference.matches_unattended_grant(session, &decision, grant) {
        return Err(invalid());
    }
    Ok(parent.clone())
}

async fn root_on(
    db: &impl ConnectionTrait,
    actor: &str,
    conversation: &str,
) -> Result<(agent_session::Model, AssistantApprovalPreference), DbErr> {
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ActorId.eq(actor))
        .filter(agent_session::Column::ConversationId.eq(conversation))
        .one(db)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound("Assistant approval root".into()))?;
    let preference = preference_for_root(
        &row.state_json,
        conversation,
        actor,
        &row.device_id,
        row.approval_preference_json.as_deref(),
    )
    .ok_or_else(|| DbErr::RecordNotFound("Assistant approval root".into()))?;
    Ok((row, preference))
}

pub async fn load_preference(
    db: &DatabaseConnection,
    actor: &str,
    conversation: &str,
) -> Result<AssistantApprovalPreference, DbErr> {
    root_on(db, actor, conversation).await.map(|(_, mode)| mode)
}

pub async fn save_preference(
    db: &DatabaseConnection,
    actor: &str,
    change: UpdateAssistantApprovalMode,
) -> Result<Option<AssistantApprovalPreference>, DbErr> {
    let txn = crate::db::begin_write(db, agent_session::Entity).await?;
    let (row, mut preference) = root_on(&txn, actor, &change.session).await?;
    if !preference.update(change) {
        return Ok(None);
    }
    write_on(&txn, row.id, &preference).await?;
    txn.commit().await?;
    Ok(Some(preference))
}

/// Existing explicit enable/disable actions must update the same root preference.
pub(super) async fn override_mode_on(
    txn: &DatabaseTransaction,
    row: &agent_session::Model,
    mode: AssistantApprovalMode,
) -> Result<(), DbErr> {
    let mut preference =
        AssistantApprovalPreference::decode(row.approval_preference_json.as_deref())
            .ok_or_else(invalid)?;
    if preference.mode == mode {
        return Ok(());
    }
    if !preference.update(UpdateAssistantApprovalMode {
        session: row.conversation_id.clone(),
        expected_revision: preference.revision,
        mode,
        no_review_confirmation_id: None,
    }) {
        return Err(invalid());
    }
    write_on(txn, row.id, &preference).await
}

async fn write_on(
    txn: &DatabaseTransaction,
    row_id: i64,
    preference: &AssistantApprovalPreference,
) -> Result<(), DbErr> {
    let json = serde_json::to_string(preference).map_err(|_| invalid())?;
    let changed = agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::ApprovalPreferenceJson,
            Expr::value(json),
        )
        .filter(agent_session::Column::Id.eq(row_id))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    Ok(())
}
