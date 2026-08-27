//! Durable, metadata-only model-egress receipts for the OSS runtime.

use desk_diagnose_core::sink_authorizer::SinkProjectionAudit;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter, Set,
};
use sha2::{Digest, Sha256};

use crate::entity::model_egress_receipt;

pub const STATE_DISPATCH_INTENT: &str = "dispatch_intent";
pub const STATE_SUCCEEDED: &str = "succeeded";
pub const STATE_FAILED: &str = "failed";

#[derive(Clone)]
pub struct SignalModelEgressStore {
    db: DatabaseConnection,
}

impl SignalModelEgressStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Persist the exact authorizer projection before any provider I/O. Failure
    /// is fail-closed: the caller must not dispatch a request without a receipt.
    pub async fn record_dispatch_intent(
        &self,
        receipt_id: String,
        export_authorization_id: String,
        model_call_ordinal: u64,
        audit: &SinkProjectionAudit,
    ) -> Result<model_egress_receipt::Model, DbErr> {
        let destination_json = serde_json::to_string(&audit.destination)
            .map_err(|error| DbErr::Custom(format!("encode model destination: {error}")))?;
        let envelope_ids_json = serde_json::to_string(&audit.envelope_ids)
            .map_err(|error| DbErr::Custom(format!("encode egress envelope ids: {error}")))?;
        let digests_sha256_json = serde_json::to_string(&audit.digests_sha256)
            .map_err(|error| DbErr::Custom(format!("encode egress digests: {error}")))?;
        let total_bytes = i64::try_from(audit.total_bytes)
            .map_err(|_| DbErr::Custom("model egress byte count exceeds i64".into()))?;
        let ordinal = i32::try_from(model_call_ordinal)
            .map_err(|_| DbErr::Custom("model call ordinal exceeds i32".into()))?;
        let projection_digest_sha256 = projection_digest(
            &destination_json,
            &envelope_ids_json,
            &digests_sha256_json,
            total_bytes,
        );
        let now = chrono::Utc::now();
        model_egress_receipt::ActiveModel {
            receipt_id: Set(receipt_id),
            export_authorization_id: Set(export_authorization_id),
            model_call_ordinal: Set(ordinal),
            destination_json: Set(destination_json),
            envelope_ids_json: Set(envelope_ids_json),
            digests_sha256_json: Set(digests_sha256_json),
            projection_digest_sha256: Set(projection_digest_sha256),
            total_bytes: Set(total_bytes),
            state: Set(STATE_DISPATCH_INTENT.into()),
            model_output_envelope_id: Set(None),
            authorized_at: Set(now),
            completed_at: Set(None),
        }
        .insert(&self.db)
        .await
    }

    pub async fn mark_succeeded(
        &self,
        receipt_id: &str,
        model_output_envelope_id: &str,
    ) -> Result<(), DbErr> {
        let result = model_egress_receipt::Entity::update_many()
            .col_expr(
                model_egress_receipt::Column::State,
                Expr::value(STATE_SUCCEEDED),
            )
            .col_expr(
                model_egress_receipt::Column::ModelOutputEnvelopeId,
                Expr::value(Some(model_output_envelope_id.to_string())),
            )
            .col_expr(
                model_egress_receipt::Column::CompletedAt,
                Expr::value(Some(chrono::Utc::now())),
            )
            .filter(model_egress_receipt::Column::ReceiptId.eq(receipt_id))
            .filter(model_egress_receipt::Column::State.eq(STATE_DISPATCH_INTENT))
            .exec(&self.db)
            .await?;
        ensure_single_update(result.rows_affected, "complete successful model egress")
    }

    pub async fn mark_failed(&self, receipt_id: &str) -> Result<(), DbErr> {
        let result = model_egress_receipt::Entity::update_many()
            .col_expr(
                model_egress_receipt::Column::State,
                Expr::value(STATE_FAILED),
            )
            .col_expr(
                model_egress_receipt::Column::CompletedAt,
                Expr::value(Some(chrono::Utc::now())),
            )
            .filter(model_egress_receipt::Column::ReceiptId.eq(receipt_id))
            .filter(model_egress_receipt::Column::State.eq(STATE_DISPATCH_INTENT))
            .exec(&self.db)
            .await?;
        ensure_single_update(result.rows_affected, "complete failed model egress")
    }
}

fn ensure_single_update(rows_affected: u64, operation: &str) -> Result<(), DbErr> {
    if rows_affected == 1 {
        Ok(())
    } else {
        Err(DbErr::Custom(format!(
            "{operation} expected one dispatch-intent receipt, updated {rows_affected}"
        )))
    }
}

fn projection_digest(
    destination_json: &str,
    envelope_ids_json: &str,
    digests_sha256_json: &str,
    total_bytes: i64,
) -> String {
    let mut hasher = Sha256::new();
    for part in [destination_json, envelope_ids_json, digests_sha256_json] {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    hasher.update(total_bytes.to_le_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::data_lineage::DestinationIdentity;
    use sea_orm::{Database, EntityTrait};

    fn audit() -> SinkProjectionAudit {
        SinkProjectionAudit {
            destination: DestinationIdentity::Model {
                connection_id: "oss-ai-gateway:1".into(),
                connection_revision: 2,
                model_id: "fake-model".into(),
                profile_revision: 3,
            },
            envelope_ids: vec!["user-1".into(), "tool-1".into()],
            digests_sha256: vec!["a".repeat(64), "b".repeat(64)],
            total_bytes: 42,
        }
    }

    #[tokio::test]
    async fn receipt_is_durable_and_contains_metadata_only() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let store = SignalModelEgressStore::new(db.clone());
        let row = store
            .record_dispatch_intent("receipt-1".into(), "export-1".into(), 1, &audit())
            .await
            .unwrap();
        assert_eq!(row.state, STATE_DISPATCH_INTENT);
        assert!(!row.destination_json.contains("credential"));
        assert!(!row.envelope_ids_json.contains("prompt"));

        store
            .mark_succeeded("receipt-1", "model-output-1")
            .await
            .unwrap();
        let row = model_egress_receipt::Entity::find_by_id("receipt-1")
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, STATE_SUCCEEDED);
        assert_eq!(
            row.model_output_envelope_id.as_deref(),
            Some("model-output-1")
        );
        assert!(row.completed_at.is_some());
    }

    #[tokio::test]
    async fn failed_dispatch_cannot_be_reclassified() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let store = SignalModelEgressStore::new(db.clone());
        store
            .record_dispatch_intent("receipt-2".into(), "export-2".into(), 1, &audit())
            .await
            .unwrap();
        store.mark_failed("receipt-2").await.unwrap();
        assert!(
            store
                .mark_succeeded("receipt-2", "model-output-2")
                .await
                .is_err()
        );
    }
}
