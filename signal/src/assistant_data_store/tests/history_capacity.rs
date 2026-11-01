use super::*;
use crate::entity::{
    agent_assistant_history as archive, agent_assistant_history_coverage as coverage,
    agent_assistant_origin as origin, agent_assistant_quota as quota,
};
use desk_agent_protocol::{
    ai_assistant::{history::HistoryAdmission, source::AssistantSourceKind},
    data_lineage::DestinationIdentity,
};
use desk_diagnose_core::{
    ai_assistant::source::{HistoryCapture, HistorySourceDirectory, owner_text_policy},
    native_ids::{self, Kind},
};
use sea_orm::PaginatorTrait;

mod staged;

#[tokio::test]
#[allow(clippy::field_reassign_with_default)]
async fn fragments_count_as_one_native_record_and_later_replay_cannot_recreate_removed_text() {
    let store = setup().await;
    store
        .settings(1, &AssistantDataPolicy::default())
        .await
        .unwrap();
    let config = store.db.config_read().await;
    let audience = "offline-device";
    let device_ref = native_ids::stable(
        Kind::AssistantDeviceBinding,
        &(&config.metadata.instance, &audience),
    );
    let mut policy = AssistantDataPolicy::default();
    policy.max_history_records = 1;
    let source_policy = owner_text_policy(1, false);
    let text = "database 中文 ".repeat(20000);
    let message = desk_diagnose_core::model_message_labels::model_bound_user_message(
        native_ids::stable(Kind::Message, &"source-one"),
        text.clone(),
        DestinationIdentity::Model {
            connection_id: "model-connection".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
    )
    .unwrap();
    runtime::Entity::update_many()
        .col_expr(
            runtime::Column::InputSeq,
            sea_orm::sea_query::Expr::value(1_i64),
        )
        .filter(runtime::Column::ActorId.eq("1"))
        .exec(&store.db)
        .await
        .unwrap();
    let capture = HistoryCapture {
        native: None,
        actor_id: "1",
        device_ref: &device_ref,
        directory: Some(HistorySourceDirectory {
            instance_id: &config.metadata.instance,
            audience,
            device_name: Some("Offline device"),
        }),
        conversation_id: "conversation",
        root_conversation_id: "conversation",
        native_id: &message.message_id,
        kind: AssistantSourceKind::Input,
        role: "user",
        content: &text,
        envelope: message.data_envelope.as_ref().unwrap(),
        source_policy: &source_policy,
        dependencies: &[],
        accepted_input_seq: 1,
        recorded_at_unix_ms: 100,
        statement_timezone: "UTC",
        revision: 1,
    };
    let txn = store.db.begin().await.unwrap();
    ensure_runtime_on(&txn, "1").await.unwrap();
    let mut insufficient = policy.clone();
    insufficient.max_history_body_bytes = 1024;
    let rejected = super::super::history::capture_on(&txn, &capture, &insufficient)
        .await
        .unwrap();
    assert_eq!(rejected.admission, HistoryAdmission::OmittedCapacity);
    assert_eq!(archive::Entity::find().count(&txn).await.unwrap(), 0);
    assert_eq!(
        desk_assistant_data_store::entity::agent_assistant_search_document::Entity::find()
            .count(&txn)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        desk_assistant_data_store::entity::agent_assistant_search_term::Entity::find()
            .count(&txn)
            .await
            .unwrap(),
        0
    );
    let first = super::super::history::capture_on(&txn, &capture, &policy)
        .await
        .unwrap();
    assert_eq!(first.admission, HistoryAdmission::Indexed);
    assert!(!first.partial);
    assert!(text.len() > 256 * 1024);
    assert!(first.source_ids.len() > 64);
    txn.commit().await.unwrap();
    let counted = quota::Entity::find()
        .filter(quota::Column::ActorId.eq("1"))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(counted.history_records, 1);
    assert_eq!(
        archive::Entity::find().count(&store.db).await.unwrap(),
        first.source_ids.len() as u64
    );
    let txn = store.db.begin().await.unwrap();
    ensure_runtime_on(&txn, "1").await.unwrap();
    let replay = super::super::history::capture_on(&txn, &capture, &policy)
        .await
        .unwrap();
    assert_eq!(replay.source_ids, first.source_ids);
    txn.commit().await.unwrap();
    assert_eq!(
        quota::Entity::find()
            .filter(quota::Column::ActorId.eq("1"))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        counted
    );
    let other_native_id = native_ids::stable(Kind::Message, &"source-two");
    let second = HistoryCapture {
        native_id: &other_native_id,
        ..capture
    };
    let txn = store.db.begin().await.unwrap();
    ensure_runtime_on(&txn, "1").await.unwrap();
    let rejected = super::super::history::capture_on(&txn, &second, &policy)
        .await
        .unwrap();
    assert_eq!(rejected.admission, HistoryAdmission::OmittedCapacity);
    txn.commit().await.unwrap();
    assert_eq!(
        archive::Entity::find().count(&store.db).await.unwrap(),
        first.source_ids.len() as u64
    );
    let origin_count = origin::Entity::find().count(&store.db).await.unwrap();
    assert_eq!(origin_count, 1);
    let indexed = coverage::Entity::find()
        .filter(coverage::Column::Reason.eq("indexed"))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(indexed.source_count, 1);
    let omitted = coverage::Entity::find()
        .filter(coverage::Column::Reason.eq("omitted_capacity"))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(omitted.source_count, 2);
    // The native source marker remains even when its history rows are removed.
    archive::Entity::delete_many()
        .exec(&store.db)
        .await
        .unwrap();
    let original = HistoryCapture {
        native_id: &message.message_id,
        ..second
    };
    let txn = store.db.begin().await.unwrap();
    ensure_runtime_on(&txn, "1").await.unwrap();
    let restricted = super::super::history::capture_on(&txn, &original, &policy)
        .await
        .unwrap();
    assert_eq!(restricted.admission, HistoryAdmission::OmittedPolicy);
    txn.commit().await.unwrap();
    assert_eq!(archive::Entity::find().count(&store.db).await.unwrap(), 0);
    assert_eq!(
        origin::Entity::find().count(&store.db).await.unwrap(),
        origin_count
    );
}
