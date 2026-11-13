//! Durable personal-assistant data; execution authority remains in the original task stores.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_assistant_runtime")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique_key = "actor_id")]
    pub actor_id: String,
    pub settings_instance_id: Option<String>,
    pub input_seq: i64,
    pub history_data_generation: i64,
    pub memory_state_generation: i64,
    pub control_generation: i64,

    pub learning_cutoff_input_seq: i64,
    pub settings_revision_seen: i64,
    pub revision: i64,
}

impl ActiveModelBehavior for ActiveModel {}
