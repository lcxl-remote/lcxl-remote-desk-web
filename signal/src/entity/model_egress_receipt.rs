use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Metadata-only receipt for one external model dispatch boundary.
///
/// No prompt, tool result, model output, credential, or provider response body
/// is stored here. The exact exported envelope identities and their digests are
/// sufficient to reconstruct which labeled content the authorizer admitted.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "model_egress_receipt")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub receipt_id: String,
    pub export_authorization_id: String,
    pub model_call_ordinal: i32,
    pub destination_json: String,
    pub envelope_ids_json: String,
    pub digests_sha256_json: String,
    pub projection_digest_sha256: String,
    pub total_bytes: i64,
    /// `dispatch_intent` is intentionally durable before provider I/O. A crash
    /// in that state means the provider outcome is unknown, never "not sent".
    pub state: String,
    pub model_output_envelope_id: Option<String>,
    pub authorized_at: DateTimeUtc,
    pub completed_at: Option<DateTimeUtc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
