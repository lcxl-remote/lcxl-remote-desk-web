//! Durable singleton context policy with compare-and-swap updates.
use crate::entity::context_management_config as entity;
use desk_diagnose_core::model_context::PlatformContextPolicy;
use desk_signal_facade::context_management::UpdateContextManagementRequest;
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

fn parse(value: &str) -> Result<PlatformContextPolicy, DbErr> {
    let config: PlatformContextPolicy =
        serde_json::from_str(value).map_err(|_| DbErr::Custom("invalid context policy".into()))?;
    config
        .validate()
        .map_err(|message| DbErr::Custom(message.into()))?;
    Ok(config)
}

async fn row<C: ConnectionTrait>(db: &C) -> Result<entity::Model, DbErr> {
    entity::Entity::insert(entity::ActiveModel {
        id: Set(1),
        config_json: Set(serde_json::to_string(&PlatformContextPolicy::default())
            .map_err(|e| DbErr::Custom(e.to_string()))?),
    })
    .on_conflict(
        OnConflict::column(entity::Column::Id)
            .do_nothing()
            .to_owned(),
    )
    .exec_without_returning(db)
    .await?;
    entity::Entity::find_by_id(1)
        .one(db)
        .await?
        .ok_or_else(|| DbErr::Custom("context policy missing".into()))
}

pub async fn read<C: ConnectionTrait>(db: &C) -> Result<PlatformContextPolicy, DbErr> {
    parse(&row(db).await?.config_json)
}

pub async fn update<C: ConnectionTrait>(
    db: &C,
    update: &UpdateContextManagementRequest,
) -> Result<PlatformContextPolicy, WriteError> {
    let row = row(db).await?;
    let current = parse(&row.config_json)?;
    if current.revision != update.expected_revision {
        return Err(WriteError::Conflict);
    }
    let next = current
        .candidate(update.strategy.into())
        .map_err(|_| WriteError::Invalid)?;
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
    use desk_signal_facade::context_management::ContextManagementStrategyDto;
    #[tokio::test]
    async fn default_compaction_explicit_window_and_revision_conflicts() {
        let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
        db.execute(
            &sea_orm::Schema::new(db.get_database_backend())
                .create_table_from_entity(entity::Entity),
        )
        .await
        .unwrap();
        assert_eq!(
            read(&db).await.unwrap().strategy.as_str(),
            "checkpoint_summary"
        );
        let request = UpdateContextManagementRequest {
            expected_revision: 0,
            strategy: ContextManagementStrategyDto::Window,
        };
        assert_eq!(update(&db, &request).await.unwrap().revision, 1);
        assert_eq!(read(&db).await.unwrap().strategy.as_str(), "window");
        assert!(matches!(
            update(&db, &request).await,
            Err(WriteError::Conflict)
        ));
    }
}
