use sea_orm::entity::prelude::*;
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "agent_image_attachment")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    #[sea_orm(indexed)]
    pub conversation_id: String,
    pub actor_id: String,
    pub device_id: String,
    pub tool_call_id: String,
    pub metadata_json: String,
    pub size_bytes: i64,
    #[sea_orm(indexed)]
    pub created_at_unix_ms: i64,
    pub deleted: bool,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
