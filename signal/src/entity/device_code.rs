use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "device_code")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    #[sea_orm(unique)]
    pub client_id: String,
    #[sea_orm(unique)]
    pub device_code: String,
    /// Owner-configured per-code capability ceiling, JSON-encoded
    /// `SecuritySettings`. `None` means no explicit config — treated as an
    /// all-`None` ceiling (every dimension prompts, the restrictive default for a
    /// shareable code), never a wide-open owner session.
    pub capabilities: Option<String>,
    /// Code generation. Regenerating a code bumps this so a superseded code (and
    /// any grant session minted from it) is refused at redeem/stamp time.
    #[sea_orm(default_value = 0)]
    pub generation: i32,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
