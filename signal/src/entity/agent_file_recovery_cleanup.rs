//! Durable conversation deletion intent; retained after conversation history is removed.
use sea_orm::entity::prelude::*;
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "agent_file_recovery_cleanup")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub conversation_id: String,
    pub actor_id: String,
    #[sea_orm(indexed)]
    pub device_id: String,
    pub created_at_unix_ms: i64,
    #[sea_orm(indexed)]
    pub next_attempt_at_unix_ms: i64,
    pub attempts: i64,
    pub lease_id: Option<String>,
    pub lease_until_unix_ms: Option<i64>,
    pub completed_at_unix_ms: Option<i64>,
    pub last_error: Option<String>,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
