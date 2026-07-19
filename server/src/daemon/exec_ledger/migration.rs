//! The ledger's own migrator, independent of the signalling database's.

use sea_orm_migration::prelude::*;

mod m20260719_000001_create_exec_ledger_entry;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(
            m20260719_000001_create_exec_ledger_entry::Migration,
        )]
    }
}
