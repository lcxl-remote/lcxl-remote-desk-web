use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "agent_assistant_device_roster")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub conversation_id: String,
    #[sea_orm(indexed)]
    pub actor_id: String,
    pub revision: i64,
    pub members_json: String,
}

impl ActiveModelBehavior for ActiveModel {}
