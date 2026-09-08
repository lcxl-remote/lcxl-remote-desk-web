use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Metadata-only receipt for one external model dispatch boundary.
///
/// No prompt, tool result, model output, credential, or provider response body
/// is stored here. Exported identities, digests and source links record which
/// labeled content the authorizer admitted without retaining its bytes.
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
    pub input_lineage_json: String,
    pub projection_digest_sha256: String,
    pub total_bytes: i64,
    /// `dispatch_intent` is intentionally durable before provider I/O. A crash
    /// in that state means the provider outcome is unknown, never "not sent".
    pub state: String,
    pub model_output_envelope_id: Option<String>,
    pub model_output_digest_sha256: Option<String>,
    pub authorized_at: DateTimeUtc,
    pub completed_at: Option<DateTimeUtc>,
    /// Immutable normalized usage from a returned provider turn; absent means
    /// no terminal usage was recorded and the task reservation must be retained.
    pub usage_json: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
