//! Transactional calendar materialization and lease claims.
use super::authority::authority_now;
use super::{ScheduleStoreError, digest, json};
use crate::entity::{agent_schedule as entity, agent_schedule_run as run};
use desk_agent_protocol::schedule::policy::ScheduleBudgetPolicy;
use desk_diagnose_core::schedule::{
    SCHEDULE_CALC_VERSION,
    due::{due_window, start_deadline},
    lifecycle::FailureState,
    parse_json,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
    sea_query::{Alias, Expr, Query},
};

pub async fn database_now<C: ConnectionTrait>(db: &C) -> Result<i64, ScheduleStoreError> {
    let query = Query::select()
        .expr_as(Expr::current_timestamp(), Alias::new("now"))
        .to_owned();
    let row = db
        .query_one(&query)
        .await?
        .ok_or(ScheduleStoreError::Invalid)?;
    Ok(row
        .try_get::<chrono::DateTime<chrono::Utc>>("", "now")?
        .timestamp_millis())
}

/// The budget policy an occurrence freezes when it is claimed. The claim reads it
/// after the task-row fence so a host can lock the policy row in that order.
#[async_trait::async_trait]
pub trait BudgetPolicySource<C: ConnectionTrait>: Sync {
    async fn read(&self, txn: &C) -> Result<ScheduleBudgetPolicy, DbErr>;
}

/// Manual execution has its own idempotency identity and never moves the calendar.
/// A set revision binds a management request to the version the owner reviewed.
pub async fn enqueue_manual_on<C: ConnectionTrait>(
    txn: &C,
    owner: i32,
    schedule_id: &str,
    client_key: &str,
    expected_revision: Option<i64>,
) -> Result<run::Model, ScheduleStoreError> {
    if owner <= 0
        || client_key.is_empty()
        || client_key.len() > 256
        || client_key.chars().any(char::is_control)
    {
        return Err(ScheduleStoreError::Invalid);
    }
    let now = database_now(txn).await?;
    let task = entity::Entity::find()
        .filter(entity::Column::ScheduleId.eq(schedule_id))
        .filter(entity::Column::OwnerUserId.eq(owner))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let identity = digest(&json(&(schedule_id, "manual", client_key))?);
    if let Some(existing) = run::Entity::find()
        .filter(run::Column::OccurrenceIdentity.eq(&identity))
        .one(txn)
        .await?
    {
        if existing.owner_user_id != owner
            || existing.schedule_id != schedule_id
            || expected_revision.is_some_and(|revision| existing.schedule_revision != revision)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        return Ok(existing);
    }
    if expected_revision.is_some_and(|revision| revision != task.revision) {
        return Err(ScheduleStoreError::Conflict);
    }
    let failures: FailureState =
        serde_json::from_str(&task.failure_state_json).map_err(|_| ScheduleStoreError::Invalid)?;
    if task.kind != "fresh_task"
        || task.status != "active"
        || task.active_run_id.is_some()
        || !failures.pause_reasons.is_empty()
        || task.calc_version != SCHEDULE_CALC_VERSION
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let run_id = format!("schedule-run-{identity}");
    let changed = entity::Entity::update_many()
        .set(entity::ActiveModel {
            active_run_id: Set(Some(run_id.clone())),
            updated_at: Set(now),
            revision: Set(task
                .revision
                .checked_add(1)
                .ok_or(ScheduleStoreError::Invalid)?),
            ..Default::default()
        })
        .filter(entity::Column::Id.eq(task.id))
        .filter(entity::Column::Revision.eq(task.revision))
        .filter(entity::Column::ActiveRunId.is_null())
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    let grace = u32::try_from(task.grace_seconds).map_err(|_| ScheduleStoreError::Invalid)?;
    run::Entity::insert(run::ActiveModel {
        run_id: Set(run_id.clone()),
        schedule_id: Set(task.schedule_id.clone()),
        owner_user_id: Set(owner),
        occurrence_identity: Set(identity),
        source: Set("manual".into()),
        scheduled_at: Set(None),
        requested_at: Set(now),
        schedule_revision: Set(task.revision),
        recovery_epoch: Set(
            i64::try_from(failures.recovery_epoch).map_err(|_| ScheduleStoreError::Invalid)?
        ),
        task_snapshot_json: Set(json(&task)?),
        conversation_id: Set(run_id.clone()),
        turn_id: Set(format!("{run_id}-turn")),
        status: Set("queued".into()),
        start_deadline: Set(start_deadline(now, grace).map_err(|_| ScheduleStoreError::Invalid)?),
        lease_epoch: Set(0),
        lease_owner: Set(None),
        lease_deadline: Set(None),
        attempt: Set(0),
        failure_accounted: Set(false),
        error_kind: Set(None),
        result_ref: Set(None),
        missed_count: Set(0),
        started_at: Set(None),
        finished_at: Set(None),
        receipts_reconciled_at: Set(None),
        outcome_review_json: Set(None),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    })
    .exec_without_returning(txn)
    .await?;
    let result = run::Entity::find()
        .filter(run::Column::RunId.eq(&run_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    Ok(result)
}

pub async fn due_candidates<C: ConnectionTrait>(
    db: &C,
    after_id: i64,
    limit: u64,
) -> Result<Vec<entity::Model>, ScheduleStoreError> {
    if after_id < 0 || limit == 0 || limit > 100 {
        return Err(ScheduleStoreError::Invalid);
    }
    let now = database_now(db).await?;
    Ok(entity::Entity::find()
        .filter(entity::Column::Status.eq("active"))
        .filter(entity::Column::NextRunAt.lte(now))
        .filter(entity::Column::Id.gt(after_id))
        .order_by_asc(entity::Column::Id)
        .limit(limit)
        .all(db)
        .await?)
}

/// Only the row CAS can create an occurrence and advance its calendar cursor.
pub async fn materialize_due_on<C: ConnectionTrait>(
    txn: &C,
    schedule_id: &str,
    expected_revision: i64,
) -> Result<Option<run::Model>, ScheduleStoreError> {
    let now = database_now(txn).await?;
    let row = entity::Entity::find()
        .filter(entity::Column::ScheduleId.eq(schedule_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if row.revision != expected_revision {
        return Err(ScheduleStoreError::Conflict);
    }
    if row.status != "active" || row.next_run_at.is_none_or(|at| at > now) {
        return Ok(None);
    }
    if row.calc_version != SCHEDULE_CALC_VERSION {
        return Err(ScheduleStoreError::Invalid);
    }
    let failures: FailureState =
        serde_json::from_str(&row.failure_state_json).map_err(|_| ScheduleStoreError::Invalid)?;
    if !failures.pause_reasons.is_empty() {
        return Ok(None);
    }
    let spec = parse_json(&row.spec_json).map_err(|_| ScheduleStoreError::Invalid)?;
    let first = row.next_run_at.ok_or(ScheduleStoreError::Invalid)?;
    let window = due_window(
        &spec,
        first.checked_sub(1).ok_or(ScheduleStoreError::Invalid)?,
        now,
    )
    .map_err(|_| ScheduleStoreError::Invalid)?;
    let Some(latest) = window.latest_at else {
        return Ok(None);
    };
    let grace = u32::try_from(row.grace_seconds).map_err(|_| ScheduleStoreError::Invalid)?;
    let deadline = start_deadline(latest, grace).map_err(|_| ScheduleStoreError::Invalid)?;
    let status = if row.active_run_id.is_some() {
        "skipped_overlap"
    } else if now >= deadline {
        "missed"
    } else {
        "queued"
    };
    let identity = digest(&json(&(&row.schedule_id, "calendar", latest))?);
    let run_id = format!("schedule-run-{identity}");
    let conversation_id = match row.kind.as_str() {
        "fresh_task" => run_id.clone(),
        "conversation_resume" => row
            .source_conversation_id
            .clone()
            .ok_or(ScheduleStoreError::Invalid)?,
        _ => return Err(ScheduleStoreError::Invalid),
    };
    let revision = row
        .revision
        .checked_add(1)
        .ok_or(ScheduleStoreError::Invalid)?;
    let terminal = status != "queued";
    let next_status = if window.next_at.is_some() {
        "active"
    } else if terminal {
        "completed"
    } else {
        "triggered"
    };
    let changed = entity::Entity::update_many()
        .set(entity::ActiveModel {
            revision: Set(revision),
            next_run_at: Set(window.next_at),
            recurrence_cursor_at: Set(Some(latest)),
            status: Set(next_status.into()),
            active_run_id: Set(if terminal {
                row.active_run_id.clone()
            } else {
                Some(run_id.clone())
            }),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(entity::Column::Id.eq(row.id))
        .filter(entity::Column::Revision.eq(expected_revision))
        .filter(entity::Column::Status.eq("active"))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    run::Entity::insert(run::ActiveModel {
        run_id: Set(run_id.clone()),
        schedule_id: Set(row.schedule_id.clone()),
        owner_user_id: Set(row.owner_user_id),
        occurrence_identity: Set(identity),
        source: Set("calendar".into()),
        scheduled_at: Set(Some(latest)),
        requested_at: Set(now),
        schedule_revision: Set(row.revision),
        recovery_epoch: Set(
            i64::try_from(failures.recovery_epoch).map_err(|_| ScheduleStoreError::Invalid)?
        ),
        task_snapshot_json: Set(json(&row)?),
        conversation_id: Set(conversation_id),
        turn_id: Set(format!("{run_id}-turn")),
        status: Set(status.into()),
        start_deadline: Set(deadline),
        lease_epoch: Set(0),
        lease_owner: Set(None),
        lease_deadline: Set(None),
        attempt: Set(0),
        failure_accounted: Set(terminal),
        error_kind: Set(if status == "missed" {
            Some("misfire".into())
        } else {
            None
        }),
        result_ref: Set(None),
        missed_count: Set(
            i64::try_from(window.due_count - 1).map_err(|_| ScheduleStoreError::Invalid)?
        ),
        started_at: Set(None),
        finished_at: Set(terminal.then_some(now)),
        receipts_reconciled_at: Set(None),
        outcome_review_json: Set(None),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    })
    .exec_without_returning(txn)
    .await?;
    let result = run::Entity::find()
        .filter(run::Column::RunId.eq(&run_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    Ok(Some(result))
}

/// The caller must roll back on any error and commit only after pairing the
/// claimed occurrence with its fresh session and current authorization.
/// This function never commits and grants no permission to execute tools.
pub async fn claim_queued_on<C: ConnectionTrait, P: BudgetPolicySource<C>>(
    txn: &C,
    run_id: &str,
    node_id: &str,
    lease_seconds: u32,
    budget_policy: &P,
) -> Result<run::Model, ScheduleStoreError> {
    if node_id.is_empty() || node_id.len() > 256 || !(30..=300).contains(&lease_seconds) {
        return Err(ScheduleStoreError::Invalid);
    }
    let now = database_now(txn).await?;
    let work = run::Entity::find()
        .filter(run::Column::RunId.eq(run_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if !matches!(work.status.as_str(), "queued" | "waiting_device")
        || now >= work.start_deadline
        || work.started_at.is_some()
        || work.finished_at.is_some()
        || work.cancel_requested_at.is_some()
        || work.failure_accounted
        || work.attempt != 0
        || work.lease_epoch != 0
        || work.lease_owner.is_some()
        || work.lease_deadline.is_some()
        || work.error_kind.is_some()
        || work.result_ref.is_some()
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let schedule = entity::Entity::find()
        .filter(entity::Column::ScheduleId.eq(&work.schedule_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if schedule.kind != "fresh_task" {
        return Err(ScheduleStoreError::Invalid);
    }
    if work.conversation_id != work.run_id || work.turn_id != format!("{}-turn", work.run_id) {
        return Err(ScheduleStoreError::Conflict);
    }
    super::retrieval_consent(&schedule)?;
    let snapshot: entity::Model =
        serde_json::from_str(&work.task_snapshot_json).map_err(|_| ScheduleStoreError::Invalid)?;
    if work.owner_user_id != schedule.owner_user_id
        || snapshot.schedule_id != schedule.schedule_id
        || snapshot.owner_user_id != schedule.owner_user_id
        || snapshot.kind != schedule.kind
        || snapshot.revision != work.schedule_revision
        || snapshot.task_revision != schedule.task_revision
        || snapshot.retrieval_consent_json != schedule.retrieval_consent_json
        || snapshot.target_device_id != schedule.target_device_id
        || snapshot.prompt != schedule.prompt
        || snapshot.locale != schedule.locale
        || snapshot.model_id != schedule.model_id
        || schedule.calc_version != SCHEDULE_CALC_VERSION
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let failures: FailureState = serde_json::from_str(&schedule.failure_state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    if !matches!(schedule.status.as_str(), "active" | "triggered")
        || !failures.pause_reasons.is_empty()
        || schedule.active_run_id.as_deref() != Some(run_id)
        || i64::try_from(failures.recovery_epoch).ok() != Some(work.recovery_epoch)
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let touched = entity::Entity::update_many()
        .set(entity::ActiveModel {
            revision: Set(schedule
                .revision
                .checked_add(1)
                .ok_or(ScheduleStoreError::Invalid)?),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(entity::Column::Id.eq(schedule.id))
        .filter(entity::Column::Revision.eq(schedule.revision))
        .exec(txn)
        .await?;
    if touched.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    // Waiting for the task fence can outlive the start window. Re-read
    // the occurrence as cancellation and settlement may have changed it.
    let now = authority_now(txn).await?;
    if now >= work.start_deadline
        || run::Entity::find_by_id(work.id).one(txn).await?.as_ref() != Some(&work)
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let policy = budget_policy.read(txn).await?;
    let changed = run::Entity::update_many()
        .set(run::ActiveModel {
            status: Set("running".into()),
            // Later policy changes do not affect this occurrence.
            budget_policy_json: Set(Some(json(&policy)?)),
            lease_owner: Set(Some(node_id.into())),
            lease_epoch: Set(work
                .lease_epoch
                .checked_add(1)
                .ok_or(ScheduleStoreError::Invalid)?),
            lease_deadline: Set(Some(
                start_deadline(now, lease_seconds).map_err(|_| ScheduleStoreError::Invalid)?,
            )),
            attempt: Set(work
                .attempt
                .checked_add(1)
                .ok_or(ScheduleStoreError::Invalid)?),
            started_at: Set(Some(now)),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(run::Column::Id.eq(work.id))
        .filter(run::Column::Status.eq(&work.status))
        .filter(run::Column::LeaseEpoch.eq(work.lease_epoch))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    let result = run::Entity::find_by_id(work.id)
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity_table::EntityTableExt;
    use desk_agent_protocol::schedule::policy::TaskBudget;
    use sea_orm::{Database, DatabaseConnection, Schema};
    use std::sync::Mutex;

    /// Records what the claim transaction shows when the policy is read.
    struct Recording {
        policy: ScheduleBudgetPolicy,
        seen: Mutex<Option<(i64, String)>>,
    }

    #[async_trait::async_trait]
    impl BudgetPolicySource<DatabaseConnection> for Recording {
        async fn read(&self, txn: &DatabaseConnection) -> Result<ScheduleBudgetPolicy, DbErr> {
            let task = entity::Entity::find().one(txn).await?.unwrap();
            let work = run::Entity::find().one(txn).await?.unwrap();
            *self.seen.lock().unwrap() = Some((task.revision, work.status));
            Ok(self.policy.clone())
        }
    }

    async fn queued_task() -> (DatabaseConnection, entity::Model, run::Model) {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let schema = Schema::new(db.get_database_backend());
        for statement in [
            schema.entity_table(entity::Entity),
            schema.entity_table(run::Entity),
        ] {
            db.execute(&statement).await.unwrap();
        }
        let now = database_now(&db).await.unwrap();
        entity::Entity::insert(entity::ActiveModel {
            schedule_id: Set("task-1".into()),
            owner_user_id: Set(1),
            target_device_id: Set("device".into()),
            kind: Set("fresh_task".into()),
            status: Set("active".into()),
            title: Set("Check".into()),
            prompt: Set("Check disk usage".into()),
            locale: Set(None),
            model_id: Set(None),
            spec_json: Set("{}".into()),
            calc_version: Set(SCHEDULE_CALC_VERSION.into()),
            revision: Set(1),
            task_revision: Set(1),
            next_run_at: Set(None),
            recurrence_cursor_at: Set(None),
            grace_seconds: Set(300),
            creation_source: Set("owner".into()),
            creation_identity: Set("create-1".into()),
            creation_payload_sha256: Set(digest("create-1")),
            source_conversation_id: Set(None),
            requirement_revision: Set(None),
            retrieval_consent_json: Set(None),
            prompt_retention_json: Set(None),
            proposal_conversation_id: Set(None),
            failure_state_json: Set(json(&FailureState::default()).unwrap()),
            active_run_id: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        })
        .exec_without_returning(&db)
        .await
        .unwrap();
        let queued = enqueue_manual_on(&db, 1, "task-1", "manual-1", Some(1))
            .await
            .unwrap();
        let task = entity::Entity::find().one(&db).await.unwrap().unwrap();
        (db, task, queued)
    }

    #[tokio::test]
    async fn claims_read_the_budget_policy_after_the_task_fence_and_freeze_it() {
        let (db, task, queued) = queued_task().await;
        let policy = ScheduleBudgetPolicy {
            schema_version: 1,
            revision: 7,
            maximum: TaskBudget {
                max_runs_per_utc_day: 3,
                max_calls_per_run: 4,
                max_model_tokens_per_run: 5_000,
                max_runtime_seconds: 120,
            },
        };
        let source = Recording {
            policy: policy.clone(),
            seen: Mutex::new(None),
        };
        let claimed = claim_queued_on(&db, &queued.run_id, "node-1", 60, &source)
            .await
            .unwrap();
        // The task row was fenced and the occurrence was not yet claimed.
        assert_eq!(
            source.seen.lock().unwrap().clone(),
            Some((task.revision + 1, "queued".into()))
        );
        assert_eq!(claimed.status, "running");
        assert_eq!(claimed.lease_owner.as_deref(), Some("node-1"));
        assert_eq!(
            claimed.budget_policy_json.as_deref(),
            Some(json(&policy).unwrap().as_str())
        );
        // A second claim cannot reuse the occurrence.
        assert!(matches!(
            claim_queued_on(&db, &queued.run_id, "node-2", 60, &source).await,
            Err(ScheduleStoreError::Conflict)
        ));
    }
}
