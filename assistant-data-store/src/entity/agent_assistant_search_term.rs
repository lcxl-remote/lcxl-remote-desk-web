//! Durable personal-assistant data; execution authority remains in the original task stores.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_assistant_search_term")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(indexed)]
    pub actor_id: String,
    #[sea_orm(indexed)]
    pub document_id: String,
    #[sea_orm(indexed)]
    pub term: String,
    pub frequency: i32,
    pub source_revision: i64,
}

impl ActiveModelBehavior for ActiveModel {}
