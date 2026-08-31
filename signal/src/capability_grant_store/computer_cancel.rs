//! Durable stop intent on the original accepted execution, never a new action.

use super::computer_background::{Promotion, bound, task_on};
use super::computer_binding::original_on;
use super::*;
use desk_agent_protocol::computer_use::{ComputerActionPhase, ComputerActionStateReport};
use desk_diagnose_core::dynamic_run::BackgroundTaskRecord;
use sea_orm::{DatabaseTransaction, QueryOrder, QuerySelect};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CancelIntent {
    pub request_id: String,
    reason_sha256: String,
    requested_at_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    observed_at_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CancelCandidate {
    pub work_id: i64,
    pub connection_id: String,
    pub audience: String,
    pub actor_id: String,
    pub action_request_id: String,
    pub execution_generation: String,
}

fn invalid() -> DbErr {
    DbErr::Custom("original background stop is invalid or inaccessible".into())
}

fn valid_request_id(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

pub(crate) fn wire_request_id(work: i64, generation: &str) -> String {
    format!(
        "provider-cancel-{work}-{:x}",
        Sha256::digest(generation.as_bytes())
    )
}

pub(super) fn validate_intent(
    intent: &Option<CancelIntent>,
    work: &agent_action_item::Model,
    promotion: &Promotion,
) -> Result<(), DbErr> {
    let Some(intent) = intent else {
        if work.cancel_requested_at.is_some()
            || work.cancel_requested_by.is_some()
            || work.cancel_generation.is_some()
        {
            return Err(invalid());
        }
        return Ok(());
    };
    if !valid_request_id(&intent.request_id)
        || intent.reason_sha256.len() != 64
        || !intent
            .reason_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || intent.requested_at_unix_ms < promotion.promoted_at_unix_ms
        || work.cancel_requested_at != Some(timestamp(intent.requested_at_unix_ms)?)
        || work.cancel_requested_by.as_deref() != Some(&work.actor_id)
        || work.cancel_generation != work.execution_id
        || work.cancel_generation.is_none()
        || intent
            .observed_at_unix_ms
            .is_some_and(|at| at < intent.requested_at_unix_ms)
    {
        return Err(invalid());
    }
    Ok(())
}

// Acquire SQLite's writer reservation before reading facts. Concurrent owners,
// ACKs and completions serialize without upgrading a stale deferred snapshot.
async fn lock_task(txn: &DatabaseTransaction, task: &str) -> Result<(), DbErr> {
    agent_action_item::Entity::update_many()
        .filter(agent_action_item::Column::ActionRequestId.eq(task))
        .col_expr(
            agent_action_item::Column::Id,
            Expr::col(agent_action_item::Column::Id),
        )
        .exec(txn)
        .await?;
    Ok(())
}

async fn save_promotion(
    txn: &DatabaseTransaction,
    outbox: agent_capability_dispatch_outbox::Model,
    promotion: &Promotion,
) -> Result<(), DbErr> {
    let mut active: agent_capability_dispatch_outbox::ActiveModel = outbox.into();
    active.computer_background_json = Set(Some(
        serde_json::to_string(promotion).map_err(|_| invalid())?,
    ));
    // Do not renew the dispatch, acceptance or promotion timestamps.
    active.update(txn).await?;
    Ok(())
}

impl SignalCapabilityGrantStore {
    /// `None` means this is not an original Provider task; the caller may check
    /// the legacy store. A known but mismatched task never falls through.
    pub(crate) async fn request_computer_background_cancel(
        &self,
        task: &str,
        run: &str,
        actor: &str,
        device: &str,
        request_id: &str,
        reason: &str,
    ) -> Result<Option<BackgroundTaskRecord>, DbErr> {
        if !valid_request_id(request_id) || reason.len() > 4096 {
            return Err(invalid());
        }
        let txn = self.db.begin().await?;
        lock_task(&txn, task).await?;
        let Some(work) = agent_action_item::Entity::find()
            .filter(agent_action_item::Column::ActionRequestId.eq(task))
            .filter(agent_action_item::Column::Kind.eq(CAPABILITY_WORK_KIND))
            .one(&txn)
            .await?
        else {
            return Ok(None);
        };
        if work.conversation_id != run || work.actor_id != actor || work.target_device_id != device
        {
            return Err(invalid());
        }
        let now = u64::try_from(Utc::now().timestamp_millis()).map_err(|_| invalid())?;
        let record = task_on(&txn, &work, now).await?.ok_or_else(invalid)?;
        if !record.supports_cancel {
            return Err(invalid());
        }
        let outbox = agent_capability_dispatch_outbox::Entity::find()
            .filter(agent_capability_dispatch_outbox::Column::WorkId.eq(work.id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let mut promotion: Promotion = serde_json::from_str(
            outbox
                .computer_background_json
                .as_deref()
                .ok_or_else(invalid)?,
        )
        .map_err(|_| invalid())?;
        let digest = format!("{:x}", Sha256::digest(reason.as_bytes()));
        if let Some(previous) = &promotion.cancel {
            if previous.request_id != request_id || previous.reason_sha256 != digest {
                return Err(invalid());
            }
            txn.commit().await?;
            return Ok(Some(record));
        }
        // Record even a terminal no-op, so replay cannot change its request bytes.
        promotion.cancel = Some(CancelIntent {
            request_id: request_id.into(),
            reason_sha256: digest,
            requested_at_unix_ms: now,
            observed_at_unix_ms: None,
        });
        let mut active: agent_action_item::ActiveModel = work.clone().into();
        active.cancel_requested_at = Set(Some(timestamp(now)?));
        active.cancel_requested_by = Set(Some(actor.into()));
        active.cancel_generation = Set(work.execution_id);
        // The result timestamp belongs to its receipt, not this later stop.
        if work.result_json.is_none() {
            active.updated_at = Set(timestamp(now)?);
        }
        let updated = active.update(&txn).await?;
        save_promotion(&txn, outbox, &promotion).await?;
        let record = task_on(&txn, &updated, now).await?.ok_or_else(invalid)?;
        txn.commit().await?;
        Ok(Some(record))
    }

    pub(crate) async fn computer_cancel_candidate(
        &self,
        work_id: i64,
    ) -> Result<Option<CancelCandidate>, DbErr> {
        let txn = self.db.begin().await?;
        let outbox = agent_capability_dispatch_outbox::Entity::find()
            .filter(agent_capability_dispatch_outbox::Column::WorkId.eq(work_id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let (outbox, work, payload) = original_on(&txn, &outbox.dispatch_id).await?;
        let now = u64::try_from(Utc::now().timestamp_millis()).map_err(|_| invalid())?;
        if task_on(&txn, &work, now).await?.is_none() {
            return Ok(None);
        }
        let promotion: Promotion = serde_json::from_str(
            outbox
                .computer_background_json
                .as_deref()
                .ok_or_else(invalid)?,
        )
        .map_err(|_| invalid())?;
        if promotion
            .cancel
            .is_none_or(|intent| intent.observed_at_unix_ms.is_some())
            || work.manual_resolved_at.is_some()
            || !matches!(
                work.status.as_str(),
                CAPABILITY_WORK_DISPATCHING | CAPABILITY_WORK_OUTCOME_UNKNOWN
            )
            || super::computer_completion::terminal_result(&outbox, work.clone(), &payload)?
                .is_some()
        {
            return Ok(None);
        }
        let binding = bound(&outbox, &work, &payload)?;
        Ok(Some(CancelCandidate {
            work_id,
            connection_id: binding.connection_id,
            audience: work.target_device_id,
            actor_id: work.actor_id,
            action_request_id: payload.call_id,
            execution_generation: outbox.dispatch_id,
        }))
    }

    /// Bounded keyset scan; corrupt or disconnected earlier tasks cannot starve
    /// later ones. No execution send permit is acquired by this query.
    pub(crate) async fn computer_cancel_page(
        &self,
        after: i64,
    ) -> Result<(Vec<i64>, Option<i64>), DbErr> {
        let rows = agent_action_item::Entity::find()
            .filter(agent_action_item::Column::Id.gt(after))
            .filter(agent_action_item::Column::Kind.eq(CAPABILITY_WORK_KIND))
            .filter(agent_action_item::Column::CancelRequestedAt.is_not_null())
            .filter(
                agent_action_item::Column::Status
                    .is_in([CAPABILITY_WORK_DISPATCHING, CAPABILITY_WORK_OUTCOME_UNKNOWN]),
            )
            .order_by_asc(agent_action_item::Column::Id)
            .limit(32)
            .all(&self.db)
            .await?;
        let next = (rows.len() == 32).then(|| rows.last().unwrap().id);
        Ok((rows.into_iter().map(|row| row.id).collect(), next))
    }

    pub(crate) async fn accept_computer_cancel_state(
        &self,
        connection: &str,
        audience: &str,
        request: &str,
        state: &ComputerActionStateReport,
    ) -> Result<bool, DbErr> {
        let id = state.work_id.parse::<i64>().map_err(|_| invalid())?;
        if id <= 0
            || state.work_id != id.to_string()
            || request != wire_request_id(id, &state.execution_generation)
            || state.phase != ComputerActionPhase::CancelRequested
            || state.result.is_some()
        {
            return Ok(false);
        }
        let txn = self.db.begin().await?;
        lock_task(&txn, &state.action_request_id).await?;
        let (outbox, work, payload) = original_on(&txn, &state.execution_generation).await?;
        let now = u64::try_from(Utc::now().timestamp_millis()).map_err(|_| invalid())?;
        task_on(&txn, &work, now).await?.ok_or_else(invalid)?;
        let binding = bound(&outbox, &work, &payload)?;
        if work.id != id
            || payload.call_id != state.action_request_id
            || binding.connection_id != connection
            || work.target_device_id != audience
        {
            return Err(invalid());
        }
        let mut promotion: Promotion = serde_json::from_str(
            outbox
                .computer_background_json
                .as_deref()
                .ok_or_else(invalid)?,
        )
        .map_err(|_| invalid())?;
        let intent = promotion.cancel.as_mut().ok_or_else(invalid)?;
        if intent.observed_at_unix_ms.is_some() {
            return Ok(false);
        }
        if now < intent.requested_at_unix_ms {
            return Err(invalid());
        }
        intent.observed_at_unix_ms = Some(now);
        save_promotion(&txn, outbox, &promotion).await?;
        txn.commit().await?;
        Ok(true)
    }
}
