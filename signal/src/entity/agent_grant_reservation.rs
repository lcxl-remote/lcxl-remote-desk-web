use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_grant_reservation")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub reservation_id: String,
    pub grant_id: String,
    pub run_id: String,
    #[sea_orm(unique)]
    pub call_id: String,
    #[sea_orm(unique)]
    pub work_id: i64,
    pub canonical_input_digest_sha256: String,
    pub state: String,
    pub generation: i64,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
