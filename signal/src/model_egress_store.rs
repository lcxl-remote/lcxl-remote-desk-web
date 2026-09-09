//! Durable, metadata-only model-egress receipts for the OSS runtime.

use desk_diagnose_core::sink_authorizer::SinkProjectionAudit;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter, Set,
    TransactionTrait,
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
        inputs: &[desk_agent_protocol::data_lineage::DataEnvelope],
    ) -> Result<model_egress_receipt::Model, DbErr> {
        let txn = self.db.begin().await?;
        let row = Self::record_dispatch_intent_on(
            &txn,
            receipt_id,
            export_authorization_id,
            model_call_ordinal,
            audit,
            inputs,
        )
        .await?;
        txn.commit().await?;
        Ok(row)
    }

    /// Join task authority and budget admission without committing independently.
    /// A receipt permits no I/O until the caller commits all admission checks.
    /// Roll back on error; duplicate receipt identities must never dispatch again.
    pub async fn record_dispatch_intent_on(
        txn: &sea_orm::DatabaseTransaction,
        receipt_id: String,
        export_authorization_id: String,
        model_call_ordinal: u64,
        audit: &SinkProjectionAudit,
        inputs: &[desk_agent_protocol::data_lineage::DataEnvelope],
    ) -> Result<model_egress_receipt::Model, DbErr> {
        let input_lineage =
            desk_diagnose_core::model_egress::project_model_input_lineage(audit, inputs)
                .map_err(|_| DbErr::Custom("invalid model input lineage".into()))?;
        let input_lineage_json = serde_json::to_string(&input_lineage)
            .map_err(|_| DbErr::Custom("invalid model input lineage".into()))?;
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
            &input_lineage_json,
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
            input_lineage_json: Set(input_lineage_json),
            projection_digest_sha256: Set(projection_digest_sha256),
            total_bytes: Set(total_bytes),
            state: Set(STATE_DISPATCH_INTENT.into()),
            model_output_envelope_id: Set(None),
            model_output_digest_sha256: Set(None),
            authorized_at: Set(now),
            completed_at: Set(None),
            usage_json: Set(None),
        }
        .insert(txn)
        .await
    }

    /// Record the provider's normalized terminal usage before interpreting output.
    /// Missing counters stay missing. Replays may only repeat the exact same usage.
    pub async fn record_terminal_usage(
        &self,
        receipt_id: &str,
        usage: &desk_diagnose_core::chat::TokenUsage,
    ) -> Result<(), DbErr> {
        let encoded = serde_json::to_string(usage)
            .map_err(|_| DbErr::Custom("model usage could not be encoded".into()))?;
        let changed = model_egress_receipt::Entity::update_many()
            .set(model_egress_receipt::ActiveModel {
                usage_json: Set(Some(encoded.clone())),
                ..Default::default()
            })
            .filter(model_egress_receipt::Column::ReceiptId.eq(receipt_id))
            .filter(model_egress_receipt::Column::UsageJson.is_null())
            .exec(&self.db)
            .await?;
        if changed.rows_affected == 1 {
            return Ok(());
        }
        let row = model_egress_receipt::Entity::find_by_id(receipt_id)
            .one(&self.db)
            .await?
            .ok_or_else(|| DbErr::Custom("model usage receipt is missing".into()))?;
        if row.usage_json.as_deref() != Some(encoded.as_str()) {
            return Err(DbErr::Custom(
                "model usage receipt cannot be replaced".into(),
            ));
        }
        Ok(())
    }

    pub async fn mark_succeeded(
        &self,
        receipt_id: &str,
        output: &desk_agent_protocol::data_lineage::DataEnvelope,
    ) -> Result<(), DbErr> {
        let txn = self.db.begin().await?;
        // Acquire SQLite's writer slot before reading the validation snapshot.
        // A deferred read followed by UPDATE can fail immediately with BUSY_SNAPSHOT
        // despite busy_timeout when another connection commits in between.
        if self.db.get_database_backend() == sea_orm::DbBackend::Sqlite {
            model_egress_receipt::Entity::update_many()
                .col_expr(
                    model_egress_receipt::Column::ReceiptId,
                    Expr::col(model_egress_receipt::Column::ReceiptId).into(),
                )
                .filter(model_egress_receipt::Column::ReceiptId.eq(receipt_id))
                .exec(&txn)
                .await?;
        }

        let row = model_egress_receipt::Entity::find_by_id(receipt_id)
            .one(&txn)
            .await?
            .ok_or_else(|| DbErr::Custom("model egress receipt missing".into()))?;
        let invalid = || DbErr::Custom("invalid model output lineage".into());
        let (audit, inputs) = receipt_inputs(&row)?;
        desk_diagnose_core::model_egress::validate_model_output_lineage(&audit, &inputs, output)
            .map_err(|_| invalid())?;
        let result = model_egress_receipt::Entity::update_many()
            .col_expr(
                model_egress_receipt::Column::State,
                Expr::value(STATE_SUCCEEDED),
            )
            .col_expr(
                model_egress_receipt::Column::ModelOutputEnvelopeId,
                Expr::value(Some(output.envelope_id.clone())),
            )
            .col_expr(
                model_egress_receipt::Column::ModelOutputDigestSha256,
                Expr::value(Some(output.digest_sha256.clone())),
            )
            .col_expr(
                model_egress_receipt::Column::CompletedAt,
                Expr::value(Some(chrono::Utc::now())),
            )
            .filter(model_egress_receipt::Column::ReceiptId.eq(receipt_id))
            .filter(model_egress_receipt::Column::State.eq(STATE_DISPATCH_INTENT))
            .exec(&txn)
            .await?;
        ensure_single_update(result.rows_affected, "complete successful model egress")?;
        txn.commit().await
    }

    /// The export identity and ordinal must come from the owner-bound frozen
    /// session's model call, never from a client-provided output identifier.
    /// This returns historical source links, not permission to use or export data.
    pub async fn read_output_evidence_on<C: sea_orm::ConnectionTrait>(
        db: &C,
        receipt_id: &str,
        export_authorization_id: &str,
        ordinal: u64,
        message: &desk_diagnose_core::chat::ChatMessage,
    ) -> Result<desk_diagnose_core::schedule::rehearsal::sources::RehearsalModelCall, DbErr> {
        let invalid = || DbErr::Custom("invalid completed model output evidence".into());
        let row = model_egress_receipt::Entity::find_by_id(receipt_id)
            .one(db)
            .await?
            .ok_or_else(invalid)?;
        if export_authorization_id.trim().is_empty()
            || row.export_authorization_id != export_authorization_id
            || ordinal == 0
            || i32::try_from(ordinal).ok() != Some(row.model_call_ordinal)
            || row.state != STATE_SUCCEEDED
            || row.completed_at.is_none_or(|at| at < row.authorized_at)
        {
            return Err(invalid());
        }
        let output = desk_diagnose_core::model_egress::model_output_message_envelope(message)
            .map_err(|_| invalid())?;
        let inputs = completed_output_inputs(&row, output)?;
        Ok(
            desk_diagnose_core::schedule::rehearsal::sources::RehearsalModelCall {
                output_message_id: message.message_id.clone(),
                export_authorization_id: row.export_authorization_id,
                inputs,
            },
        )
    }

    /// The export scope must be derived from the owner-bound original session.
    /// Compression stores the hash of its actual successful durable receipt ID.
    pub async fn read_compression_inputs_on<C: sea_orm::ConnectionTrait>(
        db: &C,
        export_authorization_id: &str,
        trace: &desk_diagnose_core::model_context::ContextSummaryDerivationV1,
    ) -> Result<Vec<desk_diagnose_core::model_egress::ModelInputLineage>, DbErr> {
        use sea_orm::QuerySelect;
        let invalid = || DbErr::Custom("invalid original compression model receipt".into());
        if export_authorization_id.trim().is_empty()
            || export_authorization_id != trace.export_authorization_id
        {
            return Err(invalid());
        }
        let output = &trace.model_output;
        let rows = model_egress_receipt::Entity::find()
            .filter(model_egress_receipt::Column::ExportAuthorizationId.eq(export_authorization_id))
            .filter(model_egress_receipt::Column::ModelOutputEnvelopeId.eq(&output.envelope_id))
            .filter(model_egress_receipt::Column::ModelOutputDigestSha256.eq(&output.digest_sha256))
            .limit(2)
            .all(db)
            .await?;
        if rows.len() != 1 {
            return Err(invalid());
        }
        let row = &rows[0];
        if trace.compressor.provider_call_key
            != format!("{:x}", Sha256::digest(row.receipt_id.as_bytes()))
        {
            return Err(invalid());
        }
        completed_output_inputs(row, output)
    }

    /// The caller must first authenticate and freeze the rehearsal session.
    /// Derive historical scope from its unique original input and the stored
    /// output turn, never from a transport request or a client-supplied receipt.
    pub async fn find_rehearsal_output_evidence_on<C: sea_orm::ConnectionTrait>(
        db: &C,
        session: &desk_diagnose_core::session::PersistedAgentSession,
        original_input_id: &str,
        message: &desk_diagnose_core::chat::ChatMessage,
    ) -> Result<desk_diagnose_core::schedule::rehearsal::sources::RehearsalModelCall, DbErr> {
        use crate::assistant_model::{ModelExportSource, model_export_id};
        use desk_diagnose_core::schedule::rehearsal::model_source::{
            RehearsalModelSource, rehearsal_model_source,
        };
        let source =
            match rehearsal_model_source(session, original_input_id, message).map_err(|_| {
                DbErr::Custom("model output is not bound to the original rehearsal input".into())
            })? {
                RehearsalModelSource::Input(id) => ModelExportSource::Input(id),
                RehearsalModelSource::Turn(id) => ModelExportSource::Turn(id),
            };
        let export_id = model_export_id(
            &session.actor_id,
            &session.device_id,
            &session.conversation_id,
            source,
        );
        Self::find_output_evidence_on(db, &export_id, message).await
    }

    /// Resolve only within the original owner/run export identity. An output ID
    /// alone is never a lookup scope, and multiple matching receipts are rejected.
    pub async fn find_output_evidence_on<C: sea_orm::ConnectionTrait>(
        db: &C,
        export_authorization_id: &str,
        message: &desk_diagnose_core::chat::ChatMessage,
    ) -> Result<desk_diagnose_core::schedule::rehearsal::sources::RehearsalModelCall, DbErr> {
        use sea_orm::QuerySelect;
        let invalid =
            || DbErr::Custom("model output cannot be uniquely located in its export".into());
        if export_authorization_id.trim().is_empty() {
            return Err(invalid());
        }
        let output = desk_diagnose_core::model_egress::model_output_message_envelope(message)
            .map_err(|_| invalid())?;
        let rows = model_egress_receipt::Entity::find()
            .filter(model_egress_receipt::Column::ExportAuthorizationId.eq(export_authorization_id))
            .filter(model_egress_receipt::Column::ModelOutputEnvelopeId.eq(&output.envelope_id))
            .filter(model_egress_receipt::Column::ModelOutputDigestSha256.eq(&output.digest_sha256))
            .limit(2)
            .all(db)
            .await?;
        if rows.len() != 1 {
            return Err(invalid());
        }
        let row = &rows[0];
        Self::read_output_evidence_on(
            db,
            &row.receipt_id,
            export_authorization_id,
            u64::try_from(row.model_call_ordinal).map_err(|_| invalid())?,
            message,
        )
        .await
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

fn completed_output_inputs(
    row: &model_egress_receipt::Model,
    output: &desk_agent_protocol::data_lineage::DataEnvelope,
) -> Result<Vec<desk_diagnose_core::model_egress::ModelInputLineage>, DbErr> {
    let invalid = || DbErr::Custom("invalid completed model output evidence".into());
    if row.state != STATE_SUCCEEDED
        || row.model_call_ordinal <= 0
        || row.completed_at.is_none_or(|at| at < row.authorized_at)
    {
        return Err(invalid());
    }
    if row.model_output_envelope_id.as_deref() != Some(output.envelope_id.as_str())
        || row.model_output_digest_sha256.as_deref() != Some(output.digest_sha256.as_str())
    {
        return Err(invalid());
    }
    let (audit, inputs) = receipt_inputs(row)?;
    desk_diagnose_core::model_egress::validate_model_output_lineage(&audit, &inputs, output)
        .map_err(|_| invalid())?;
    Ok(inputs)
}

fn receipt_inputs(
    row: &model_egress_receipt::Model,
) -> Result<
    (
        SinkProjectionAudit,
        Vec<desk_diagnose_core::model_egress::ModelInputLineage>,
    ),
    DbErr,
> {
    let invalid = || DbErr::Custom("invalid model output lineage".into());
    let audit = SinkProjectionAudit {
        destination: serde_json::from_str(&row.destination_json).map_err(|_| invalid())?,
        envelope_ids: serde_json::from_str(&row.envelope_ids_json).map_err(|_| invalid())?,
        digests_sha256: serde_json::from_str(&row.digests_sha256_json).map_err(|_| invalid())?,
        total_bytes: usize::try_from(row.total_bytes).map_err(|_| invalid())?,
    };
    let inputs = serde_json::from_str::<Vec<desk_diagnose_core::model_egress::ModelInputLineage>>(
        &row.input_lineage_json,
    )
    .map_err(|_| invalid())?;
    if row.projection_digest_sha256
        != projection_digest(
            &row.destination_json,
            &row.envelope_ids_json,
            &row.digests_sha256_json,
            &row.input_lineage_json,
            row.total_bytes,
        )
    {
        return Err(invalid());
    }
    desk_diagnose_core::model_egress::validate_model_input_lineage(&audit, &inputs)
        .map_err(|_| invalid())?;
    Ok((audit, inputs))
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
    input_lineage_json: &str,
    total_bytes: i64,
) -> String {
    let mut hasher = Sha256::new();
    for part in [
        destination_json,
        envelope_ids_json,
        digests_sha256_json,
        input_lineage_json,
    ] {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    hasher.update(total_bytes.to_le_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
pub(crate) fn test_inputs(
    audit: &SinkProjectionAudit,
) -> Vec<desk_agent_protocol::data_lineage::DataEnvelope> {
    use desk_agent_protocol::data_lineage::{
        ContentRef, DataEnvelope, DataProvenance, RetentionBoundary, Sensitivity,
    };
    audit
        .envelope_ids
        .iter()
        .zip(&audit.digests_sha256)
        .map(|(id, digest)| DataEnvelope {
            schema_version: 1,
            envelope_id: id.clone(),
            content: ContentRef::ImmutableBlob {
                blob_id: "test-input".into(),
                sha256: digest.clone(),
                size_bytes: audit.total_bytes as u64,
                media_type: "text/plain".into(),
            },
            provenance: DataProvenance {
                source_provider_id: "user".into(),
                source_tool_name: "user-input".into(),
                source_object_id: None,
                source_envelope_ids: vec![],
            },
            digest_sha256: digest.clone(),
            sensitivity: Sensitivity::UserContent,
            allowed_destinations: vec![audit.destination.clone()],
            retention: RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: true,
            },
        })
        .collect()
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

    fn output(id: &str) -> desk_agent_protocol::data_lineage::DataEnvelope {
        let audit = audit();
        let mut value = test_inputs(&audit).remove(0);
        value.envelope_id = id.into();
        value.provenance.source_provider_id = "external-model".into();
        value.provenance.source_tool_name = "model-response".into();
        value.provenance.source_envelope_ids = audit.envelope_ids;
        value
    }

    #[tokio::test]
    async fn output_lineage_must_match_original_receipt_before_success() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let store = SignalModelEgressStore::new(db.clone());
        let row = store
            .record_dispatch_intent(
                "output-binding".into(),
                "export".into(),
                1,
                &audit(),
                &test_inputs(&audit()),
            )
            .await
            .unwrap();
        for case in 0..3 {
            let mut wrong = output("output");
            match case {
                0 => {
                    wrong.provenance.source_envelope_ids.pop();
                }
                1 => wrong
                    .provenance
                    .source_envelope_ids
                    .push("different-call-input".into()),
                _ => wrong.allowed_destinations.clear(),
            }
            assert!(
                store
                    .mark_succeeded("output-binding", &wrong)
                    .await
                    .is_err()
            );
            assert_eq!(
                model_egress_receipt::Entity::find_by_id("output-binding")
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap(),
                row
            );
        }
        let mut valid = output("output");
        let text = "actual model answer";
        let hash = format!("{:x}", Sha256::digest(text.as_bytes()));
        valid.digest_sha256 = hash.clone();
        valid.content = desk_agent_protocol::data_lineage::ContentRef::ImmutableBlob {
            blob_id: "result".into(),
            sha256: hash,
            size_bytes: text.len() as u64,
            media_type: "text/plain".into(),
        };

        store
            .mark_succeeded("output-binding", &valid)
            .await
            .unwrap();
        let finished = model_egress_receipt::Entity::find_by_id("output-binding")
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            finished.model_output_digest_sha256.as_deref(),
            Some(valid.digest_sha256.as_str())
        );
        assert_eq!(finished.state, STATE_SUCCEEDED);
        assert!(
            store
                .mark_succeeded("output-binding", &output("another-output"))
                .await
                .is_err()
        );
        assert_eq!(
            model_egress_receipt::Entity::find_by_id("output-binding")
                .one(&db)
                .await
                .unwrap()
                .unwrap(),
            finished
        );
        let mut message = desk_diagnose_core::chat::ChatMessage::text(
            "answer",
            desk_diagnose_core::chat::ChatRole::Assistant,
            text,
        );
        message.data_envelope = Some(valid);
        assert_eq!(
            SignalModelEgressStore::find_output_evidence_on(&db, "export", &message)
                .await
                .unwrap()
                .inputs
                .len(),
            2
        );
        use sea_orm::IntoActiveModel;
        let mut other = finished.clone().into_active_model();
        other.receipt_id = Set("other-export-output".into());
        other.export_authorization_id = Set("another-export".into());
        other.model_call_ordinal = Set(2);
        other.insert(&db).await.unwrap();
        assert!(
            SignalModelEgressStore::find_output_evidence_on(&db, "export", &message)
                .await
                .is_ok()
        );
        let mut duplicate = finished.into_active_model();
        duplicate.receipt_id = Set("duplicate-output".into());
        duplicate.model_call_ordinal = Set(2);
        duplicate.insert(&db).await.unwrap();
        let before = model_egress_receipt::Entity::find().all(&db).await.unwrap();
        assert!(
            SignalModelEgressStore::find_output_evidence_on(&db, "export", &message)
                .await
                .is_err()
        );
        assert_eq!(
            model_egress_receipt::Entity::find().all(&db).await.unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn lineage_is_persisted_and_mismatched_inputs_create_no_receipt() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let store = SignalModelEgressStore::new(db.clone());
        let audit = audit();
        let mut inputs = test_inputs(&audit);
        inputs[1].provenance.source_envelope_ids = vec!["original-tool-result".into()];
        let row = store
            .record_dispatch_intent("lineage".into(), "export".into(), 1, &audit, &inputs)
            .await
            .unwrap();
        let persisted = model_egress_receipt::Entity::find_by_id("lineage")
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted, row);
        let entries: Vec<desk_diagnose_core::model_egress::ModelInputLineage> =
            serde_json::from_str(&row.input_lineage_json).unwrap();
        assert_eq!(entries[1].source_envelope_ids, ["original-tool-result"]);
        assert!(!row.input_lineage_json.contains("test-input"));
        let original_digest = row.projection_digest_sha256;
        inputs[1].provenance.source_envelope_ids = vec!["other-tool-result".into()];
        let changed = store
            .record_dispatch_intent("lineage-other".into(), "export".into(), 2, &audit, &inputs)
            .await
            .unwrap();
        assert_ne!(changed.projection_digest_sha256, original_digest);
        inputs[1].envelope_id = "unmatched-input".into();
        assert!(
            store
                .record_dispatch_intent("invalid".into(), "export".into(), 3, &audit, &inputs)
                .await
                .is_err()
        );
        assert!(
            model_egress_receipt::Entity::find_by_id("invalid")
                .one(&db)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn terminal_usage_is_immutable_and_missing_counters_stay_unknown() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let store = SignalModelEgressStore::new(db.clone());
        let usage = desk_diagnose_core::chat::TokenUsage {
            input_tokens: Some(7),
            output_tokens: Some(3),
            ..Default::default()
        };
        assert!(
            store
                .record_terminal_usage("missing", &usage)
                .await
                .is_err()
        );
        for (id, original) in [("known", usage), ("unknown", Default::default())] {
            store
                .record_dispatch_intent(
                    id.into(),
                    format!("export-{id}"),
                    1,
                    &audit(),
                    &test_inputs(&audit()),
                )
                .await
                .unwrap();
            store.record_terminal_usage(id, &original).await.unwrap();
            store.record_terminal_usage(id, &original).await.unwrap();
            store.mark_failed(id).await.unwrap();
            let changed = desk_diagnose_core::chat::TokenUsage {
                output_tokens: Some(99),
                ..original
            };
            assert!(store.record_terminal_usage(id, &changed).await.is_err());
            let row = model_egress_receipt::Entity::find_by_id(id)
                .one(&db)
                .await
                .unwrap()
                .unwrap();
            let recorded: desk_diagnose_core::chat::TokenUsage =
                serde_json::from_str(row.usage_json.as_deref().unwrap()).unwrap();
            assert_eq!(recorded, original);
            assert_eq!(
                desk_diagnose_core::schedule::model_usage::terminal_token_units(&recorded),
                if id == "known" { Some(10) } else { None }
            );
        }
    }

    #[tokio::test]
    async fn dispatch_receipt_follows_caller_commit_and_rollback() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let txn = db.begin().await.unwrap();
        SignalModelEgressStore::record_dispatch_intent_on(
            &txn,
            "receipt-txn".into(),
            "export-txn".into(),
            1,
            &audit(),
            &test_inputs(&audit()),
        )
        .await
        .unwrap();
        assert_eq!(
            model_egress_receipt::Entity::find()
                .all(&txn)
                .await
                .unwrap()
                .len(),
            1
        );
        txn.rollback().await.unwrap();
        assert!(
            model_egress_receipt::Entity::find()
                .all(&db)
                .await
                .unwrap()
                .is_empty()
        );
        let txn = db.begin().await.unwrap();
        let expected = SignalModelEgressStore::record_dispatch_intent_on(
            &txn,
            "receipt-txn".into(),
            "export-txn".into(),
            1,
            &audit(),
            &test_inputs(&audit()),
        )
        .await
        .unwrap();
        txn.commit().await.unwrap();
        let txn = db.begin().await.unwrap();
        assert!(
            SignalModelEgressStore::record_dispatch_intent_on(
                &txn,
                "receipt-txn".into(),
                "export-txn".into(),
                1,
                &audit(),
                &test_inputs(&audit()),
            )
            .await
            .is_err()
        );
        txn.rollback().await.unwrap();
        assert_eq!(
            model_egress_receipt::Entity::find().all(&db).await.unwrap(),
            vec![expected]
        );
    }

    #[tokio::test]
    async fn audit_completion_waits_for_writer_without_read_lock_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("audit.db").display()
        );
        let mut options = sea_orm::ConnectOptions::new(url);
        options.max_connections(4).map_sqlx_sqlite_opts(|options| {
            options
                .journal_mode(sea_orm::sqlx::sqlite::SqliteJournalMode::Wal)
                .busy_timeout(std::time::Duration::from_secs(2))
        });
        let db = Database::connect(options).await.unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let store = SignalModelEgressStore::new(db.clone());
        store
            .record_dispatch_intent(
                "contended".into(),
                "export".into(),
                1,
                &audit(),
                &test_inputs(&audit()),
            )
            .await
            .unwrap();
        let writer = db.begin().await.unwrap();
        model_egress_receipt::Entity::update_many()
            .col_expr(model_egress_receipt::Column::UsageJson, Expr::value("{}"))
            .filter(model_egress_receipt::Column::ReceiptId.eq("contended"))
            .exec(&writer)
            .await
            .unwrap();
        let completion =
            tokio::spawn(
                async move { store.mark_succeeded("contended", &output("complete")).await },
            );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !completion.is_finished(),
            "audit should wait for the writer instead of failing a read-to-write upgrade"
        );
        writer.commit().await.unwrap();
        completion.await.unwrap().unwrap();
        assert_eq!(
            model_egress_receipt::Entity::find_by_id("contended")
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .state,
            STATE_SUCCEEDED
        );
    }

    #[tokio::test]
    async fn receipt_is_durable_and_contains_metadata_only() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let store = SignalModelEgressStore::new(db.clone());
        let row = store
            .record_dispatch_intent(
                "receipt-1".into(),
                "export-1".into(),
                1,
                &audit(),
                &test_inputs(&audit()),
            )
            .await
            .unwrap();
        assert_eq!(row.state, STATE_DISPATCH_INTENT);
        assert!(!row.destination_json.contains("credential"));
        assert!(!row.envelope_ids_json.contains("prompt"));

        store
            .mark_succeeded("receipt-1", &output("model-output-1"))
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
            .record_dispatch_intent(
                "receipt-2".into(),
                "export-2".into(),
                1,
                &audit(),
                &test_inputs(&audit()),
            )
            .await
            .unwrap();
        store.mark_failed("receipt-2").await.unwrap();
        assert!(
            store
                .mark_succeeded("receipt-2", &output("model-output-2"))
                .await
                .is_err()
        );
    }
}
