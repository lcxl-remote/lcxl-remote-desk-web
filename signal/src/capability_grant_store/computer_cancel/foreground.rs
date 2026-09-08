//! Stop an original dispatched action without inventing a background promotion.
use super::*;

fn intent(
    outbox: &agent_capability_dispatch_outbox::Model,
    work: &agent_action_item::Model,
) -> Result<Option<CancelIntent>, DbErr> {
    let Some(raw) = &outbox.computer_cancel_json else {
        return Ok(None);
    };
    let value: CancelIntent = serde_json::from_str(raw).map_err(|_| invalid())?;
    if !valid_request_id(&value.request_id)
        || value.reason_sha256.len() != 64
        || !value
            .reason_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || work.cancel_requested_at != Some(timestamp(value.requested_at_unix_ms)?)
        || work.cancel_requested_by.as_deref() != Some(work.actor_id.as_str())
        || work.cancel_generation.as_deref() != Some(outbox.dispatch_id.as_str())
        || work.execution_id != work.cancel_generation
        || timestamp(value.requested_at_unix_ms)? < outbox.created_at
        || value
            .observed_at_unix_ms
            .is_some_and(|at| at < value.requested_at_unix_ms)
    {
        return Err(invalid());
    }
    Ok(Some(value))
}

pub(super) fn candidate(
    outbox: &agent_capability_dispatch_outbox::Model,
    work: &agent_action_item::Model,
    payload: &CapabilityDispatchPayload,
) -> Result<Option<CancelCandidate>, DbErr> {
    let Some(intent) = intent(outbox, work)? else {
        return Ok(None);
    };
    if intent.observed_at_unix_ms.is_some()
        || work.manual_resolved_at.is_some()
        || !matches!(
            work.status.as_str(),
            CAPABILITY_WORK_DISPATCHING | CAPABILITY_WORK_OUTCOME_UNKNOWN
        )
        || super::super::computer_completion::terminal_result(outbox, work.clone(), payload)?
            .is_some()
    {
        return Ok(None);
    }
    let binding = bound(outbox, work, payload)?;
    Ok(Some(CancelCandidate {
        work_id: work.id,
        connection_id: binding.connection_id,
        audience: work.target_device_id.clone(),
        actor_id: work.actor_id.clone(),
        action_request_id: payload.call_id.clone(),
        execution_generation: outbox.dispatch_id.clone(),
    }))
}

#[expect(
    clippy::too_many_arguments,
    reason = "Validate the original dispatch, live connection, audience and terminal receipt together under the same transaction"
)]
pub(super) async fn accept(
    txn: &DatabaseTransaction,
    outbox: &agent_capability_dispatch_outbox::Model,
    work: &agent_action_item::Model,
    payload: &CapabilityDispatchPayload,
    connection: &str,
    audience: &str,
    state: &ComputerActionStateReport,
    now: u64,
) -> Result<bool, DbErr> {
    let binding = bound(outbox, work, payload)?;
    if state.work_id != work.id.to_string()
        || state.action_request_id != payload.call_id
        || binding.connection_id != connection
        || work.target_device_id != audience
    {
        return Err(invalid());
    }
    let mut value = intent(outbox, work)?.ok_or_else(invalid)?;
    if value.observed_at_unix_ms.is_some() {
        return Ok(false);
    }
    if now < value.requested_at_unix_ms {
        return Err(invalid());
    }
    value.observed_at_unix_ms = Some(now);
    let mut active: agent_capability_dispatch_outbox::ActiveModel = outbox.clone().into();
    active.computer_cancel_json = Set(Some(serde_json::to_string(&value).map_err(|_| invalid())?));
    active.update(txn).await?;
    Ok(true)
}

impl SignalCapabilityGrantStore {
    pub(crate) async fn request_computer_execution_cancel(
        &self,
        task: &str,
        run: &str,
        actor: &str,
        device: &str,
        request_id: &str,
        reason: &str,
    ) -> Result<bool, DbErr> {
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
            return Ok(false);
        };
        if work.conversation_id != run || work.actor_id != actor || work.target_device_id != device
        {
            return Err(invalid());
        }
        let (outbox, work, payload) =
            original_on(&txn, work.execution_id.as_deref().ok_or_else(invalid)?).await?;
        if outbox.computer_background_json.is_some() {
            txn.rollback().await?;
            return self
                .request_computer_background_cancel(task, run, actor, device, request_id, reason)
                .await
                .map(|value| value.is_some());
        }
        bound(&outbox, &work, &payload)?;
        let digest = format!("{:x}", Sha256::digest(reason.as_bytes()));
        if let Some(old) = intent(&outbox, &work)? {
            if old.request_id != request_id || old.reason_sha256 != digest {
                return Err(invalid());
            }
            txn.commit().await?;
            return Ok(true);
        }
        if work.cancel_requested_at.is_some()
            || work.manual_resolved_at.is_some()
            || !matches!(
                work.status.as_str(),
                CAPABILITY_WORK_DISPATCHING | CAPABILITY_WORK_OUTCOME_UNKNOWN
            )
            || work.result_json.is_some()
        {
            return Ok(false);
        }
        let now = u64::try_from(Utc::now().timestamp_millis()).map_err(|_| invalid())?;
        if timestamp(now)? < outbox.created_at {
            return Err(invalid());
        }
        let value = CancelIntent {
            request_id: request_id.into(),
            reason_sha256: digest,
            requested_at_unix_ms: now,
            observed_at_unix_ms: None,
        };
        let mut active: agent_action_item::ActiveModel = work.into();
        active.cancel_requested_at = Set(Some(timestamp(now)?));
        active.cancel_requested_by = Set(Some(actor.into()));
        active.cancel_generation = Set(Some(outbox.dispatch_id.clone()));
        active.updated_at = Set(timestamp(now)?);
        active.update(&txn).await?;
        let mut active: agent_capability_dispatch_outbox::ActiveModel = outbox.into();
        active.computer_cancel_json =
            Set(Some(serde_json::to_string(&value).map_err(|_| invalid())?));
        active.update(&txn).await?;
        txn.commit().await?;
        Ok(true)
    }
}
