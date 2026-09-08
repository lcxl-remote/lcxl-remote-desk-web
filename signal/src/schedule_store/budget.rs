//! Task quotas are reserved with current authority in the caller's dispatch transaction.
use super::authority::authority_now;
use super::publication::{key, valid_digest};
use super::{CurrentTaskAuthority, ScheduleStore, ScheduleStoreError, digest, entity, json};
use crate::entity::agent_task_budget_reservation as ledger;
use sea_orm::{
    ColumnTrait, DatabaseTransaction, EntityTrait, ExprTrait, PaginatorTrait, QueryFilter,
    QuerySelect, Set, sea_query::Expr,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskBudgetKind {
    ToolCall,
    ModelTokens,
}
impl TaskBudgetKind {
    fn name(self) -> &'static str {
        match self {
            Self::ToolCall => "tool_call",
            Self::ModelTokens => "model_tokens",
        }
    }
}

/// Server-derived request. Input digest binds the original exact tool/model request,
/// and units are one tool call or a conservative model token upper bound.
pub struct TaskBudgetRequest<'a> {
    pub owner: i32,
    pub device: &'a str,
    pub run_id: &'a str,
    pub node: &'a str,
    pub lease_epoch: i64,
    pub kind: TaskBudgetKind,
    pub rule_id: Option<&'a str>,
    pub logical_key: &'a str,
    pub input_sha256: &'a str,
    pub units: u64,
}

fn reservation_id(run: &str, kind: &str, key_hash: &str) -> Result<String, ScheduleStoreError> {
    Ok(digest(&json(&(run, kind, key_hash))?))
}

async fn insert(
    txn: &DatabaseTransaction,
    authority: &CurrentTaskAuthority,
    kind: &str,
    logical_key: &str,
    input: &str,
    units: i64,
    rule_id: Option<&str>,
) -> Result<ledger::Model, ScheduleStoreError> {
    let logical_key_sha256 = digest(logical_key);
    let id = reservation_id(&authority.run().run_id, kind, &logical_key_sha256)?;
    ledger::Entity::insert(ledger::ActiveModel {
        reservation_id: Set(id.clone()),
        schedule_id: Set(authority.provenance().schedule_id.clone()),
        run_id: Set(authority.run().run_id.clone()),
        owner_user_id: Set(authority.run().owner_user_id),
        kind: Set(kind.into()),
        rule_id: Set(rule_id.map(str::to_owned)),
        exception_grant_id: Set(None),
        utc_day: Set(authority.verified_at().div_euclid(86_400_000)),
        logical_key_sha256: Set(logical_key_sha256),
        input_sha256: Set(input.into()),
        authority_sha256: Set(digest(&json(authority.provenance())?)),
        reserved_units: Set(units),
        charged_units: Set(units),
        state: Set("reserved".into()),
        receipt_sha256: Set(None),
        version: Set(1),
        created_at: Set(authority.verified_at()),
        settled_at: Set(None),
        ..Default::default()
    })
    .exec_without_returning(txn)
    .await?;
    ledger::Entity::find()
        .filter(ledger::Column::ReservationId.eq(id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)
}

fn validate_row(row: &ledger::Model) -> Result<(), ScheduleStoreError> {
    match (row.kind.as_str(), row.rule_id.as_deref()) {
        ("tool_call", Some(rule)) => key(rule)?,
        ("model_tokens" | "run", None) => {}
        _ => return Err(ScheduleStoreError::Invalid),
    }
    if let Some(grant) = &row.exception_grant_id {
        if row.kind != "tool_call" {
            return Err(ScheduleStoreError::Invalid);
        }
        key(grant)?;
    }
    if row.reservation_id != reservation_id(&row.run_id, &row.kind, &row.logical_key_sha256)?
        || !valid_digest(&row.logical_key_sha256)
        || !valid_digest(&row.input_sha256)
        || !valid_digest(&row.authority_sha256)
        || row.reserved_units <= 0
        || row.charged_units < 0
        || !matches!(row.kind.as_str(), "run" | "tool_call" | "model_tokens")
    {
        return Err(ScheduleStoreError::Invalid);
    }
    let valid_state = match row.state.as_str() {
        "reserved" => {
            row.version == 1
                && row.charged_units == row.reserved_units
                && row.receipt_sha256.is_none()
                && row.settled_at.is_none()
        }
        "settled" | "overrun" => {
            row.kind == "model_tokens"
                && row.version == 2
                && row.receipt_sha256.as_deref().is_some_and(valid_digest)
                && row.settled_at.is_some_and(|at| at >= row.created_at)
                && ((row.state == "settled" && row.charged_units <= row.reserved_units)
                    || (row.state == "overrun" && row.charged_units > row.reserved_units))
        }
        _ => false,
    };
    if !valid_state || (row.kind != "model_tokens" && row.reserved_units != 1) {
        return Err(ScheduleStoreError::Invalid);
    }
    Ok(())
}

/// Reserve one UTC-day occurrence without allocating a model/tool call.
/// The caller holds the current task fence and must roll back on any error.
pub(super) async fn reserve_run_budget(
    txn: &DatabaseTransaction,
    authority: &CurrentTaskAuthority,
) -> Result<(), ScheduleStoreError> {
    let parent_digest = digest(&json(authority.provenance())?);
    let base = ledger::Entity::find()
        .filter(ledger::Column::RunId.eq(&authority.run().run_id))
        .filter(ledger::Column::OwnerUserId.eq(authority.run().owner_user_id));
    let run_id = reservation_id(authority.run().run_id.as_str(), "run", &digest("run"))?;
    if let Some(existing) = base
        .clone()
        .filter(ledger::Column::ReservationId.eq(run_id))
        .one(txn)
        .await?
    {
        validate_row(&existing)?;
        if existing.schedule_id != authority.provenance().schedule_id
            || existing.input_sha256 != parent_digest
            || existing.authority_sha256 != parent_digest
        {
            return Err(ScheduleStoreError::Conflict);
        }
    } else {
        if base.clone().count(txn).await? > 0 {
            return Err(ScheduleStoreError::Invalid);
        }
        let count = ledger::Entity::find()
            .filter(ledger::Column::ScheduleId.eq(&authority.provenance().schedule_id))
            .filter(ledger::Column::Kind.eq("run"))
            .filter(ledger::Column::UtcDay.eq(authority.verified_at().div_euclid(86_400_000)))
            .count(txn)
            .await?;
        if count >= u64::from(authority.contract().contract().budget.max_runs_per_utc_day) {
            return Err(ScheduleStoreError::BudgetExceeded);
        }
        insert(txn, authority, "run", "run", &parent_digest, 1, None).await?;
    }
    Ok(())
}

impl ScheduleStore {
    /// This joins run admission, call allocation and the current parent fence. It is NOT
    /// send authorization. The caller must add policy/input checks and the exact grant/work
    /// reservation before commit, and roll back the entire transaction on any error.
    pub async fn reserve_task_budget(
        txn: &DatabaseTransaction,
        request: &TaskBudgetRequest<'_>,
    ) -> Result<ledger::Model, ScheduleStoreError> {
        key(request.logical_key)?;
        match (request.kind, request.rule_id) {
            (TaskBudgetKind::ToolCall, Some(rule)) => key(rule)?,
            (TaskBudgetKind::ModelTokens, None) => {}
            _ => return Err(ScheduleStoreError::Invalid),
        }
        if !valid_digest(request.input_sha256)
            || request.units == 0
            || (request.kind == TaskBudgetKind::ToolCall && request.units != 1)
        {
            return Err(ScheduleStoreError::Invalid);
        }
        let units = i64::try_from(request.units).map_err(|_| ScheduleStoreError::Invalid)?;
        let authority = Self::lock_run_authority(
            txn,
            request.owner,
            request.device,
            request.run_id,
            request.node,
            request.lease_epoch,
        )
        .await?;
        let rule_limit = request
            .rule_id
            .map(|id| {
                authority
                    .contract()
                    .contract()
                    .permissions
                    .iter()
                    .find(|rule| rule.rule_id == id)
                    .map(|rule| rule.automatic.limits.max_calls)
                    .ok_or(ScheduleStoreError::Invalid)
            })
            .transpose()?;
        let parent_digest = digest(&json(authority.provenance())?);
        let base = ledger::Entity::find()
            .filter(ledger::Column::RunId.eq(request.run_id))
            .filter(ledger::Column::OwnerUserId.eq(request.owner));
        if base
            .clone()
            .filter(ledger::Column::State.eq("overrun"))
            .count(txn)
            .await?
            > 0
        {
            return Err(ScheduleStoreError::BudgetExceeded);
        }
        let id = reservation_id(
            request.run_id,
            request.kind.name(),
            &digest(request.logical_key),
        )?;
        let existing_call = base
            .clone()
            .filter(ledger::Column::ReservationId.eq(id))
            .one(txn)
            .await?;
        reserve_run_budget(txn, &authority).await?;
        if let Some(existing) = existing_call {
            validate_row(&existing)?;
            if existing.schedule_id != authority.provenance().schedule_id
                || existing.input_sha256 != request.input_sha256
                || existing.rule_id.as_deref() != request.rule_id
                || existing.reserved_units != units
                || existing.authority_sha256 != parent_digest
            {
                return Err(ScheduleStoreError::Conflict);
            }
            return Ok(existing);
        }
        if let (Some(rule_id), Some(maximum)) = (request.rule_id, rule_limit) {
            let used = base
                .clone()
                .filter(ledger::Column::Kind.eq("tool_call"))
                .filter(ledger::Column::RuleId.eq(rule_id))
                .count(txn)
                .await?;
            if used >= u64::from(maximum) {
                return Err(ScheduleStoreError::BudgetExceeded);
            }
        }
        let query = base.filter(ledger::Column::Kind.eq(request.kind.name()));
        // Corrupt negative or undercharged pending rows must not manufacture budget.
        if query
            .clone()
            .filter(ledger::Column::ChargedUnits.lt(0))
            .count(txn)
            .await?
            > 0
            || query
                .clone()
                .filter(ledger::Column::State.eq("reserved"))
                .filter(
                    Expr::col(ledger::Column::ChargedUnits)
                        .ne(Expr::col(ledger::Column::ReservedUnits)),
                )
                .count(txn)
                .await?
                > 0
        {
            return Err(ScheduleStoreError::Invalid);
        }
        let used = query
            .select_only()
            .column_as(
                Expr::col(ledger::Column::ChargedUnits)
                    .sum()
                    .cast_as("BIGINT"),
                "used",
            )
            .into_tuple::<Option<i64>>()
            .one(txn)
            .await?
            .flatten()
            .unwrap_or(0);
        let maximum = match request.kind {
            TaskBudgetKind::ToolCall => {
                u64::from(authority.contract().contract().budget.max_calls_per_run)
            }
            TaskBudgetKind::ModelTokens => {
                authority
                    .contract()
                    .contract()
                    .budget
                    .max_model_tokens_per_run
            }
        };
        if used < 0
            || u64::try_from(used)
                .ok()
                .and_then(|v| v.checked_add(request.units))
                .is_none_or(|v| v > maximum)
        {
            return Err(ScheduleStoreError::BudgetExceeded);
        }
        insert(
            txn,
            &authority,
            request.kind.name(),
            request.logical_key,
            request.input_sha256,
            units,
            request.rule_id,
        )
        .await
    }

    /// Only the trusted model receipt path supplies actual usage. Late receipts can settle
    /// after cancellation/revocation; they never restore authority or reopen a run.
    pub async fn settle_task_model_budget(
        txn: &DatabaseTransaction,
        owner: i32,
        run_id: &str,
        id: &str,
        actual_units: u64,
        receipt_sha256: &str,
    ) -> Result<ledger::Model, ScheduleStoreError> {
        if !valid_digest(receipt_sha256) {
            return Err(ScheduleStoreError::Invalid);
        }
        let actual = i64::try_from(actual_units).map_err(|_| ScheduleStoreError::Invalid)?;
        let row = ledger::Entity::find()
            .filter(ledger::Column::ReservationId.eq(id))
            .filter(ledger::Column::OwnerUserId.eq(owner))
            .filter(ledger::Column::RunId.eq(run_id))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        validate_row(&row)?;
        if row.kind != "model_tokens" {
            return Err(ScheduleStoreError::Invalid);
        }
        if row.state != "reserved" {
            if row.charged_units == actual && row.receipt_sha256.as_deref() == Some(receipt_sha256)
            {
                return Ok(row);
            }
            return Err(ScheduleStoreError::Conflict);
        }
        let task = entity::Entity::find()
            .filter(entity::Column::ScheduleId.eq(&row.schedule_id))
            .filter(entity::Column::OwnerUserId.eq(owner))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let touched = entity::Entity::update_many()
            .set(entity::ActiveModel {
                revision: Set(task
                    .revision
                    .checked_add(1)
                    .ok_or(ScheduleStoreError::Invalid)?),
                ..Default::default()
            })
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(task.revision))
            .exec(txn)
            .await?;
        if touched.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let changed = ledger::Entity::update_many()
            .set(ledger::ActiveModel {
                charged_units: Set(actual),
                state: Set(if actual > row.reserved_units {
                    "overrun"
                } else {
                    "settled"
                }
                .into()),
                receipt_sha256: Set(Some(receipt_sha256.into())),
                version: Set(2),
                settled_at: Set(Some(authority_now(txn).await?)),
                ..Default::default()
            })
            .filter(ledger::Column::Id.eq(row.id))
            .filter(ledger::Column::Version.eq(row.version))
            .filter(ledger::Column::State.eq("reserved"))
            .exec(txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        ledger::Entity::find_by_id(row.id)
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)
    }
}

#[cfg(test)]
pub(super) mod tests;
