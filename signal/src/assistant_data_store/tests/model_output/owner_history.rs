use super::super::super::{native_export::load_owner_history_set_on, output_archive};
use super::*;
use crate::entity::{
    agent_assistant_read_projection as projection, agent_memory_use as memory_use,
};
use desk_agent_protocol::ai_assistant::user_instructions::AssistantDataSettingsUpdate;

mod visible;

#[tokio::test]
async fn accepted_owner_use_keeps_its_identity_after_model_revocation_and_cached_body_cleanup() {
    let (store, _, proof, _) = admitted("owner-history-consumed-read").await;
    let candidate = output(&proof);
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("native-output", &candidate)
        .await
        .unwrap();
    let stored = output_archive::find_on(&store.db, "root", &candidate)
        .await
        .unwrap()
        .unwrap();
    let original = (*stored.proof).clone();
    let views = load_owner_history_set_on(&store.db, &original)
        .await
        .unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].usage(), &proof.usage);
    let settings = store
        .settings(1, &AssistantDataPolicy::default())
        .await
        .unwrap();
    store
        .update_settings(
            1,
            &AssistantDataSettingsUpdate {
                expected_revision: settings.revision,
                automatic_memory_enabled: false,
                model_history_search_enabled: false,

                explanation_version: settings.explanation_version,
            },
            &AssistantDataPolicy::default(),
        )
        .await
        .unwrap();
    store
        .forget_memory(
            1,
            &MemoryDeleteRequest {
                memory_id: proof.usage.memories[0].memory_id.clone(),
                expected_revision: 1,
                operation_id: "forget-after-accepted-native".into(),
            },
            &AssistantDataPolicy::default(),
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        .unwrap();
    // The accepted proof is independent of this optional body cache. Actual
    // current source controls still decide whether a new copy is permissible.
    projection::Entity::update_many()
        .col_expr(
            projection::Column::ResultJson,
            Expr::value(Option::<String>::None),
        )
        .filter(projection::Column::ProjectionId.eq(&proof.usage.projection_id))
        .exec(&store.db)
        .await
        .unwrap();
    assert!(
        native_export::load_call_set_on(&store.db, &original.source_set)
            .await
            .is_err()
    );
    let views = load_owner_history_set_on(&store.db, &original)
        .await
        .unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].usage(), &proof.usage);
    assert_eq!(views[0].input_envelope(), &proof.source_envelope);
    assert_eq!(
        *output_archive::find_on(&store.db, "root", &candidate)
            .await
            .unwrap()
            .unwrap()
            .proof,
        original
    );
}

#[tokio::test]
async fn original_owner_history_cannot_infer_a_missing_grant_use_row_or_consumption() {
    for case in 0..5 {
        let (store, _, proof, _) = admitted(&format!("owner-history-incomplete-{case}")).await;
        let candidate = output(&proof);
        SignalModelEgressStore::new(store.db.clone())
            .mark_succeeded("native-output", &candidate)
            .await
            .unwrap();
        let archive = output_archive::find_on(&store.db, "root", &candidate)
            .await
            .unwrap()
            .unwrap();
        let native_before = native::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let quota_before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
        match case {
            0 => {
                authorization::Entity::delete_many()
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
            1 => {
                source_use::Entity::delete_many()
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
            2 => {
                memory_use::Entity::delete_many()
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
            3 => {
                projection::Entity::update_many()
                    .col_expr(
                        projection::Column::FirstConsumedAtMs,
                        Expr::value(Option::<i64>::None),
                    )
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
            _ => {
                projection::Entity::update_many()
                    .col_expr(
                        projection::Column::LastConsumedAtMs,
                        Expr::value(Some(1_i64)),
                    )
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
        }
        assert!(
            load_owner_history_set_on(&store.db, &archive.proof)
                .await
                .is_err(),
            "case {case}"
        );
        assert_eq!(
            native::Entity::find()
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            native_before
        );
        assert_eq!(
            quota::Entity::find().one(&store.db).await.unwrap().unwrap(),
            quota_before
        );
        assert!(!proof.usage.memories.is_empty());
    }
}
