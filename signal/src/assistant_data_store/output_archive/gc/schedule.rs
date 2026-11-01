//! Existing retention timers drive these bounded GC passes.
//! Shared storage owns discovery and hints; this host owns each collector transaction.
use super::collect_actor;
use crate::assistant_data_store::{AssistantStoreError, signed};
use desk_assistant_data_store::output_gc;
pub(crate) use output_gc::OutputGcSweep;

pub(crate) fn run_due_once(
    db: &crate::config::connection::DatabaseConnection,
) -> impl std::future::Future<Output = Result<OutputGcSweep, AssistantStoreError>> + Send + '_ {
    desk_diagnose_core::future::send_phase(|| run_due_once_inner(db))
}

async fn run_due_once_inner(
    db: &crate::config::connection::DatabaseConnection,
) -> Result<OutputGcSweep, AssistantStoreError> {
    let now = u64::try_from(chrono::Utc::now().timestamp_millis())
        .map_err(|_| AssistantStoreError::Inaccessible)?;
    let policy = crate::assistant_data_config::policy(db).await;
    run_with_policy(db, now, &policy.limits.processing, 1).await
}

#[cfg(test)]
pub(crate) async fn run_at_with_limit(
    db: &crate::config::connection::DatabaseConnection,
    now: u64,
    maximum: u64,
) -> Result<OutputGcSweep, AssistantStoreError> {
    let policy = crate::assistant_data_config::policy(db).await;
    run_with_policy(
        db,
        now,
        &policy.limits.processing,
        maximum.min(u64::from(
            policy.limits.processing.output_gc_actor_batch_size,
        )) as u32,
    )
    .await
}

async fn run_with_policy(
    db: &crate::config::connection::DatabaseConnection,
    now: u64,
    limits: &desk_agent_protocol::ai_assistant::limits::AssistantProcessingLimits,
    maximum: u32,
) -> Result<OutputGcSweep, AssistantStoreError> {
    let now_signed = signed(now)?;
    let candidates = output_gc::due_actors_on(
        db,
        Some(&crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string()),
        now_signed,
        limits.output_gc_interval_ms,
        maximum,
    )
    .await?;
    let mut outcome = OutputGcSweep::default();
    for (id, actor, previous) in candidates {
        if !output_gc::claim_on(
            db,
            id,
            &actor,
            previous,
            now_signed,
            limits.output_gc_interval_ms,
        )
        .await?
        {
            continue;
        }
        outcome.claimed_actors += 1;
        let collected = collect_actor(db, now, u64::from(limits.output_gc_batch_size)).await;
        match collected {
            Ok(value) => outcome.include(value)?,
            Err(_) => outcome.failed_actors += 1,
        }
    }
    Ok(outcome)
}

#[cfg(test)]
pub(crate) async fn claim_on<C: sea_orm::ConnectionTrait>(
    db: &C,
    id: i64,
    actor: &str,
    previous: Option<i64>,
    now: i64,
) -> Result<bool, AssistantStoreError> {
    output_gc::claim_on(
        db,
        id,
        actor,
        previous,
        now,
        desk_agent_protocol::ai_assistant::limits::AssistantProcessingLimits::default()
            .output_gc_interval_ms,
    )
    .await
}
