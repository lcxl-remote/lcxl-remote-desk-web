//! Database-owned OSS search settings; no environment fallback or secret cache.

use crate::entity::web_search_config as entity;
use desk_signal_facade::web_search::{SearchConfig, SearchConfigUpdate};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, Set,
    sea_query::{Expr, OnConflict},
};

#[derive(Debug)]
pub enum WriteError {
    Conflict(u64),
    Invalid(&'static str),
    Db(DbErr),
}

impl From<DbErr> for WriteError {
    fn from(value: DbErr) -> Self {
        Self::Db(value)
    }
}

pub async fn read<C: ConnectionTrait>(db: &C) -> Result<SearchConfig, DbErr> {
    // Keep the database future off nested agent/dispatch futures' stack. This
    // read is also reached from deep permission and object-read workflows.
    let row = Box::pin(read_row(db)).await?;
    SearchConfig::parse(&row.config_json).map_err(|message| DbErr::Custom(message.into()))
}

async fn read_row<C: ConnectionTrait>(db: &C) -> Result<entity::Model, DbErr> {
    entity::Entity::insert(entity::ActiveModel {
        id: Set(1),
        config_json: Set(serde_json::to_string(&SearchConfig::default())
            .map_err(|_| DbErr::Custom("encode Web Search defaults".into()))?),
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
        .ok_or_else(|| DbErr::Custom("Web Search configuration row missing".into()))
}

pub async fn update<C: ConnectionTrait>(
    db: &C,
    update: &SearchConfigUpdate,
) -> Result<SearchConfig, WriteError> {
    let row = read_row(db).await?;
    let current = SearchConfig::parse(&row.config_json)
        .map_err(|message| WriteError::Db(DbErr::Custom(message.into())))?;
    if current.revision != update.expected_revision {
        return Err(WriteError::Conflict(current.revision));
    }
    let candidate = current.candidate(update).map_err(WriteError::Invalid)?;
    let value = serde_json::to_string(&candidate)
        .map_err(|_| WriteError::Invalid("encode Web Search configuration"))?;
    let result = entity::Entity::update_many()
        .col_expr(entity::Column::ConfigJson, Expr::value(value))
        .filter(entity::Column::Id.eq(1))
        .filter(entity::Column::ConfigJson.eq(row.config_json))
        .exec(db)
        .await?;
    if result.rows_affected != 1 {
        return Err(WriteError::Conflict(read(db).await?.revision));
    }
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_signal_facade::web_search::SearchProvider;
    use sea_orm::{Database, Schema};

    #[tokio::test]
    async fn defaults_persist_and_cas_isolates_credentials() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        db.execute(
            &Schema::new(db.get_database_backend()).create_table_from_entity(entity::Entity),
        )
        .await
        .unwrap();
        assert!(read(&db).await.unwrap().configured());
        let update_request = SearchConfigUpdate {
            expected_revision: 0,
            provider: SearchProvider::Brave,
            api_key: Some("secret".into()),
        };
        assert_eq!(update(&db, &update_request).await.unwrap().revision, 1);
        assert!(matches!(
            update(&db, &update_request).await,
            Err(WriteError::Conflict(1))
        ));
        let current = read(&db).await.unwrap();
        assert_eq!(current.provider, SearchProvider::Brave);
        assert!(current.configured());
        let cleared = update(
            &db,
            &SearchConfigUpdate {
                expected_revision: 1,
                provider: SearchProvider::Tavily,
                api_key: None,
            },
        )
        .await
        .unwrap();
        assert!(!cleared.configured());
        entity::Entity::update_many()
            .col_expr(entity::Column::ConfigJson, Expr::value("corrupt-secret"))
            .exec(&db)
            .await
            .unwrap();
        let error = read(&db).await.unwrap_err().to_string();
        assert!(!error.contains("corrupt-secret"));
        assert!(read(&db).await.is_err());
    }
}
