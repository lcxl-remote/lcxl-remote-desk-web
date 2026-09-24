//! Durable platform goal budget. Invalid rows and database errors never widen authority.
use desk_agent_protocol::ai_assistant::goal_budget::{GoalBudgetPolicy, UpdateGoalBudgetPolicy};
use desk_diagnose_core::goal_budget as policy;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, Set,
    sea_query::{Expr, OnConflict},
};

#[derive(Debug)]
pub enum WriteError {
    Conflict,
    Invalid,
    Db(DbErr),
}
impl From<DbErr> for WriteError {
    fn from(error: DbErr) -> Self {
        Self::Db(error)
    }
}
fn parse(value: &str) -> Result<GoalBudgetPolicy, DbErr> {
    let policy = serde_json::from_str(value)
        .map_err(|_| DbErr::Custom("invalid goal budget policy".into()))?;
    policy::validate(&policy).map_err(|error| DbErr::Custom(format!("{error:?}")))?;
    Ok(policy)
}
use crate::entity::goal_budget_policy as entity;

/// Materialize the initial row once, then retain the authoritative row lock.
/// Missing initialization never permits a first policy write to bypass dispatch fencing.
pub async fn read<C: ConnectionTrait>(db: &C) -> Result<GoalBudgetPolicy, DbErr> {
    if let Some(row) = entity::Entity::find_by_id(1).one(db).await? {
        return parse(&row.config_json);
    }
    ensure_row(db).await?;
    let row = entity::Entity::find_by_id(1)
        .one(db)
        .await?
        .ok_or_else(|| DbErr::Custom("goal budget policy missing".into()))?;
    parse(&row.config_json)
}

async fn ensure_row<C: ConnectionTrait>(db: &C) -> Result<(), DbErr> {
    let encoded = serde_json::to_string(&policy::initial())
        .map_err(|_| DbErr::Custom("invalid initial goal budget policy".into()))?;
    entity::Entity::insert(entity::ActiveModel {
        id: Set(1),
        config_json: Set(encoded),
    })
    .on_conflict(
        OnConflict::column(entity::Column::Id)
            .do_nothing()
            .to_owned(),
    )
    .exec_without_returning(db)
    .await?;
    Ok(())
}

pub async fn update<C: ConnectionTrait>(
    db: &C,
    request: &UpdateGoalBudgetPolicy,
) -> Result<GoalBudgetPolicy, WriteError> {
    ensure_row(db).await?;
    let row = entity::Entity::find_by_id(1)
        .one(db)
        .await?
        .ok_or(WriteError::Conflict)?;
    let current = parse(&row.config_json)?;
    if current.revision != request.expected_revision {
        return Err(WriteError::Conflict);
    }
    let next = policy::candidate(&current, request.limits).map_err(|_| WriteError::Invalid)?;
    let encoded = serde_json::to_string(&next).map_err(|_| WriteError::Invalid)?;
    let result = entity::Entity::update_many()
        .col_expr(entity::Column::ConfigJson, Expr::value(encoded))
        .filter(entity::Column::Id.eq(1))
        .filter(entity::Column::ConfigJson.eq(row.config_json))
        .exec(db)
        .await?;
    if result.rows_affected != 1 {
        return Err(WriteError::Conflict);
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{Database, Schema, TransactionTrait};

    #[tokio::test]
    async fn policy_writes_preserve_revisions_and_transaction_rollback() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        assert!(read(&db).await.is_err());
        db.execute(
            &Schema::new(db.get_database_backend()).create_table_from_entity(entity::Entity),
        )
        .await
        .unwrap();
        let initial = read(&db).await.unwrap();
        assert_eq!(initial.revision, 0);
        let mut limits = initial.limits;
        limits.model_calls = Some(1);
        let request = UpdateGoalBudgetPolicy {
            expected_revision: 0,
            limits,
        };
        let txn = db.begin().await.unwrap();
        assert_eq!(update(&txn, &request).await.unwrap().revision, 1);
        assert_eq!(read(&txn).await.unwrap().limits.model_calls, Some(1));
        txn.rollback().await.unwrap();
        assert_eq!(read(&db).await.unwrap().revision, 0);
        assert_eq!(update(&db, &request).await.unwrap().revision, 1);
        assert!(matches!(
            update(&db, &request).await,
            Err(WriteError::Conflict)
        ));
        let mut invalid = request;
        invalid.expected_revision = 1;
        invalid.limits.deadline_ms = Some(0);
        assert!(matches!(
            update(&db, &invalid).await,
            Err(WriteError::Invalid)
        ));
        assert_eq!(read(&db).await.unwrap().revision, 1);
        entity::Entity::update_many()
            .col_expr(entity::Column::ConfigJson, Expr::value("{}"))
            .exec(&db)
            .await
            .unwrap();
        assert!(read(&db).await.is_err());
        assert!(update(&db, &invalid).await.is_err());
    }
}
