use super::super::history::record_scope_gap_on;
use super::*;
use crate::entity::{
    agent_assistant_history as history, agent_assistant_history_coverage as coverage,
    agent_assistant_origin as origin, agent_assistant_search_document as document,
};
use sea_orm::PaginatorTrait;

#[tokio::test]
async fn native_coverage_records_only_the_original_observed_interval_without_inventing_source_records()
 {
    let store = setup().await;
    let policy = AssistantDataPolicy::default();
    store.settings(1, &policy).await.unwrap();
    ensure_quota_on(&store.db, "1").await.unwrap();
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    let txn = store.db.begin().await.unwrap();
    record_scope_gap_on(&txn, "1", 1000, &policy, "omitted_policy")
        .await
        .unwrap();
    record_scope_gap_on(&txn, "1", 500, &policy, "omitted_policy")
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let row = coverage::Entity::find()
        .filter(coverage::Column::Reason.eq("omitted_policy"))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.source_count, 2);
    assert_eq!(
        (row.earliest_recorded_at_ms, row.latest_recorded_at_ms),
        (500, 1000)
    );
    assert!(row.conversation_id.is_empty());
    assert_eq!(row.indexed_through_ms, None);
    assert_eq!(history::Entity::find().count(&store.db).await.unwrap(), 0);
    assert_eq!(origin::Entity::find().count(&store.db).await.unwrap(), 0);
    assert_eq!(document::Entity::find().count(&store.db).await.unwrap(), 0);
    let after = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    assert_eq!(
        (
            after.history_records,
            after.history_body_bytes,
            after.term_records,
            after.metadata_bytes
        ),
        (
            before.history_records,
            before.history_body_bytes,
            before.term_records,
            before.metadata_bytes
        )
    );
}

#[tokio::test]
async fn native_coverage_rejects_success_labels_and_outer_rollback_restores_the_original_metadata()
{
    let store = setup().await;
    let policy = AssistantDataPolicy::default();
    store.settings(1, &policy).await.unwrap();
    ensure_quota_on(&store.db, "1").await.unwrap();
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    let txn = store.db.begin().await.unwrap();
    for (time, reason) in [(1000, "indexed"), (1000, "partial"), (0, "omitted_error")] {
        assert!(
            record_scope_gap_on(&txn, "1", time, &policy, reason)
                .await
                .is_err()
        );
    }
    record_scope_gap_on(&txn, "1", 1000, &policy, "omitted_error")
        .await
        .unwrap();
    txn.rollback().await.unwrap();
    assert_eq!(
        quota::Entity::find().one(&store.db).await.unwrap().unwrap(),
        before
    );
    assert_eq!(
        coverage::Entity::find()
            .filter(coverage::Column::SourceCount.gt(0))
            .count(&store.db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn an_eligible_native_send_without_its_original_producer_records_a_gap_without_copying_the_result()
 {
    use crate::entity::agent_session;
    use desk_agent_protocol::{
        computer_use::{ComputerActionCompleted, ComputerActionResultClass},
        data_lineage::{RetentionBoundary, Sensitivity},
    };
    use desk_diagnose_core::{
        action_result::ActionResultOrigin,
        action_turn_fence::AssistantTurnFence,
        seam::{ToolOutputFormat, ToolRunOutput},
        session::{ActionIdentity, PersistedAgentSession, WorkKind},
    };
    let (store, _, _, _) =
        super::model_gate::fixture("native-send-missing-original-producer").await;
    let row = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    let origins_before = origin::Entity::find().count(&store.db).await.unwrap();
    let origin = ActionResultOrigin {
        command_completion: None,
        schema_version: 1,
        turn_fence: AssistantTurnFence::from_session(&session).unwrap().unwrap(),
        tool_call_id: "native-send-tool".into(),
        provider_id: desk_diagnose_core::ai_assistant::GMAIL_WEB_SEND_PROVIDER_ID.into(),
        tool_name: "send_gmail_message".into(),
        source_object_id: "original-native-scope".into(),
        source_envelope_ids: vec!["missing-original-model-output".into()],
        sensitivity: Sensitivity::Sensitive,
        retention: RetentionBoundary {
            expires_at_unix_ms: None,
            delete_with_run: true,
        },
        ephemeral: false,
    };
    let output = ToolRunOutput {
        format: ToolOutputFormat::Json,
        content: "{\"opaque_native_result\":true}".into(),
        image_data_url: None,
        document_preview: None,
    };
    let now = chrono::Utc::now().timestamp_millis() as u64;
    // This layer receives already authenticated native facts. A missing model
    // producer still denies body retention without guessing a projection.
    let receipt = origin
        .receipt(
            ActionIdentity::new(7, "request", "generation", WorkKind::ComputerAction),
            1,
            now,
            &output,
        )
        .unwrap();
    let completion = ComputerActionCompleted {
        work_id: "7".into(),
        action_request_id: "request".into(),
        execution_generation: "generation".into(),
        result: ComputerActionResultClass::Verified,
        facts: Vec::new(),
        message: None,
        output: None,
    };
    let txn = crate::db::begin_write(&store.db, agent_session::Entity)
        .await
        .unwrap();
    super::super::native_result::capture_on(
        &txn,
        super::super::native_result::NativeSendResult {
            origin: &origin,
            receipt: &receipt,
            output: &output,
            canonical_input: "{}",
            completion: &completion,
            provider_device_id: &session.device_id,
            dispatched_at_unix_ms: now - 1,
        },
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(history::Entity::find().count(&store.db).await.unwrap(), 0);
    assert_eq!(
        origin::Entity::find().count(&store.db).await.unwrap(),
        origins_before
    );
    let gap = coverage::Entity::find()
        .filter(coverage::Column::Reason.eq("omitted_policy"))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            gap.source_count,
            gap.earliest_recorded_at_ms,
            gap.latest_recorded_at_ms
        ),
        (1, now as i64, now as i64)
    );
    let after = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    assert_eq!(
        (
            after.history_records,
            after.history_body_bytes,
            after.term_records
        ),
        (
            before.history_records,
            before.history_body_bytes,
            before.term_records
        )
    );
}
