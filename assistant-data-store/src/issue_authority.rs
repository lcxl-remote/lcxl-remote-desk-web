//! Canonical source policy and accepted facts. Callers own the transaction and
//! the owner lock shared with model admission and final publication dispatch.

use crate::{
    AssistantStoreError,
    entity::{agent_issue_admission as admission, agent_issue_subscription as subscription},
};
use desk_diagnose_core::{
    external_tools::IssueAccess,
    issue_authority::{IssueAdmission, IssueSourceRef, IssueSubscription},
    issue_reply::ReplyOrigin,
};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set, sea_query::OnConflict};
use sha2::{Digest, Sha256};

const MAX_POLICY_BYTES: usize = 64 * 1024;
const OVERLAPPING: &str = "overlapping issue subscription";

pub mod settings;

pub fn decode_subscription(
    row: &subscription::Model,
) -> Result<IssueSubscription, AssistantStoreError> {
    if digest(&row.policy_json) != row.policy_sha256 {
        return Err(invalid());
    }
    let policy: IssueSubscription =
        serde_json::from_str(&row.policy_json).map_err(|_| invalid())?;
    policy.validate().map_err(AssistantStoreError::Invalid)?;
    if policy.subscription_id != row.subscription_id
        || policy.actor_id != row.actor_id
        || policy.conversation_id != row.conversation_id
        || i64::try_from(policy.revision).ok() != Some(row.revision)
        || policy.enabled != row.enabled
    {
        return Err(invalid());
    }
    Ok(policy)
}

pub async fn current(
    db: &impl ConnectionTrait,
    actor: &str,
    id: &str,
) -> Result<Option<IssueSubscription>, AssistantStoreError> {
    subscription::Entity::find()
        .filter(subscription::Column::ActorId.eq(actor))
        .filter(subscription::Column::SubscriptionId.eq(id))
        .one(db)
        .await?
        .map(|row| decode_subscription(&row))
        .transpose()
}

/// Stamp the next revision under the original owner lock. Overlap checks and
/// the CAS must commit together; callers never write a second authority copy.
pub async fn put(
    db: &impl ConnectionTrait,
    mut policy: IssueSubscription,
    expected: u64,
    now: i64,
) -> Result<IssueSubscription, AssistantStoreError> {
    policy.revision = expected
        .checked_add(1)
        .filter(|value| *value <= i64::MAX as u64)
        .ok_or_else(invalid)?;
    policy.canonicalize();
    policy.validate().map_err(AssistantStoreError::Invalid)?;
    let json = serde_json::to_string(&policy).map_err(|_| invalid())?;
    if json.len() > MAX_POLICY_BYTES {
        return Err(AssistantStoreError::Capacity);
    }
    let original = subscription::Entity::find()
        .filter(subscription::Column::SubscriptionId.eq(&policy.subscription_id))
        .one(db)
        .await?;
    match &original {
        Some(row) if row.actor_id != policy.actor_id => {
            return Err(AssistantStoreError::Inaccessible);
        }
        Some(row) if u64::try_from(row.revision).ok() != Some(expected) => {
            return Err(AssistantStoreError::Stale {
                current_revision: u64::try_from(row.revision).map_err(|_| invalid())?,
            });
        }
        None if expected != 0 => return Err(AssistantStoreError::Inaccessible),
        _ => {}
    }
    let previous = original.as_ref().map(decode_subscription).transpose()?;
    if overlapping_on(db, &policy).await?.is_some() {
        return Err(AssistantStoreError::Invalid(OVERLAPPING));
    }
    let changed = if let Some(row) = original {
        subscription::Entity::update_many()
            .filter(subscription::Column::Id.eq(row.id))
            .filter(subscription::Column::ActorId.eq(&policy.actor_id))
            .filter(subscription::Column::Revision.eq(row.revision))
            .filter(subscription::Column::PolicyJson.eq(row.policy_json))
            .filter(subscription::Column::PolicySha256.eq(row.policy_sha256))
            .set(subscription::ActiveModel {
                conversation_id: Set(policy.conversation_id.clone()),
                revision: Set(policy.revision as i64),
                enabled: Set(policy.enabled),
                policy_sha256: Set(digest(&json)),
                policy_json: Set(json),
                updated_at_ms: Set(now),
                ..Default::default()
            })
            .exec(db)
            .await?
            .rows_affected
            == 1
    } else {
        subscription::Entity::insert(subscription::ActiveModel {
            subscription_id: Set(policy.subscription_id.clone()),
            actor_id: Set(policy.actor_id.clone()),
            conversation_id: Set(policy.conversation_id.clone()),
            revision: Set(policy.revision as i64),
            enabled: Set(policy.enabled),
            policy_sha256: Set(digest(&json)),
            policy_json: Set(json),
            created_at_ms: Set(now),
            updated_at_ms: Set(now),
            ..Default::default()
        })
        .on_conflict(
            OnConflict::column(subscription::Column::SubscriptionId)
                .do_nothing()
                .to_owned(),
        )
        .exec_without_returning(db)
        .await?
            == 1
    };
    if !changed {
        return Err(AssistantStoreError::Stale {
            current_revision: current(db, &policy.actor_id, &policy.subscription_id)
                .await?
                .ok_or_else(invalid)?
                .revision,
        });
    }
    crate::issue_events::policy_changed_on(db, previous.as_ref(), &policy, now).await?;
    Ok(policy)
}

async fn overlapping_on(
    db: &impl ConnectionTrait,
    policy: &IssueSubscription,
) -> Result<Option<IssueSubscription>, AssistantStoreError> {
    if !policy.enabled {
        return Ok(None);
    }
    let others = subscription::Entity::find()
        .filter(subscription::Column::ActorId.eq(&policy.actor_id))
        .filter(subscription::Column::ConversationId.eq(&policy.conversation_id))
        .filter(subscription::Column::Enabled.eq(true))
        .filter(subscription::Column::SubscriptionId.ne(&policy.subscription_id))
        .all(db)
        .await?;
    for row in others {
        let existing = decode_subscription(&row)?;
        if policy.overlaps(&existing) {
            return Ok(Some(existing));
        }
    }
    Ok(None)
}

pub fn decode_admission(row: &admission::Model) -> Result<IssueAdmission, AssistantStoreError> {
    let json = row
        .snapshot_json
        .as_deref()
        .filter(|_| row.retired_at_ms.is_none())
        .ok_or(AssistantStoreError::Inaccessible)?;
    if digest(json) != row.snapshot_sha256 {
        return Err(invalid());
    }
    let snapshot: IssueAdmission = serde_json::from_str(json).map_err(|_| invalid())?;
    snapshot.validate().map_err(AssistantStoreError::Invalid)?;
    if snapshot.authorization_id != row.authorization_id
        || snapshot.subscription.actor_id != row.actor_id
        || snapshot.subscription.subscription_id != row.subscription_id
        || snapshot.subscription.conversation_id != row.conversation_id
        || snapshot.request_id != row.request_id
        || snapshot.execution_root_id != row.execution_root_id
        || snapshot.expires_at_ms != row.content_expires_at_ms
        || snapshot.event_id != row.event_id
        || i64::try_from(snapshot.subscription.revision).ok() != Some(row.subscription_revision)
    {
        return Err(invalid());
    }
    Ok(snapshot)
}

/// Only trusted event intake calls this after freezing the actual remote event.
/// A replay preserves its first scope, request, deadline and stopped state.
pub async fn admit(
    db: &impl ConnectionTrait,
    snapshot: &IssueAdmission,
) -> Result<admission::Model, AssistantStoreError> {
    snapshot.validate().map_err(AssistantStoreError::Invalid)?;
    if let Some(existing) = admission::Entity::find()
        .filter(admission::Column::AuthorizationId.eq(&snapshot.authorization_id))
        .one(db)
        .await?
    {
        if decode_admission(&existing)? != *snapshot {
            return Err(invalid());
        }
        return Ok(existing);
    }
    let policy = current(
        db,
        &snapshot.subscription.actor_id,
        &snapshot.subscription.subscription_id,
    )
    .await?
    .ok_or(AssistantStoreError::Inaccessible)?;
    if policy != snapshot.subscription {
        return Err(AssistantStoreError::Inaccessible);
    }
    insert_admission_on(db, snapshot).await
}

// A persisted source observation may have been accepted under an earlier
// revision. The intake transaction proves its frozen original rule and the
// intersection with the current rule before calling this writer.
pub(crate) async fn insert_admission_on(
    db: &impl ConnectionTrait,
    snapshot: &IssueAdmission,
) -> Result<admission::Model, AssistantStoreError> {
    snapshot.validate().map_err(AssistantStoreError::Invalid)?;
    let json = serde_json::to_string(snapshot).map_err(|_| invalid())?;
    if json.len() > MAX_POLICY_BYTES + 1024 * 1024 {
        return Err(AssistantStoreError::Capacity);
    }
    let policy = &snapshot.subscription;
    admission::Entity::insert(admission::ActiveModel {
        authorization_id: Set(snapshot.authorization_id.clone()),
        actor_id: Set(policy.actor_id.clone()),
        subscription_id: Set(policy.subscription_id.clone()),
        subscription_revision: Set(policy.revision as i64),
        conversation_id: Set(policy.conversation_id.clone()),
        request_id: Set(snapshot.request_id.clone()),
        execution_root_id: Set(snapshot.execution_root_id.clone()),
        event_id: Set(snapshot.event_id.clone()),
        snapshot_sha256: Set(digest(&json)),
        snapshot_json: Set(Some(json)),
        stopped_at_ms: Set(None),
        retired_at_ms: Set(None),
        content_expires_at_ms: Set(snapshot.expires_at_ms),
        created_at_ms: Set(snapshot.admitted_at_ms),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::column(admission::Column::AuthorizationId)
            .do_nothing()
            .to_owned(),
    )
    .exec_without_returning(db)
    .await?;
    let saved = admission::Entity::find()
        .filter(admission::Column::AuthorizationId.eq(&snapshot.authorization_id))
        .one(db)
        .await?
        .ok_or_else(invalid)?;
    if decode_admission(&saved)? != *snapshot {
        return Err(invalid());
    }
    Ok(saved)
}

pub async fn resolve(
    db: &impl ConnectionTrait,
    actor: &str,
    reference: &IssueSourceRef,
    issue: &IssueAccess,
    now: i64,
) -> Result<Option<(IssueAdmission, IssueSubscription)>, AssistantStoreError> {
    Ok(resolve_reference(db, actor, reference, now)
        .await?
        .filter(|(snapshot, _)| snapshot.issue == *issue))
}

async fn resolve_reference(
    db: &impl ConnectionTrait,
    actor: &str,
    reference: &IssueSourceRef,
    now: i64,
) -> Result<Option<(IssueAdmission, IssueSubscription)>, AssistantStoreError> {
    Ok(reference_scope_on(db, actor, reference)
        .await?
        .filter(|(snapshot, policy)| snapshot.usable(policy, now)))
}

pub(crate) async fn reference_scope_on(
    db: &impl ConnectionTrait,
    actor: &str,
    reference: &IssueSourceRef,
) -> Result<Option<(IssueAdmission, IssueSubscription)>, AssistantStoreError> {
    let Some(row) = admission::Entity::find()
        .filter(admission::Column::ActorId.eq(actor))
        .filter(admission::Column::AuthorizationId.eq(&reference.authorization_id))
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    if row.stopped_at_ms.is_some() || row.retired_at_ms.is_some() {
        return Ok(None);
    }
    let snapshot = decode_admission(&row)?;
    if snapshot.subscription.revision != reference.subscription_revision
        || reference.tool_ref != snapshot.issue.tool_ref()
    {
        return Ok(None);
    }
    let Some(policy) = current(db, actor, &snapshot.subscription.subscription_id).await? else {
        return Ok(None);
    };
    Ok(Some((snapshot, policy)))
}

/// Match the complete original logical work, in addition to the current rule.
pub async fn resolve_work_source(
    db: &impl ConnectionTrait,
    actor: &str,
    source: &desk_diagnose_core::issue_authority::IssueWorkSource,
    issue: &IssueAccess,
    now: i64,
) -> Result<Option<(IssueAdmission, IssueSubscription)>, AssistantStoreError> {
    let Some((admitted, current)) = resolve(db, actor, &source.reference, issue, now).await? else {
        return Ok(None);
    };
    let original = desk_diagnose_core::issue_authority::IssueWorkSource::from_admission(&admitted)
        .map_err(AssistantStoreError::Invalid)?;
    Ok((original == *source).then_some((admitted, current)))
}

/// The stored native role pins the admitted source for both the main
/// coordinator and its Worker. A device action needs the additional device
/// scope intersection even if the request can still read the issue.
pub async fn authorized_session_on(
    db: &impl ConnectionTrait,
    session: &desk_diagnose_core::session::PersistedAgentSession,
    requires_device: bool,
    now: i64,
) -> Result<Option<IssueAdmission>, AssistantStoreError> {
    use desk_diagnose_core::issue_authority::IssueWorkSource;
    let Some(source) = session.agent_role.issue_source() else {
        return Ok(None);
    };
    let (snapshot, policy) = resolve_reference(db, &session.actor_id, &source.reference, now)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    let native_root = session
        .agent_role
        .binding()
        .map_or(session.conversation_id.as_str(), |binding| {
            binding.root_conversation_id.as_str()
        });
    if snapshot.execution_root_id != native_root
        || IssueWorkSource::from_admission(&snapshot).map_err(AssistantStoreError::Invalid)?
            != *source
        || (requires_device && !snapshot.can_use_devices(&policy, now))
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(Some(snapshot))
}

pub async fn publication_revision(
    db: &impl ConnectionTrait,
    origin: &ReplyOrigin,
    issue: &IssueAccess,
    now: i64,
) -> Result<Option<u64>, AssistantStoreError> {
    let reference = IssueSourceRef {
        authorization_id: origin.authorization_id.clone(),
        subscription_revision: origin.authorization_revision,
        tool_ref: issue.tool_ref(),
    };
    let Some((snapshot, policy)) = resolve(db, &origin.actor_id, &reference, issue, now).await?
    else {
        return Ok(None);
    };
    Ok((snapshot.request_id == origin.request_id
        && snapshot.subscription.conversation_id == origin.root_conversation_id
        && snapshot.can_publish(&policy, now)
        && crate::issue_events::supplements::publication_content_current(db, &snapshot, &policy)
            .await?)
        .then_some(policy.revision))
}

/// The host has already checked this exact native input, version and lease.
/// A source reference cannot be borrowed from another execution or input.
pub async fn resolve_input(
    db: &impl ConnectionTrait,
    held: &desk_diagnose_core::session::PersistedAgentSession,
    original: &desk_diagnose_core::input_read_context::ReadContextSelection,
    issue: &IssueAccess,
    now: i64,
) -> Result<Option<(IssueAdmission, IssueSubscription)>, AssistantStoreError> {
    let Some(reference) = original
        .external_issue_sources
        .iter()
        .find(|source| source.tool_ref == issue.tool_ref())
    else {
        return Ok(None);
    };
    let Some((snapshot, policy)) = resolve(db, &held.actor_id, reference, issue, now).await? else {
        return Ok(None);
    };
    let root = held
        .agent_role
        .binding()
        .map_or(held.conversation_id.as_str(), |binding| {
            binding.root_conversation_id.as_str()
        });
    Ok(
        (original.external_issues.contains(issue) && snapshot.execution_root_id == root)
            .then_some((snapshot, policy)),
    )
}

pub async fn publication_revision_for_input(
    db: &impl ConnectionTrait,
    held: &desk_diagnose_core::session::PersistedAgentSession,
    original: &desk_diagnose_core::input_read_context::ReadContextSelection,
    origin: &ReplyOrigin,
    issue: &IssueAccess,
    now: i64,
) -> Result<Option<u64>, AssistantStoreError> {
    let Some((snapshot, policy)) = resolve_input(db, held, original, issue, now).await? else {
        return Ok(None);
    };
    Ok((snapshot.request_id == origin.request_id
        && snapshot.subscription.conversation_id == origin.root_conversation_id
        && snapshot.authorization_id == origin.authorization_id
        && snapshot.subscription.revision == origin.authorization_revision
        && snapshot.can_publish(&policy, now)
        && crate::issue_events::supplements::publication_content_current(db, &snapshot, &policy)
            .await?)
        .then_some(policy.revision))
}

pub async fn input_sources_current(
    db: &impl ConnectionTrait,
    held: &desk_diagnose_core::session::PersistedAgentSession,
    original: &desk_diagnose_core::input_read_context::ReadContextSelection,
    now: i64,
) -> Result<bool, AssistantStoreError> {
    for reference in &original.external_issue_sources {
        let Some(issue) = original
            .external_issues
            .iter()
            .find(|issue| issue.tool_ref() == reference.tool_ref)
        else {
            return Ok(false);
        };
        if resolve_input(db, held, original, issue, now)
            .await?
            .is_none()
        {
            return Ok(false);
        }
    }
    Ok(true)
}

pub async fn stop_request(
    db: &impl ConnectionTrait,
    actor: &str,
    request: &str,
    now: i64,
) -> Result<(), AssistantStoreError> {
    crate::issue_events::lifecycle::stop_request_on(db, actor, request, now).await?;
    use sea_orm::sea_query::Expr;
    admission::Entity::update_many()
        .filter(admission::Column::ActorId.eq(actor))
        .filter(admission::Column::RequestId.eq(request))
        .filter(admission::Column::StoppedAtMs.is_null())
        .col_expr(admission::Column::StoppedAtMs, Expr::value(now))
        .exec(db)
        .await?;
    Ok(())
}

pub async fn stop_conversation(
    db: &impl ConnectionTrait,
    actor: &str,
    conversation: &str,
    now: i64,
) -> Result<(), AssistantStoreError> {
    crate::issue_events::lifecycle::stop_conversation_on(db, actor, conversation, now).await?;
    use sea_orm::sea_query::Expr;
    admission::Entity::update_many()
        .filter(admission::Column::ActorId.eq(actor))
        .filter(admission::Column::ConversationId.eq(conversation))
        .filter(admission::Column::StoppedAtMs.is_null())
        .col_expr(admission::Column::StoppedAtMs, Expr::value(now))
        .exec(db)
        .await?;
    Ok(())
}
fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
fn invalid() -> AssistantStoreError {
    AssistantStoreError::Invalid("invalid original issue authority")
}

#[cfg(test)]
mod tests;

/// Current source reads stop after deletion; replay never restores erased bodies.
pub async fn retire_conversation(
    db: &impl ConnectionTrait,
    actor: &str,
    conversation: &str,
    now: i64,
) -> Result<(), AssistantStoreError> {
    crate::issue_events::lifecycle::retire_conversation_on(db, actor, conversation, now).await?;
    use sea_orm::{Condition, sea_query::Expr};
    admission::Entity::update_many()
        .filter(admission::Column::ActorId.eq(actor))
        .filter(
            Condition::any()
                .add(admission::Column::ConversationId.eq(conversation))
                .add(admission::Column::ExecutionRootId.eq(conversation)),
        )
        .filter(admission::Column::RetiredAtMs.is_null())
        .col_expr(admission::Column::SnapshotJson, Expr::value(None::<String>))
        .col_expr(admission::Column::RetiredAtMs, Expr::value(now))
        .exec(db)
        .await?;
    Ok(())
}

pub async fn retire_actor(
    db: &impl ConnectionTrait,
    actor: &str,
    now: i64,
) -> Result<(), AssistantStoreError> {
    crate::issue_events::lifecycle::retire_actor_on(db, actor, now).await?;
    use sea_orm::sea_query::Expr;
    admission::Entity::update_many()
        .filter(admission::Column::ActorId.eq(actor))
        .filter(admission::Column::RetiredAtMs.is_null())
        .col_expr(admission::Column::SnapshotJson, Expr::value(None::<String>))
        .col_expr(admission::Column::RetiredAtMs, Expr::value(now))
        .exec(db)
        .await?;
    Ok(())
}

pub async fn expire_on(
    db: &impl ConnectionTrait,
    now: i64,
    limit: u64,
) -> Result<u64, AssistantStoreError> {
    use sea_orm::{QueryOrder, QuerySelect, sea_query::Expr};
    let ids = admission::Entity::find()
        .select_only()
        .column(admission::Column::Id)
        .filter(admission::Column::RetiredAtMs.is_null())
        .filter(admission::Column::ContentExpiresAtMs.lte(now))
        .order_by_asc(admission::Column::Id)
        .limit(limit)
        .into_tuple::<i64>()
        .all(db)
        .await?;
    if ids.is_empty() {
        return Ok(0);
    }
    let count = ids.len() as u64;
    admission::Entity::update_many()
        .filter(admission::Column::Id.is_in(ids))
        .filter(admission::Column::RetiredAtMs.is_null())
        .filter(admission::Column::ContentExpiresAtMs.lte(now))
        .col_expr(admission::Column::SnapshotJson, Expr::value(None::<String>))
        .col_expr(admission::Column::RetiredAtMs, Expr::value(now))
        .exec(db)
        .await?;
    Ok(count)
}
