use super::*;
use crate::entity::agent_issue_reply as reply;
use desk_diagnose_core::issue_events::CommentDisposition;

pub fn decode_comment(row: &event::Model) -> Result<IssueSourceComment, AssistantStoreError> {
    let json = row
        .snapshot_json
        .as_deref()
        .filter(|_| row.retired_at_ms.is_none())
        .ok_or(AssistantStoreError::Inaccessible)?;
    let comment: IssueSourceComment = serde_json::from_str(json).map_err(|_| invalid())?;
    comment.validate().map_err(AssistantStoreError::Invalid)?;
    let policy = original_policy(row)?;
    if comment.event_id() != row.event_id
        || signed(comment.repository.repository_id)? != row.repository_id
        || signed(comment.comment_id)? != row.comment_id
        || comment.issue_number.map(signed).transpose()? != row.issue_number
        || comment.sender_id.map(signed).transpose()? != row.sender_id
        || comment.automation != row.automation
        || comment.created_at_ms != row.source_created_at_ms
        || comment.updated_at_ms != row.source_updated_at_ms
        || event_key(&row.subscription_id, &row.event_id)? != row.event_key
        || comment.repository != policy.repository
        || comment.disposition(&policy, 0, false) != CommentDisposition::Pending
    {
        return Err(invalid());
    }
    Ok(comment)
}

pub fn original_policy(row: &event::Model) -> Result<IssueSubscription, AssistantStoreError> {
    let json = row
        .admitted_policy_json
        .as_deref()
        .filter(|_| row.retired_at_ms.is_none())
        .ok_or(AssistantStoreError::Inaccessible)?;
    let policy: IssueSubscription = serde_json::from_str(json).map_err(|_| invalid())?;
    policy.validate().map_err(AssistantStoreError::Invalid)?;
    if policy.actor_id != row.actor_id
        || policy.conversation_id != row.conversation_id
        || policy.subscription_id != row.subscription_id
        || signed(policy.revision)? != row.subscription_revision
        || !policy.enabled
    {
        return Err(invalid());
    }
    Ok(policy)
}

pub(super) async fn observe_on(
    db: &impl ConnectionTrait,
    claim: &SourceClaim,
    comment: &IssueSourceComment,
    now: i64,
) -> Result<(), AssistantStoreError> {
    let event_id = comment.event_id();
    let key = event_key(&claim.policy.subscription_id, &event_id)?;
    let hash = comment.fingerprint();
    if let Some(row) = event::Entity::find()
        .filter(event::Column::EventKey.eq(&key))
        .one(db)
        .await?
    {
        if row.actor_id != claim.policy.actor_id
            || row.subscription_id != claim.policy.subscription_id
            || row.event_id != event_id
        {
            return Err(invalid());
        }
        if row.retired_at_ms.is_none() && row.state != "ignored" {
            decode_comment(&row)?;
        }
        if row.last_observation_sha256 != hash {
            // The original body, admission disposition and request never change.
            event::Entity::update_many()
                .filter(event::Column::Id.eq(row.id))
                .filter(event::Column::LastObservationSha256.eq(row.last_observation_sha256))
                .set(event::ActiveModel {
                    last_observation_sha256: Set(hash),
                    edited_at_ms: Set(row.edited_at_ms.or(Some(now))),
                    updated_at_ms: Set(now),
                    ..Default::default()
                })
                .exec(db)
                .await?;
        }
        return Ok(());
    }
    let echo = is_outgoing_echo_on(db, &claim.policy.actor_id, comment).await?;
    let disposition = comment.disposition(&claim.policy, claim.row.first_enabled_at_ms, echo);
    let reason = serde_json::to_value(disposition)
        .map_err(|_| invalid())?
        .as_str()
        .ok_or_else(invalid)?
        .to_owned();
    let json = serde_json::to_string(comment).map_err(|_| invalid())?;
    let policy_json = (disposition == CommentDisposition::Pending)
        .then(|| serde_json::to_string(&claim.policy))
        .transpose()
        .map_err(|_| invalid())?;
    event::Entity::insert(event::ActiveModel {
        event_key: Set(key),
        actor_id: Set(claim.policy.actor_id.clone()),
        subscription_id: Set(claim.policy.subscription_id.clone()),
        subscription_revision: Set(signed(claim.policy.revision)?),
        admitted_policy_json: Set(policy_json),
        conversation_id: Set(claim.policy.conversation_id.clone()),
        event_id: Set(event_id),
        repository_id: Set(signed(comment.repository.repository_id)?),
        comment_id: Set(signed(comment.comment_id)?),
        issue_number: Set(comment.issue_number.map(signed).transpose()?),
        sender_id: Set(comment.sender_id.map(signed).transpose()?),
        automation: Set(comment.automation),
        source_created_at_ms: Set(comment.created_at_ms),
        source_updated_at_ms: Set(comment.updated_at_ms),
        snapshot_json: Set((disposition == CommentDisposition::Pending).then_some(json)),
        last_observation_sha256: Set(hash),
        state: Set(if disposition == CommentDisposition::Pending {
            "pending"
        } else {
            "ignored"
        }
        .into()),
        reason: Set(reason),
        created_at_ms: Set(now),
        updated_at_ms: Set(now),
        ..Default::default()
    })
    .exec_without_returning(db)
    .await?;
    Ok(())
}

/// A native remote ID or the exact random marker/body acknowledges only the
/// original dispatched issue/account, including writes whose response was lost.
pub async fn is_outgoing_echo_on(
    db: &impl ConnectionTrait,
    actor: &str,
    comment: &IssueSourceComment,
) -> Result<bool, AssistantStoreError> {
    if comment.sender_id != Some(comment.repository.account_id) {
        return Ok(false);
    }
    let marker = comment
        .body
        .rsplit_once("\n\n<!-- assistant-reply:")
        .and_then(|(_, tail)| tail.strip_suffix(" -->"))
        .filter(|marker| marker.len() == 32 && marker.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let mut identity =
        Condition::any().add(reply::Column::CommentId.eq(signed(comment.comment_id)?));
    if let Some(marker) = marker {
        identity =
            identity.add(reply::Column::ReceiptJson.contains(format!("\"marker\":\"{marker}\"")));
    }
    let rows = reply::Entity::find()
        .filter(reply::Column::ActorId.eq(actor))
        .filter(reply::Column::DispatchedAtMs.is_not_null())
        .filter(identity)
        .all(db)
        .await?;
    for row in rows {
        let key = crate::issue_reply::receipt_key(&row)?;
        let target = &key.access.target;
        let repo = &comment.repository;
        if key.access.connection_ref == repo.connection_ref
            && target.platform == repo.platform
            && target.instance == repo.instance
            && target.account_id == repo.account_id
            && target.repository_id == repo.repository_id
            && target.owner == repo.owner
            && target.repository == repo.repository
            && target.private == repo.private
            && target.internal == repo.internal
            && Some(target.issue_number) == comment.issue_number
            && (row.comment_id == Some(signed(comment.comment_id)?)
                || key.matches_receipt(repo.account_id, &comment.body))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn event_key(subscription: &str, event: &str) -> Result<String, AssistantStoreError> {
    Ok(digest(
        &serde_json::to_string(&(subscription, event)).map_err(|_| invalid())?,
    ))
}
