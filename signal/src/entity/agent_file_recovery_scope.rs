//! Original vault identities, retained independently of conversation history.
use sea_orm::entity::prelude::*;
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "agent_file_recovery_scope")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    #[sea_orm(indexed)]
    pub conversation_id: String,
    pub actor_id: String,
    pub device_id: String,
    pub authority: String,
    pub os_user: String,
    pub registered_at_unix_ms: i64,
    pub cleaned_at_unix_ms: Option<i64>,
}
impl ActiveModelBehavior for ActiveModel {}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
