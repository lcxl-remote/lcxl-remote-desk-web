use sea_orm_migration::prelude::*;

mod m20231123_000001_create_device_code_table;
mod m20260624_000001_create_turn_usage_table;
mod m20260625_000001_create_ai_usage_table;
mod m20260625_000002_create_model_provider_table;
mod m20260708_000001_create_usage_hour_indexes;
mod m20260708_000002_create_usage_retention_table;
mod m20260710_000001_add_device_code_capabilities;
mod m20260722_000001_create_host_remote_access_state;
mod m20260724_000001_create_agent_session;
mod m20260724_000002_add_model_provider_same_tool_limit;
mod m20260724_000003_add_model_provider_max_steps;
mod m20260724_000004_raise_agent_turn_defaults;
mod m20260725_000001_create_agent_exec_task;
mod m20260725_000002_add_agent_session_retention;

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
            Box::new(m20260710_000001_add_device_code_capabilities::Migration),
            Box::new(m20260722_000001_create_host_remote_access_state::Migration),
            Box::new(m20260724_000001_create_agent_session::Migration),
            Box::new(m20260724_000002_add_model_provider_same_tool_limit::Migration),
            Box::new(m20260724_000003_add_model_provider_max_steps::Migration),
            Box::new(m20260724_000004_raise_agent_turn_defaults::Migration),
            Box::new(m20260725_000001_create_agent_exec_task::Migration),
            Box::new(m20260725_000002_add_agent_session_retention::Migration),
        ]
    }
}
