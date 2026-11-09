//! Active comments never enter native input history. A physical model receipt
//! consumes exactly the frozen material it actually received.

use super::*;
use crate::issue_authority;
use desk_agent_protocol::data_lineage::{DataEnvelope, DestinationIdentity};
use desk_diagnose_core::{
    chat::ChatMessage,
    issue_authority::{IssueAdmission, IssueCommentSnapshot, is_issue_supplement_label},
    session::PersistedAgentSession,
};

const COMMENTS_PER_PROJECTION: u64 = 16;

pub async fn model_material_on(
    db: &impl ConnectionTrait,
    session: &PersistedAgentSession,
    destination: &DestinationIdentity,
    now: i64,
) -> Result<Vec<ChatMessage>, AssistantStoreError> {
    let Some(admission) = issue_authority::authorized_session_on(db, session, false, now).await?
    else {
        return Ok(vec![]);
    };
    let current = issue_authority::current(
        db,
        &session.actor_id,
        &admission.subscription.subscription_id,
    )
    .await?
    .ok_or(AssistantStoreError::Inaccessible)?;
    let rows = scoped(&admission)
        .filter(event::Column::State.eq("attached"))
        .order_by_asc(event::Column::Id)
        .limit(COMMENTS_PER_PROJECTION)
        .all(db)
        .await?;
    let mut material = Vec::new();
    let mut bytes: usize = 0;
    for row in rows {
        let Some(comment) = authorized_comment(&row, &admission, &current)? else {
            continue;
        };
        let message = admission
            .supplemental_material(&comment, destination)
            .map_err(|_| invalid())?;
        bytes = bytes.saturating_add(message.text.len());
        if !material.is_empty() && bytes > 64 * 1024 {
            break;
        }
        material.push(message);
    }
    Ok(material)
}

/// Called inside the physical dispatch and output acceptance transactions.
/// Only the acceptance transaction may advance a comment's consumption state.
pub async fn verify_model_material_on(
    db: &impl ConnectionTrait,
    session: &PersistedAgentSession,
    destination: &DestinationIdentity,
    inputs: &[DataEnvelope],
    now: i64,
    consume: bool,
) -> Result<(), AssistantStoreError> {
    let labels = inputs
        .iter()
        .filter(|input| is_issue_supplement_label(input))
        .collect::<Vec<_>>();
    if labels.is_empty() {
        return Ok(());
    }
    let admission = issue_authority::authorized_session_on(db, session, false, now)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    let current = issue_authority::current(
        db,
        &session.actor_id,
        &admission.subscription.subscription_id,
    )
    .await?
    .ok_or(AssistantStoreError::Inaccessible)?;
    for label in labels {
        let event_id = label
            .provenance
            .source_object_id
            .as_ref()
            .ok_or_else(invalid)?;
        let row = scoped(&admission)
            .filter(event::Column::EventId.eq(event_id))
            .filter(event::Column::State.is_in(["attached", "consumed"]))
            .one(db)
            .await?
            .ok_or(AssistantStoreError::Inaccessible)?;
        let comment = authorized_comment(&row, &admission, &current)?
            .ok_or(AssistantStoreError::Inaccessible)?;
        let expected = admission
            .supplemental_material(&comment, destination)
            .map_err(|_| invalid())?;
        if expected.data_envelope.as_ref() != Some(label) {
            return Err(AssistantStoreError::Inaccessible);
        }
        if consume
            && row.state == "attached"
            && event::Entity::update_many()
                .filter(event::Column::Id.eq(row.id))
                .filter(event::Column::RequestId.eq(&admission.request_id))
                .filter(event::Column::State.eq("attached"))
                .filter(event::Column::SnapshotJson.eq(row.snapshot_json.clone()))
                .set(event::ActiveModel {
                    state: Set("consumed".into()),
                    reason: Set("model_accepted_material".into()),
                    consumed_at_ms: Set(Some(now)),
                    updated_at_ms: Set(now),
                    ..Default::default()
                })
                .exec(db)
                .await?
                .rows_affected
                != 1
        {
            return Err(AssistantStoreError::Inaccessible);
        }
    }
    Ok(())
}

/// These are immutable comment snapshots from the same workset. Publication
/// includes only comments that reached an accepted model request in this round.
pub async fn include_on(
    db: &impl ConnectionTrait,
    admission: &mut IssueAdmission,
    current: &IssueSubscription,
    consumed_only: bool,
) -> Result<(), AssistantStoreError> {
    let rows = scoped(admission)
        .filter(if consumed_only {
            event::Column::State.eq("consumed")
        } else {
            event::Column::State.is_in(["attached", "consumed"])
        })
        .order_by_asc(event::Column::Id)
        .all(db)
        .await?;
    for row in rows {
        let Some(comment) = authorized_comment(&row, admission, current)? else {
            continue;
        };
        if admission
            .workset
            .comments
            .iter()
            .any(|existing| existing.id == comment.comment_id)
        {
            continue;
        }
        admission.workset.comments.push(IssueCommentSnapshot {
            id: comment.comment_id,
            author_id: comment.sender_id.ok_or_else(invalid)?,
            body: comment.body,
        });
        if admission.workset.validate(&admission.subscription).is_err() {
            admission.workset.comments.pop();
            break;
        }
    }
    Ok(())
}

/// A draft may already contain a consumed comment. If that author's scope is
/// withdrawn, the existing draft cannot safely be published under the new rule.
pub async fn publication_content_current(
    db: &impl ConnectionTrait,
    admission: &IssueAdmission,
    current: &IssueSubscription,
) -> Result<bool, AssistantStoreError> {
    if admission
        .workset
        .comments
        .iter()
        .any(|comment| !current.allows_event(&admission.issue, comment.author_id, false, false))
    {
        return Ok(false);
    }
    let authors = current
        .allowed_sender_ids
        .iter()
        .map(|id| signed(*id))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(scoped(admission)
        .select_only()
        .column(event::Column::Id)
        .filter(event::Column::State.eq("consumed"))
        .filter(
            Condition::any()
                .add(event::Column::SenderId.is_null())
                .add(event::Column::SenderId.is_not_in(authors))
                .add(event::Column::Automation.eq(true)),
        )
        .into_tuple::<i64>()
        .one(db)
        .await?
        .is_none())
}

fn scoped(admission: &IssueAdmission) -> sea_orm::Select<event::Entity> {
    event::Entity::find()
        .filter(event::Column::ActorId.eq(&admission.subscription.actor_id))
        .filter(event::Column::ConversationId.eq(&admission.subscription.conversation_id))
        .filter(event::Column::SubscriptionId.eq(&admission.subscription.subscription_id))
        .filter(event::Column::RequestId.eq(&admission.request_id))
        .filter(event::Column::RetiredAtMs.is_null())
}

fn authorized_comment(
    row: &event::Model,
    admission: &IssueAdmission,
    current: &IssueSubscription,
) -> Result<Option<IssueSourceComment>, AssistantStoreError> {
    let comment = decode_comment(row)?;
    let sender = comment.sender_id.ok_or_else(invalid)?;
    let target = &admission.issue.target;
    let work_id = desk_diagnose_core::issue_events::work_id(
        &row.actor_id,
        &row.subscription_id,
        target.repository_id,
        target.issue_id,
    );
    Ok((row.work_id.as_deref() == Some(work_id.as_str())
        && comment.issue_number == Some(target.issue_number)
        && comment.repository == admission.subscription.repository
        && admission.subscription.allows_event(
            &admission.issue,
            sender,
            comment.automation,
            false,
        )
        && current.allows_event(&admission.issue, sender, comment.automation, false))
    .then_some(comment))
}
