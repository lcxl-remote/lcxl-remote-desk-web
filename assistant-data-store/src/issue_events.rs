//! Source polling uses the same owner lock as policy changes. The caller owns
//! the transaction; a failed page must roll back both observations and progress.

use crate::{
    AssistantStoreError,
    entity::{agent_issue_event as event, agent_issue_source as source},
    signed,
};
use desk_diagnose_core::{issue_authority::IssueSubscription, issue_events::IssueSourceComment};
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
    Set, sea_query::OnConflict,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::num::NonZeroU32;

pub mod lifecycle;
mod observations;
pub mod routing;
pub mod supplements;
pub use observations::{decode_comment, is_outgoing_echo_on, original_policy};
#[cfg(test)]
mod tests;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceScan {
    pub subscription_revision: u64,
    pub started_at_ms: i64,
    pub since_ms: i64,
    pub page: NonZeroU32,
    digest: String,
    previous_digest: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SourceClaim {
    pub row: source::Model,
    pub policy: IssueSubscription,
    pub scan: SourceScan,
}

/// A policy commit invalidates the old read lease before it becomes visible.
/// Source identity changes start a new window; pause/resume preserves its watermark.
pub(crate) async fn policy_changed_on(
    db: &impl ConnectionTrait,
    previous: Option<&IssueSubscription>,
    policy: &IssueSubscription,
    now: i64,
) -> Result<(), AssistantStoreError> {
    if previous.is_some_and(|previous| previous.repository != policy.repository) {
        source::Entity::delete_many()
            .filter(source::Column::ActorId.eq(&policy.actor_id))
            .filter(source::Column::SubscriptionId.eq(&policy.subscription_id))
            .exec(db)
            .await?;
    }
    if let Some(row) = source::Entity::find()
        .filter(source::Column::ActorId.eq(&policy.actor_id))
        .filter(source::Column::SubscriptionId.eq(&policy.subscription_id))
        .one(db)
        .await?
    {
        let changed = source::Entity::update_many()
            .filter(source::Column::Id.eq(row.id))
            .filter(source::Column::Revision.eq(row.revision))
            .set(source::ActiveModel {
                conversation_id: Set(policy.conversation_id.clone()),
                revision: Set(crate::increment(row.revision)?),
                scan_json: Set(None),
                scan_sha256: Set(None),
                lease_id: Set(None),
                lease_until_ms: Set(None),
                next_poll_at_ms: Set(now),
                updated_at_ms: Set(now),
                ..Default::default()
            })
            .exec(db)
            .await?
            .rows_affected;
        if changed != 1 {
            return Err(AssistantStoreError::Inaccessible);
        }
    } else if policy.enabled {
        initialize_on(db, policy, now).await?;
    }
    Ok(())
}

pub async fn initialize_on(
    db: &impl ConnectionTrait,
    policy: &IssueSubscription,
    enabled_at_ms: i64,
) -> Result<(), AssistantStoreError> {
    policy.validate().map_err(AssistantStoreError::Invalid)?;
    if !policy.enabled || enabled_at_ms < 0 {
        return Err(invalid());
    }
    source::Entity::insert(source::ActiveModel {
        subscription_id: Set(policy.subscription_id.clone()),
        actor_id: Set(policy.actor_id.clone()),
        conversation_id: Set(policy.conversation_id.clone()),
        revision: Set(1),
        first_enabled_at_ms: Set(enabled_at_ms),
        watermark_ms: Set(enabled_at_ms),
        next_poll_at_ms: Set(enabled_at_ms),
        updated_at_ms: Set(enabled_at_ms),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::column(source::Column::SubscriptionId)
            .do_nothing()
            .to_owned(),
    )
    .exec_without_returning(db)
    .await?;
    let row = source::Entity::find()
        .filter(source::Column::SubscriptionId.eq(&policy.subscription_id))
        .one(db)
        .await?
        .ok_or_else(invalid)?;
    if row.actor_id != policy.actor_id || row.conversation_id != policy.conversation_id {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}

/// Keyset pagination prevents one busy or unhealthy subscription starving others.
pub async fn candidates_on(
    db: &impl ConnectionTrait,
    after: i64,
    now: i64,
    limit: u64,
) -> Result<Vec<source::Model>, AssistantStoreError> {
    Ok(source::Entity::find()
        .filter(source::Column::Id.gt(after))
        .filter(source::Column::NextPollAtMs.lte(now))
        .filter(
            Condition::any()
                .add(source::Column::LeaseUntilMs.is_null())
                .add(source::Column::LeaseUntilMs.lte(now)),
        )
        .order_by_asc(source::Column::Id)
        .limit(limit)
        .all(db)
        .await?)
}

pub async fn claim_on(
    db: &impl ConnectionTrait,
    policy: &IssueSubscription,
    lease_id: &str,
    now: i64,
    until: i64,
) -> Result<Option<SourceClaim>, AssistantStoreError> {
    if now < 0
        || until <= now
        || lease_id.is_empty()
        || lease_id.len() > 512
        || lease_id.chars().any(char::is_control)
    {
        return Err(invalid());
    }
    if !policy.enabled
        || crate::issue_authority::current(db, &policy.actor_id, &policy.subscription_id)
            .await?
            .as_ref()
            != Some(policy)
    {
        return Ok(None);
    }
    let row = source::Entity::find()
        .filter(source::Column::SubscriptionId.eq(&policy.subscription_id))
        .filter(source::Column::ActorId.eq(&policy.actor_id))
        .one(db)
        .await?
        .ok_or_else(invalid)?;
    if row.next_poll_at_ms > now || row.lease_until_ms.is_some_and(|at| at > now) {
        return Ok(None);
    }
    if row.conversation_id != policy.conversation_id
        || row.watermark_ms < row.first_enabled_at_ms
        || row.watermark_ms > now
    {
        return Err(invalid());
    }
    let saved = decode_scan(&row)?;
    if row.updated_at_ms > now || saved.as_ref().is_some_and(|scan| scan.started_at_ms > now) {
        return Ok(None);
    }
    let scan = saved
        .filter(|scan| scan.subscription_revision == policy.revision && row.lease_id.is_none())
        .unwrap_or_else(|| SourceScan {
            subscription_revision: policy.revision,
            started_at_ms: now,
            since_ms: row.watermark_ms.saturating_sub(1000).max(0),
            page: NonZeroU32::MIN,
            digest: digest(""),
            previous_digest: None,
        });
    let json = serde_json::to_string(&scan).map_err(|_| invalid())?;
    let changed = source::Entity::update_many()
        .filter(source::Column::Id.eq(row.id))
        .filter(source::Column::Revision.eq(row.revision))
        .set(source::ActiveModel {
            revision: Set(crate::increment(row.revision)?),
            lease_id: Set(Some(lease_id.into())),
            lease_until_ms: Set(Some(until)),
            scan_sha256: Set(Some(digest(&json))),
            scan_json: Set(Some(json)),
            updated_at_ms: Set(now),
            ..Default::default()
        })
        .exec(db)
        .await?
        .rows_affected;
    if changed != 1 {
        return Ok(None);
    }
    let row = source::Entity::find_by_id(row.id)
        .one(db)
        .await?
        .ok_or_else(invalid)?;
    Ok(Some(SourceClaim {
        row,
        policy: policy.clone(),
        scan,
    }))
}

/// Each page is one durable unit. Two matching complete passes account for
/// offset rows shifting during deletion or concurrent edits on the remote API.
pub async fn commit_page_on(
    db: &impl ConnectionTrait,
    claim: &SourceClaim,
    comments: &[IssueSourceComment],
    next: Option<NonZeroU32>,
    now: i64,
    next_poll_at_ms: i64,
) -> Result<bool, AssistantStoreError> {
    validate_claim_on(db, claim, now).await?;
    if next_poll_at_ms < now
        || next.is_some_and(|page| claim.scan.page.get().checked_add(1) != Some(page.get()))
    {
        return Err(invalid());
    }
    let mut scan = claim.scan.clone();
    for comment in comments {
        comment.validate().map_err(AssistantStoreError::Invalid)?;
        if comment.repository != claim.policy.repository {
            return Err(invalid());
        }
        observations::observe_on(db, claim, comment, now).await?;
        scan.digest = digest(&format!("{}:{}", scan.digest, comment.fingerprint()));
    }
    let finished = if let Some(page) = next {
        scan.page = page;
        false
    } else if scan.previous_digest.as_ref() == Some(&scan.digest) {
        true
    } else {
        scan.previous_digest = Some(scan.digest.clone());
        scan.digest = digest("");
        scan.page = NonZeroU32::MIN;
        false
    };
    let json = (!finished)
        .then(|| serde_json::to_string(&scan))
        .transpose()
        .map_err(|_| invalid())?;
    let changed = fenced_update(claim)
        .set(source::ActiveModel {
            revision: Set(crate::increment(claim.row.revision)?),
            watermark_ms: Set(if finished {
                scan.started_at_ms
            } else {
                claim.row.watermark_ms
            }),
            scan_sha256: Set(json.as_deref().map(digest)),
            scan_json: Set(json),
            lease_id: Set(None),
            lease_until_ms: Set(None),
            next_poll_at_ms: Set(if finished { next_poll_at_ms } else { now }),
            last_error: Set(None),
            updated_at_ms: Set(now),
            ..Default::default()
        })
        .exec(db)
        .await?
        .rows_affected;
    if changed != 1 {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(finished)
}

pub async fn retry_on(
    db: &impl ConnectionTrait,
    claim: &SourceClaim,
    now: i64,
    next_poll_at_ms: i64,
    reason: &str,
) -> Result<(), AssistantStoreError> {
    validate_claim_on(db, claim, now).await?;
    if next_poll_at_ms < now
        || reason.is_empty()
        || reason.len() > 128
        || !reason
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
    {
        return Err(invalid());
    }
    let changed = fenced_update(claim)
        .set(source::ActiveModel {
            revision: Set(crate::increment(claim.row.revision)?),
            lease_id: Set(None),
            lease_until_ms: Set(None),
            next_poll_at_ms: Set(next_poll_at_ms),
            last_error: Set(Some(reason.into())),
            updated_at_ms: Set(now),
            ..Default::default()
        })
        .exec(db)
        .await?
        .rows_affected;
    if changed != 1 {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}

fn fenced_update(claim: &SourceClaim) -> sea_orm::UpdateMany<source::Entity> {
    source::Entity::update_many()
        .filter(source::Column::Id.eq(claim.row.id))
        .filter(source::Column::ActorId.eq(&claim.policy.actor_id))
        .filter(source::Column::SubscriptionId.eq(&claim.policy.subscription_id))
        .filter(source::Column::Revision.eq(claim.row.revision))
        .filter(source::Column::LeaseId.eq(claim.row.lease_id.clone()))
        .filter(source::Column::ScanJson.eq(claim.row.scan_json.clone()))
}

async fn validate_claim_on(
    db: &impl ConnectionTrait,
    claim: &SourceClaim,
    now: i64,
) -> Result<(), AssistantStoreError> {
    let current = source::Entity::find_by_id(claim.row.id)
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if current != claim.row
        || current.actor_id != claim.policy.actor_id
        || current.subscription_id != claim.policy.subscription_id
        || current.conversation_id != claim.policy.conversation_id
        || current.lease_id.is_none()
        || current.lease_until_ms.is_none_or(|at| now >= at)
        || now < current.updated_at_ms
        || decode_scan(&current)?.as_ref() != Some(&claim.scan)
        || crate::issue_authority::current(
            db,
            &claim.policy.actor_id,
            &claim.policy.subscription_id,
        )
        .await?
        .as_ref()
            != Some(&claim.policy)
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}

fn decode_scan(row: &source::Model) -> Result<Option<SourceScan>, AssistantStoreError> {
    match (&row.scan_json, &row.scan_sha256) {
        (None, None) => Ok(None),
        (Some(json), Some(hash)) if digest(json) == *hash => {
            let scan: SourceScan = serde_json::from_str(json).map_err(|_| invalid())?;
            if scan.subscription_revision == 0
                || scan.started_at_ms < row.watermark_ms
                || scan.since_ms < 0
                || scan.since_ms != row.watermark_ms.saturating_sub(1000).max(0)
                || !valid_digest(&scan.digest)
                || scan
                    .previous_digest
                    .as_deref()
                    .is_some_and(|hash| !valid_digest(hash))
            {
                return Err(invalid());
            }
            Ok(Some(scan))
        }
        _ => Err(invalid()),
    }
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
fn invalid() -> AssistantStoreError {
    AssistantStoreError::Invalid("invalid issue source state")
}
