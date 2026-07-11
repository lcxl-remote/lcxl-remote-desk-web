use sea_orm_migration::prelude::*;

/// Add the per-code capability ceiling and generation to `device_code`, unifying
/// device codes into the `AccessGrant` model (owner-configurable ceiling, plus a
/// generation that regeneration bumps to invalidate a superseded code).
///
/// Existing rows get the restrictive defaults: `capabilities = NULL` (an
/// all-`None` ceiling — every dimension prompts) and `generation = 0`.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(DeviceCode::Table)
                    .add_column(ColumnDef::new(DeviceCode::Capabilities).text().null())
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(DeviceCode::Table)
                    .add_column(
                        ColumnDef::new(DeviceCode::Generation)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(DeviceCode::Table)
                    .drop_column(DeviceCode::Capabilities)
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(DeviceCode::Table)
                    .drop_column(DeviceCode::Generation)
                    .to_owned(),
            )
            .await
    }
}

#[derive(Iden)]
pub enum DeviceCode {
    Table,
    Capabilities,
    Generation,
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Database, FromQueryResult, Statement};
    use sea_orm_migration::{MigrationTrait, SchemaManager};

    #[derive(FromQueryResult)]
    struct NewCols {
        capabilities: Option<String>,
        generation: i32,
    }

    /// Applying the migration on an **existing** populated table adds the two
    /// columns and back-fills legacy rows with the restrictive defaults:
    /// `capabilities = NULL` (all-`None` ceiling) and `generation = 0`.
    #[tokio::test]
    async fn adds_columns_with_defaults_to_legacy_rows() {
        let db = Database::connect("sqlite::memory:").await.unwrap();

        // Build the pre-migration schema and seed a legacy device code, without
        // the new columns.
        db.execute_unprepared(
            "CREATE TABLE device_code ( \
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 client_id VARCHAR NOT NULL UNIQUE, \
                 device_code VARCHAR NOT NULL UNIQUE, \
                 created_at TIMESTAMP NOT NULL, \
                 updated_at TIMESTAMP NOT NULL )",
        )
        .await
        .unwrap();
        db.execute_unprepared(
            "INSERT INTO device_code (client_id, device_code, created_at, updated_at) \
             VALUES ('client-legacy', 'ABC234', '2024-01-01 00:00:00', '2024-01-01 00:00:00')",
        )
        .await
        .unwrap();

        let manager = SchemaManager::new(&db);
        super::Migration.up(&manager).await.unwrap();

        let row = NewCols::find_by_statement(Statement::from_string(
            db.get_database_backend(),
            "SELECT capabilities, generation FROM device_code WHERE client_id = 'client-legacy'"
                .to_string(),
        ))
        .one(&db)
        .await
        .unwrap()
        .expect("legacy row survives the migration");

        assert_eq!(
            row.capabilities, None,
            "legacy code has no explicit ceiling"
        );
        assert_eq!(row.generation, 0, "legacy code starts at generation 0");
    }
}
