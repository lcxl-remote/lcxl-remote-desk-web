//! Immutable metadata items shared by the accepted outputs of one conversation.
//! They contain no model body and grant no source access or native I/O.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_assistant_model_input_item")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub actor_id: String,
    pub conversation_id: String,
    pub kind: i16,
    pub ordinal: i64,
    pub item_sha256: String,
    #[sea_orm(column_type = "Text")]
    pub item_json: String,
}

impl ActiveModelBehavior for ActiveModel {}
