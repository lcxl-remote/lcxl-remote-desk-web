//! Subscription settings and content-free source status for the owner's workbench.

use super::*;
use crate::entity::{agent_issue_event as event, agent_issue_source as source};
use desk_agent_protocol::{
    ai_assistant::issue_subscription::{
        IssueSubscriptionIntake, IssueSubscriptionSave, IssueSubscriptionView,
    },
    external_tools::valid_component,
};
use desk_diagnose_core::{
    issue_authority::{IssueFilter, IssueRepositoryScope},
    native_ids::{self, Kind},
};
use sea_orm::{QueryOrder, QuerySelect};

mod changes;
pub use changes::AffectedWork;

pub fn validate_save(body: &IssueSubscriptionSave) -> Result<(), AssistantStoreError> {
    let automation: std::collections::BTreeSet<_> = body.automation_sender_ids.iter().collect();
    let has_valid_revision = match &body.subscription_id {
        Some(id) => native_ids::is(Kind::IssueSubscription, id) && body.expected_revision > 0,
        None => body.expected_revision == 0,
    };
    if !has_valid_revision
        || body.expected_revision >= i64::MAX as u64
        || !desk_diagnose_core::subagent::valid_id(&body.workbench_id)
        || !valid_component(&body.connection_ref)
        || !valid_component(&body.owner)
        || !valid_component(&body.repository)
        || body
            .issue_number
            .is_some_and(|number| number == 0 || number > i64::MAX as u64)
        || (body.enabled && body.allowed_sender_ids.is_empty())
        || body
            .allowed_sender_ids
            .iter()
            .chain(&body.automation_sender_ids)
            .any(|id| *id == 0 || *id > i64::MAX as u64)
        || body
            .allowed_sender_ids
            .iter()
            .any(|id| automation.contains(id))
    {
        return Err(AssistantStoreError::Invalid(
            "Invalid issue subscription settings",
        ));
    }
    Ok(())
}

pub async fn current_for_edit(
    db: &impl ConnectionTrait,
    actor: &str,
    id: &str,
    expected: u64,
) -> Result<IssueSubscription, AssistantStoreError> {
    let policy = current(db, actor, id)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if policy.revision != expected {
        return Err(AssistantStoreError::Stale {
            current_revision: policy.revision,
        });
    }
    Ok(policy)
}

pub enum WriteResult {
    Saved {
        view: IssueSubscriptionView,
        affected: Vec<AffectedWork>,
    },
    Overlap(IssueSubscriptionView),
}

pub async fn put_on(
    db: &impl ConnectionTrait,
    policy: IssueSubscription,
    now: i64,
) -> Result<WriteResult, AssistantStoreError> {
    let previous = current(db, &policy.actor_id, &policy.subscription_id).await?;
    match put(db, policy.clone(), policy.revision, now).await {
        Ok(saved) => {
            let affected = if let Some(previous) = previous {
                changes::record_on(db, &previous, &saved, now).await?
            } else {
                Vec::new()
            };
            Ok(WriteResult::Saved {
                view: view_on(db, saved).await?,
                affected,
            })
        }
        Err(AssistantStoreError::Invalid(OVERLAPPING)) => {
            let existing = overlapping_on(db, &policy).await?.ok_or_else(invalid)?;
            Ok(WriteResult::Overlap(view_on(db, existing).await?))
        }
        Err(error) => Err(error),
    }
}

pub fn candidate(
    actor: String,
    id: String,
    body: IssueSubscriptionSave,
    repository: IssueRepositoryScope,
    issue_filter: Option<IssueFilter>,
) -> IssueSubscription {
    IssueSubscription {
        subscription_id: id,
        revision: body.expected_revision,
        actor_id: actor,
        conversation_id: body.workbench_id,
        enabled: body.enabled,
        repository,
        issue_filter,
        allowed_sender_ids: body.allowed_sender_ids,
        automation_sender_ids: body.automation_sender_ids,
        work_kind: body.work_kind,
        auto_reply: body.auto_reply,
    }
}

pub async fn list_on(
    db: &impl ConnectionTrait,
    actor: &str,
    workbench: &str,
) -> Result<Vec<IssueSubscriptionView>, AssistantStoreError> {
    let rows = subscription::Entity::find()
        .filter(subscription::Column::ActorId.eq(actor))
        .filter(subscription::Column::ConversationId.eq(workbench))
        .order_by_asc(subscription::Column::Id)
        .all(db)
        .await?;
    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        items.push(view_on(db, decode_subscription(&row)?).await?);
    }
    Ok(items)
}

pub async fn view_on(
    db: &impl ConnectionTrait,
    policy: IssueSubscription,
) -> Result<IssueSubscriptionView, AssistantStoreError> {
    let source = source::Entity::find()
        .filter(source::Column::ActorId.eq(&policy.actor_id))
        .filter(source::Column::ConversationId.eq(&policy.conversation_id))
        .filter(source::Column::SubscriptionId.eq(&policy.subscription_id))
        .one(db)
        .await?;
    let mut latest_intake = if let Some(source) = &source {
        event::Entity::find()
            .select_only()
            .columns([
                event::Column::CommentId,
                event::Column::IssueNumber,
                event::Column::SenderId,
                event::Column::CreatedAtMs,
                event::Column::SubscriptionRevision,
                event::Column::State,
                event::Column::Reason,
                event::Column::RequestId,
            ])
            .filter(event::Column::ActorId.eq(&policy.actor_id))
            .filter(event::Column::ConversationId.eq(&policy.conversation_id))
            .filter(event::Column::SubscriptionId.eq(&policy.subscription_id))
            .filter(event::Column::CreatedAtMs.gte(source.first_enabled_at_ms))
            .filter(event::Column::RetiredAtMs.is_null())
            .order_by_desc(event::Column::Id)
            .into_tuple::<(
                i64,
                Option<i64>,
                Option<i64>,
                i64,
                i64,
                String,
                String,
                Option<String>,
            )>()
            .one(db)
            .await?
            .map(
                |(
                    comment_id,
                    issue_number,
                    sender_id,
                    observed_at_ms,
                    subscription_revision,
                    state,
                    reason,
                    request_id,
                )| {
                    IssueSubscriptionIntake {
                        comment_id,
                        issue_number,
                        sender_id,
                        observed_at_ms,
                        subscription_revision,
                        state,
                        reason,
                        request_id,
                        authorization_change: None,
                    }
                },
            )
    } else {
        None
    };
    if let Some(intake) = &mut latest_intake
        && let Some(request) = &intake.request_id
    {
        intake.authorization_change =
            changes::read_on(db, &policy.actor_id, &policy.subscription_id, request).await?;
    }
    let (last_checked_at_ms, source_error) =
        source.map_or((None, None), |row| (row.last_checked_at_ms, row.last_error));
    Ok(IssueSubscriptionView {
        subscription_id: policy.subscription_id,
        revision: policy.revision,
        workbench_id: policy.conversation_id,
        connection_ref: policy.repository.connection_ref,
        platform: policy.repository.platform,
        owner: policy.repository.owner,
        repository: policy.repository.repository,
        issue_number: policy.issue_filter.map(|filter| filter.issue_number),
        allowed_sender_ids: policy.allowed_sender_ids,
        automation_sender_ids: policy.automation_sender_ids,
        work_kind: policy.work_kind,
        auto_reply: policy.auto_reply,
        enabled: policy.enabled,
        last_checked_at_ms,
        source_error,
        latest_intake,
    })
}
