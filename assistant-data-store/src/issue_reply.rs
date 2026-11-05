//! Durable publication facts. Hosts own transactions, native authorization and I/O.

use crate::{AssistantStoreError, entity::agent_issue_reply as reply};
use desk_diagnose_core::issue_reply::{FrozenIssueReply, IssueReplyReceipt, IssueReplyReceiptKey};
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
    Set,
    sea_query::{Expr, OnConflict},
};
use sha2::{Digest, Sha256};

pub mod delivery;
pub mod lifecycle;
pub mod preparation;

pub const INTERNAL: &str = "internal";
pub const READY: &str = "ready";
pub const CLAIMED: &str = "claimed";
pub const DISPATCHED: &str = "dispatched";
pub const DELIVERED: &str = "delivered";
pub const NOT_SENT: &str = "not_sent";
pub const UNKNOWN: &str = "unknown";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    Internal,
    Automatic,
}

#[derive(Debug, Clone)]
pub struct Claim {
    pub artifact: Option<FrozenIssueReply>,
    pub receipt: IssueReplyReceiptKey,
    pub row: reply::Model,
}

impl Claim {
    pub fn publication(&self) -> Result<&FrozenIssueReply, AssistantStoreError> {
        self.artifact.as_ref().ok_or_else(invalid)
    }
    pub fn needs_reconciliation(&self) -> bool {
        self.row.state == UNKNOWN
    }
}

/// Constructed by the trusted publication call, never from a client body.
pub struct ReplyPreparation<'a> {
    pub held: &'a desk_diagnose_core::session::PersistedAgentSession,
    pub origin: desk_diagnose_core::issue_reply::ReplyOrigin,
    pub access: desk_diagnose_core::external_tools::IssueAccess,
    pub body: String,
    pub output: desk_agent_protocol::data_lineage::DataEnvelope,
    pub comment_tool_signature: String,
    pub random_marker: [u8; 16],
}

impl ReplyPreparation<'_> {
    pub fn check_context(&self) -> Result<(), AssistantStoreError> {
        if self.origin.actor_id != self.held.actor_id
            || self.origin.execution_conversation_id != self.held.conversation_id
            || self.origin.input_revision != self.held.input_revision
            || self.origin.control_revision != self.held.control_revision
            || !self
                .held
                .conversation
                .iter()
                .flat_map(|message| &message.tool_calls)
                .any(|call| {
                    call.id == self.origin.tool_call_id
                        && call.name == desk_diagnose_core::issue_reply::PREPARE_TOOL_NAME
                })
        {
            return Err(invalid());
        }
        Ok(())
    }
}

/// The caller first reads the successful native model receipt under the exact
/// original work. An automatic mode additionally requires current host authority.
pub async fn freeze(
    db: &impl ConnectionTrait,
    artifact: &FrozenIssueReply,
    mode: DeliveryMode,
    now_ms: i64,
) -> Result<reply::Model, AssistantStoreError> {
    artifact.validate().map_err(|_| invalid())?;
    let (state, reason) = match mode {
        DeliveryMode::Internal => (INTERNAL, Some("not_enabled")),
        DeliveryMode::Automatic
            if artifact
                .authorize_delivery(now_ms.try_into().map_err(|_| invalid())?)
                .is_ok() =>
        {
            (READY, None)
        }
        DeliveryMode::Automatic => (INTERNAL, Some("source_not_publishable")),
    };
    let json = serde_json::to_string(artifact).map_err(|_| invalid())?;
    let receipt_json = serde_json::to_string(&artifact.receipt_key()).map_err(|_| invalid())?;
    reply::Entity::insert(reply::ActiveModel {
        artifact_id: Set(artifact.artifact_id.clone()),
        actor_id: Set(artifact.origin.actor_id.clone()),
        root_conversation_id: Set(artifact.origin.root_conversation_id.clone()),
        request_id: Set(artifact.origin.request_id.clone()),
        execution_conversation_id: Set(artifact.origin.execution_conversation_id.clone()),
        input_revision: Set(i64::try_from(artifact.origin.input_revision).map_err(|_| invalid())?),
        artifact_sha256: Set(digest(&json)),
        artifact_json: Set(Some(json)),
        receipt_sha256: Set(digest(&receipt_json)),
        receipt_json: Set(receipt_json),
        retired_at_ms: Set(None),
        content_expires_at_ms: Set(artifact
            .content_expires_at_unix_ms()
            .map(|value| i64::try_from(value).unwrap_or(i64::MAX))),
        state: Set(state.into()),
        reason: Set(reason.map(str::to_owned)),
        owner_node: Set(None),
        claim_token: Set(None),
        attempt: Set(0),
        lease_expires_at_ms: Set(None),
        dispatch_token: Set(None),
        dispatched_at_ms: Set(None),
        dispatched_authorization_revision: Set(None),
        comment_id: Set(None),
        reconciliation_page: Set(1),
        next_attempt_at_ms: Set(now_ms),
        created_at_ms: Set(now_ms),
        updated_at_ms: Set(now_ms),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::column(reply::Column::ArtifactId)
            .do_nothing()
            .to_owned(),
    )
    .exec_without_returning(db)
    .await?;
    let saved = load(db, &artifact.origin.actor_id, &artifact.artifact_id)
        .await?
        .ok_or_else(invalid)?;
    let key = receipt_key(&saved)?;
    let mut replay = artifact.clone();
    replay.marker = key.marker;
    let restored = serde_json::to_string(&replay).map_err(|_| invalid())?;
    if digest(&restored) != saved.artifact_sha256 {
        return Err(invalid());
    }
    Ok(saved)
}

pub async fn load(
    db: &impl ConnectionTrait,
    actor: &str,
    artifact: &str,
) -> Result<Option<reply::Model>, AssistantStoreError> {
    Ok(reply::Entity::find()
        .filter(reply::Column::ActorId.eq(actor))
        .filter(reply::Column::ArtifactId.eq(artifact))
        .one(db)
        .await?)
}

pub fn receipt_key(row: &reply::Model) -> Result<IssueReplyReceiptKey, AssistantStoreError> {
    if digest(&row.receipt_json) != row.receipt_sha256 {
        return Err(invalid());
    }
    let key: IssueReplyReceiptKey =
        serde_json::from_str(&row.receipt_json).map_err(|_| invalid())?;
    key.validate().map_err(|_| invalid())?;
    Ok(key)
}

pub fn decode(row: &reply::Model) -> Result<FrozenIssueReply, AssistantStoreError> {
    let json = row.artifact_json.as_deref().ok_or_else(invalid)?;
    if row.retired_at_ms.is_some()
        || row.reconciliation_page < 1
        || row.reconciliation_page > i64::from(u32::MAX)
        || digest(json) != row.artifact_sha256
    {
        return Err(invalid());
    }
    let artifact: FrozenIssueReply = serde_json::from_str(json).map_err(|_| invalid())?;
    artifact.validate().map_err(|_| invalid())?;
    if artifact.artifact_id != row.artifact_id
        || artifact.origin.actor_id != row.actor_id
        || artifact.origin.root_conversation_id != row.root_conversation_id
        || artifact.origin.request_id != row.request_id
        || artifact.origin.execution_conversation_id != row.execution_conversation_id
        || i64::try_from(artifact.origin.input_revision).ok() != Some(row.input_revision)
        || artifact.receipt_key() != receipt_key(row)?
        || artifact
            .content_expires_at_unix_ms()
            .map(|value| i64::try_from(value).unwrap_or(i64::MAX))
            != row.content_expires_at_ms
    {
        return Err(invalid());
    }
    Ok(artifact)
}

fn claim_data(
    row: &reply::Model,
) -> Result<(Option<FrozenIssueReply>, IssueReplyReceiptKey), AssistantStoreError> {
    let receipt = receipt_key(row)?;
    let artifact = if row.artifact_json.is_some() {
        Some(decode(row)?)
    } else if row.retired_at_ms.is_some() && row.state == UNKNOWN && row.dispatch_token.is_some() {
        None
    } else {
        return Err(invalid());
    };
    Ok((artifact, receipt))
}

/// Claim recovery preserves the immutable dispatch generation. An expired
/// dispatch becomes unknown; expiry is never evidence that it was not sent.
pub async fn recover(db: &impl ConnectionTrait, now_ms: i64) -> Result<(), AssistantStoreError> {
    for (old, next) in [(CLAIMED, READY), (DISPATCHED, UNKNOWN)] {
        reply::Entity::update_many()
            .filter(reply::Column::State.eq(old))
            .filter(reply::Column::LeaseExpiresAtMs.lte(now_ms))
            .col_expr(reply::Column::State, Expr::value(next))
            .col_expr(reply::Column::ClaimToken, Expr::value(None::<String>))
            .col_expr(reply::Column::OwnerNode, Expr::value(None::<String>))
            .col_expr(reply::Column::LeaseExpiresAtMs, Expr::value(None::<i64>))
            .col_expr(reply::Column::NextAttemptAtMs, Expr::value(now_ms))
            .col_expr(reply::Column::UpdatedAtMs, Expr::value(now_ms))
            .exec(db)
            .await?;
    }
    Ok(())
}

/// Random claim tokens are minted by the host. The shared row is authoritative
/// across instances; the single-node host uses its ordinary writer transaction.
pub async fn claim_next(
    db: &impl ConnectionTrait,
    node: &str,
    token: &str,
    now_ms: i64,
    lease_until_ms: i64,
) -> Result<Option<Claim>, AssistantStoreError> {
    if token.is_empty()
        || token.len() > 256
        || node.is_empty()
        || node.len() > 256
        || lease_until_ms <= now_ms
    {
        return Err(invalid());
    }
    lifecycle::expire_on(db, now_ms, 64).await?;
    recover(db, now_ms).await?;
    let available = || {
        Condition::all()
            .add(reply::Column::State.is_in([READY, UNKNOWN]))
            .add(
                Condition::any()
                    .add(reply::Column::Reason.is_null())
                    .add(reply::Column::Reason.ne("artifact_invalid")),
            )
            .add(reply::Column::NextAttemptAtMs.lte(now_ms))
            .add(
                Condition::any()
                    .add(reply::Column::LeaseExpiresAtMs.is_null())
                    .add(reply::Column::LeaseExpiresAtMs.lte(now_ms)),
            )
    };
    for row in reply::Entity::find()
        .filter(available())
        .order_by_asc(reply::Column::Id)
        .limit(16)
        .all(db)
        .await?
    {
        let (artifact, receipt) = match claim_data(&row) {
            Ok(data) => data,
            Err(_) => {
                // A malformed artifact cannot block unrelated requests or become
                // permission to retry an already dispatched external effect.
                reply::Entity::update_many()
                    .filter(reply::Column::Id.eq(row.id))
                    .filter(available())
                    .filter(reply::Column::Attempt.eq(row.attempt))
                    .filter(reply::Column::ArtifactSha256.eq(&row.artifact_sha256))
                    .col_expr(reply::Column::Reason, Expr::value("artifact_invalid"))
                    .col_expr(
                        reply::Column::State,
                        Expr::value(if row.state == UNKNOWN {
                            UNKNOWN
                        } else {
                            INTERNAL
                        }),
                    )
                    .col_expr(reply::Column::UpdatedAtMs, Expr::value(now_ms))
                    .exec(db)
                    .await?;
                continue;
            }
        };
        let attempt = row.attempt.checked_add(1).ok_or_else(invalid)?;
        let state = if row.state == UNKNOWN {
            UNKNOWN
        } else {
            CLAIMED
        };
        let updated = reply::Entity::update_many()
            .filter(reply::Column::Id.eq(row.id))
            .filter(available())
            .filter(reply::Column::Attempt.eq(row.attempt))
            .filter(reply::Column::ArtifactSha256.eq(&row.artifact_sha256))
            .col_expr(reply::Column::State, Expr::value(state))
            .col_expr(reply::Column::Attempt, Expr::value(attempt))
            .col_expr(reply::Column::OwnerNode, Expr::value(node))
            .col_expr(reply::Column::ClaimToken, Expr::value(token))
            .col_expr(reply::Column::LeaseExpiresAtMs, Expr::value(lease_until_ms))
            .col_expr(reply::Column::UpdatedAtMs, Expr::value(now_ms))
            .exec(db)
            .await?;
        if updated.rows_affected == 1 {
            let row = load(db, &row.actor_id, &row.artifact_id)
                .await?
                .ok_or_else(invalid)?;
            if row.claim_token.as_deref() == Some(token) && row.attempt == attempt {
                return Ok(Some(Claim {
                    artifact,
                    receipt,
                    row,
                }));
            }
        }
    }
    Ok(None)
}

fn held(claim: &Claim, now_ms: i64) -> sea_orm::UpdateMany<reply::Entity> {
    reply::Entity::update_many()
        .filter(reply::Column::Id.eq(claim.row.id))
        .filter(reply::Column::ArtifactSha256.eq(&claim.row.artifact_sha256))
        .filter(reply::Column::Attempt.eq(claim.row.attempt))
        .filter(reply::Column::ClaimToken.eq(claim.row.claim_token.clone()))
        .filter(reply::Column::LeaseExpiresAtMs.gt(now_ms))
}

/// Called only after the host has rechecked current request, grant, source and
/// connection authority in the same transaction. No model text is accepted here.
pub async fn mark_dispatched(
    db: &impl ConnectionTrait,
    claim: &Claim,
    current_revision: u64,
    now_ms: i64,
) -> Result<bool, AssistantStoreError> {
    let minimum = claim.publication()?.origin.authorization_revision;
    let revision = i64::try_from(current_revision)
        .ok()
        .filter(|revision| *revision > 0 && current_revision >= minimum)
        .ok_or_else(invalid)?;
    Ok(held(claim, now_ms)
        .filter(reply::Column::State.eq(CLAIMED))
        .filter(reply::Column::DispatchToken.is_null())
        .filter(reply::Column::ArtifactJson.is_not_null())
        .filter(reply::Column::RetiredAtMs.is_null())
        .col_expr(reply::Column::State, Expr::value(DISPATCHED))
        .col_expr(
            reply::Column::DispatchToken,
            Expr::value(claim.row.claim_token.clone()),
        )
        .col_expr(reply::Column::DispatchedAtMs, Expr::value(now_ms))
        .col_expr(
            reply::Column::DispatchedAuthorizationRevision,
            Expr::value(revision),
        )
        .col_expr(reply::Column::UpdatedAtMs, Expr::value(now_ms))
        .exec(db)
        .await?
        .rows_affected
        == 1)
}

pub async fn release_unsent(
    db: &impl ConnectionTrait,
    claim: &Claim,
    now_ms: i64,
    retry_at_ms: i64,
) -> Result<bool, AssistantStoreError> {
    Ok(held(claim, now_ms)
        .filter(reply::Column::State.eq(CLAIMED))
        .filter(reply::Column::DispatchToken.is_null())
        .col_expr(reply::Column::State, Expr::value(READY))
        .col_expr(reply::Column::ClaimToken, Expr::value(None::<String>))
        .col_expr(reply::Column::OwnerNode, Expr::value(None::<String>))
        .col_expr(reply::Column::LeaseExpiresAtMs, Expr::value(None::<i64>))
        .col_expr(
            reply::Column::NextAttemptAtMs,
            Expr::value(retry_at_ms.max(now_ms)),
        )
        .col_expr(reply::Column::UpdatedAtMs, Expr::value(now_ms))
        .exec(db)
        .await?
        .rows_affected
        == 1)
}

/// A failed current authorization or binding keeps the frozen result internal.
/// This transition is allowed only before a dispatch intent exists.
pub async fn keep_internal(
    db: &impl ConnectionTrait,
    claim: &Claim,
    reason: &'static str,
    now_ms: i64,
) -> Result<bool, AssistantStoreError> {
    Ok(held(claim, now_ms)
        .filter(reply::Column::State.eq(CLAIMED))
        .filter(reply::Column::DispatchToken.is_null())
        .col_expr(reply::Column::State, Expr::value(INTERNAL))
        .col_expr(reply::Column::Reason, Expr::value(reason))
        .col_expr(reply::Column::ClaimToken, Expr::value(None::<String>))
        .col_expr(reply::Column::OwnerNode, Expr::value(None::<String>))
        .col_expr(reply::Column::LeaseExpiresAtMs, Expr::value(None::<i64>))
        .col_expr(reply::Column::UpdatedAtMs, Expr::value(now_ms))
        .exec(db)
        .await?
        .rows_affected
        == 1)
}

pub async fn defer_reconciliation(
    db: &impl ConnectionTrait,
    claim: &Claim,
    now_ms: i64,
    next_ms: i64,
    next_page: std::num::NonZeroU32,
) -> Result<bool, AssistantStoreError> {
    Ok(held(claim, now_ms)
        .col_expr(
            reply::Column::ReconciliationPage,
            Expr::value(i64::from(next_page.get())),
        )
        .filter(reply::Column::State.eq(UNKNOWN))
        .col_expr(reply::Column::ClaimToken, Expr::value(None::<String>))
        .col_expr(reply::Column::OwnerNode, Expr::value(None::<String>))
        .col_expr(reply::Column::LeaseExpiresAtMs, Expr::value(None::<i64>))
        .col_expr(
            reply::Column::NextAttemptAtMs,
            Expr::value(next_ms.max(now_ms)),
        )
        .col_expr(reply::Column::UpdatedAtMs, Expr::value(now_ms))
        .exec(db)
        .await?
        .rows_affected
        == 1)
}

pub async fn mark_unknown(
    db: &impl ConnectionTrait,
    artifact: &str,
    dispatch: &str,
    now_ms: i64,
) -> Result<bool, AssistantStoreError> {
    Ok(reply::Entity::update_many()
        .filter(reply::Column::ArtifactId.eq(artifact))
        .filter(reply::Column::DispatchToken.eq(dispatch))
        .filter(reply::Column::State.eq(DISPATCHED))
        .col_expr(reply::Column::State, Expr::value(UNKNOWN))
        .col_expr(reply::Column::ClaimToken, Expr::value(None::<String>))
        .col_expr(reply::Column::OwnerNode, Expr::value(None::<String>))
        .col_expr(reply::Column::LeaseExpiresAtMs, Expr::value(None::<i64>))
        .col_expr(reply::Column::NextAttemptAtMs, Expr::value(now_ms))
        .col_expr(reply::Column::UpdatedAtMs, Expr::value(now_ms))
        .exec(db)
        .await?
        .rows_affected
        == 1)
}

/// A positive remote receipt may arrive after lease takeover or cancellation.
/// Only the original dispatch generation and exact account/body can settle it.
pub async fn record_receipt(
    db: &impl ConnectionTrait,
    claim: &Claim,
    dispatch: &str,
    receipt: &IssueReplyReceipt,
    now_ms: i64,
) -> Result<bool, AssistantStoreError> {
    if !claim
        .receipt
        .matches_receipt(receipt.account_id, &receipt.body)
    {
        return Err(invalid());
    }
    let comment_id = i64::try_from(receipt.comment_id)
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(invalid)?;
    Ok(reply::Entity::update_many()
        .filter(reply::Column::Id.eq(claim.row.id))
        .filter(reply::Column::ArtifactSha256.eq(&claim.row.artifact_sha256))
        .filter(reply::Column::DispatchToken.eq(dispatch))
        .filter(reply::Column::State.is_in([DISPATCHED, UNKNOWN]))
        .col_expr(reply::Column::State, Expr::value(DELIVERED))
        .col_expr(reply::Column::CommentId, Expr::value(comment_id))
        .col_expr(reply::Column::Reason, Expr::value(None::<String>))
        .col_expr(reply::Column::ClaimToken, Expr::value(None::<String>))
        .col_expr(reply::Column::OwnerNode, Expr::value(None::<String>))
        .col_expr(reply::Column::LeaseExpiresAtMs, Expr::value(None::<i64>))
        .col_expr(reply::Column::UpdatedAtMs, Expr::value(now_ms))
        .exec(db)
        .await?
        .rows_affected
        == 1)
}

pub async fn cancel(
    db: &impl ConnectionTrait,
    actor: &str,
    artifact: &str,
    now_ms: i64,
) -> Result<bool, AssistantStoreError> {
    let Some(row) = load(db, actor, artifact).await? else {
        return Ok(false);
    };
    let state = match row.state.as_str() {
        INTERNAL | READY | CLAIMED => NOT_SENT,
        DISPATCHED | UNKNOWN => UNKNOWN,
        _ => return Ok(false),
    };
    Ok(reply::Entity::update_many()
        .filter(reply::Column::Id.eq(row.id))
        .filter(reply::Column::State.eq(&row.state))
        .filter(reply::Column::Attempt.eq(row.attempt))
        .filter(reply::Column::UpdatedAtMs.eq(row.updated_at_ms))
        .col_expr(reply::Column::State, Expr::value(state))
        .col_expr(reply::Column::Reason, Expr::value("cancelled"))
        .col_expr(reply::Column::ClaimToken, Expr::value(None::<String>))
        .col_expr(reply::Column::OwnerNode, Expr::value(None::<String>))
        .col_expr(reply::Column::LeaseExpiresAtMs, Expr::value(None::<i64>))
        .col_expr(reply::Column::UpdatedAtMs, Expr::value(now_ms))
        .exec(db)
        .await?
        .rows_affected
        == 1)
}

fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
fn invalid() -> AssistantStoreError {
    AssistantStoreError::Invalid("Invalid frozen issue delivery")
}

#[cfg(test)]
mod tests;
