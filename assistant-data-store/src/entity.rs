//! Shared table shapes; host configuration and native execution tables stay in the host.

pub mod agent_assistant_citation;
pub mod agent_assistant_control_receipt;
pub mod agent_assistant_data_authorization;
pub mod agent_assistant_device_roster;
pub mod agent_assistant_history;
pub mod agent_assistant_history_conversation;
pub mod agent_assistant_history_coverage;
pub mod agent_assistant_model_input_item;
pub mod agent_assistant_model_output;
pub mod agent_assistant_origin;
pub mod agent_assistant_quota;
pub mod agent_assistant_read_projection;
pub mod agent_assistant_retrieval_query;
pub mod agent_assistant_runtime;
pub mod agent_assistant_search_document;
pub mod agent_assistant_search_term;
pub mod agent_assistant_source_suppression;
pub mod agent_assistant_source_use;
pub mod agent_main_request;
pub mod agent_memory;
pub mod agent_memory_commit;
pub mod agent_memory_revision;
pub mod agent_memory_source;
pub mod agent_memory_tombstone;
pub mod agent_memory_use;

/// Add assistant tables to the host schema transaction.
#[cfg(any(feature = "schema-sync", test))]
pub fn register_schema(schema: sea_orm::SchemaBuilder) -> sea_orm::SchemaBuilder {
    schema
        .register(agent_assistant_citation::Entity)
        .register(agent_assistant_control_receipt::Entity)
        .register(agent_assistant_data_authorization::Entity)
        .register(agent_assistant_device_roster::Entity)
        .register(agent_assistant_history::Entity)
        .register(agent_assistant_history_conversation::Entity)
        .register(agent_assistant_history_coverage::Entity)
        .register(agent_assistant_model_output::Entity)
        .register(agent_assistant_model_input_item::Entity)
        .register(agent_assistant_origin::Entity)
        .register(agent_assistant_quota::Entity)
        .register(agent_assistant_read_projection::Entity)
        .register(agent_assistant_retrieval_query::Entity)
        .register(agent_assistant_runtime::Entity)
        .register(agent_assistant_search_document::Entity)
        .register(agent_assistant_search_term::Entity)
        .register(agent_assistant_source_suppression::Entity)
        .register(agent_assistant_source_use::Entity)
        .register(agent_main_request::Entity)
        .register(agent_memory::Entity)
        .register(agent_memory_commit::Entity)
        .register(agent_memory_revision::Entity)
        .register(agent_memory_source::Entity)
        .register(agent_memory_tombstone::Entity)
        .register(agent_memory_use::Entity)
}
