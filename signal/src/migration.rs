use sea_orm_migration::prelude::*;

mod m20231123_000001_create_device_code_table;
mod m20260624_000001_create_turn_usage_table;
mod m20260625_000001_create_ai_usage_table;
mod m20260625_000002_create_model_provider_table;
mod m20260708_000001_create_usage_hour_indexes;
mod m20260708_000002_create_usage_retention_table;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20231123_000001_create_device_code_table::Migration),
            Box::new(m20260624_000001_create_turn_usage_table::Migration),
            Box::new(m20260625_000001_create_ai_usage_table::Migration),
            Box::new(m20260625_000002_create_model_provider_table::Migration),
            Box::new(m20260708_000001_create_usage_hour_indexes::Migration),
            Box::new(m20260708_000002_create_usage_retention_table::Migration),
        ]
    }
}
