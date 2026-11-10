use super::*;
use crate::entity::{
    agent_assistant_origin as origin, agent_memory as fact, agent_memory_revision as revision,
    agent_memory_source as evidence,
};

const PARENT_TEXT: &str = "Completed using the selected memory";

async fn parent(
    label: &str,
) -> (
    TestStore,
    Box<PersistedAgentSession>,
    DataEnvelope,
    ReadProjectionProof,
) {
    let (store, _, proof, _) = super::super::super::admitted(label).await;
    let output = super::super::super::output(&proof);
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("native-output", &output)
        .await
        .unwrap();
    let row = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let session = Box::new(PersistedAgentSession::decode_json(&row.state_json).unwrap());
    (store, session, output, proof)
}

#[tokio::test]
async fn actual_followup_uses_the_accepted_memory_dependent_answer_as_history_without_a_fresh_fact_read()
 {
    for remove_receipt in [false, true] {
        let (store, mut session, parent, _) =
            parent(&format!("derived-memory-valid-{remove_receipt}")).await;
        if remove_receipt {
            native::Entity::delete_by_id("native-output")
                .exec(&store.db)
                .await
                .unwrap();
        }
        let (_, output) = admit_followup(&store, &mut session, &parent, PARENT_TEXT).await;
        SignalModelEgressStore::new(store.db.clone())
            .mark_succeeded("followup-native", &output)
            .await
            .unwrap();
        let stored = output_archive::find_on(&store.db, "root", &output)
            .await
            .unwrap()
            .unwrap();
        assert!(stored.proof.source_set.authorizations.is_empty());
        let reference = &stored.proof.input_policy_graph.parents[0];
        assert_eq!(reference.output_envelope_id, parent.envelope_id);
        let original = output_archive::find_on(&store.db, &session.conversation_id, &parent)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(original.proof.source_set.authorizations.len(), 1);
        assert_eq!(original.proof.output, parent);
    }
}

#[tokio::test]
async fn forgetting_memory_keeps_the_accepted_answer_and_inflight_followup() {
    let (store, mut session, parent, proof) = parent("derived-memory-forgotten").await;
    let (_, output) = admit_followup(&store, &mut session, &parent, PARENT_TEXT).await;
    let row = native::Entity::find_by_id("followup-native")
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        AssistantNativeSourceSet::decode(row.assistant_source_set_json.as_deref().unwrap())
            .unwrap()
            .authorizations
            .is_empty()
    );
    store
        .forget_memory(
            1,
            &MemoryDeleteRequest {
                memory_id: proof.usage.memories[0].memory_id.clone(),
                expected_revision: 1,
                operation_id: "forget-derived-answer-source".into(),
            },
            &AssistantDataPolicy::default(),
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        .unwrap();

    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("followup-native", &output)
        .await
        .unwrap();
    assert!(
        output_archive::find_on(&store.db, "root", &output)
            .await
            .unwrap()
            .is_some()
    );
    let row = native::Entity::find_by_id("followup-native")
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let usage: TokenUsage = serde_json::from_str(row.usage_json.as_deref().unwrap()).unwrap();
    assert!(row.model_output_envelope_id.is_some());
    assert_eq!(
        (usage.input_tokens, usage.output_tokens),
        (Some(13), Some(5))
    );
}

#[tokio::test]
async fn later_source_policy_changes_do_not_rewrite_accepted_snapshot_policy() {
    let (store, mut session, parent, proof) = parent("derived-memory-no-export").await;
    let (_, output) = admit_followup(&store, &mut session, &parent, PARENT_TEXT).await;
    let source = evidence::Entity::find()
        .filter(
            evidence::Column::Id.eq(desk_assistant_data_store::source_rows::memory(
                &proof.usage.roots[0].source_id,
            )),
        )
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let original = origin::Entity::find()
        .filter(origin::Column::Id.eq(source.origin_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut policy: desk_agent_protocol::ai_assistant::source::SourceUsePolicy =
        serde_json::from_str(&original.policy_json).unwrap();
    policy.no_export = true;
    origin::Entity::update_many()
        .col_expr(
            origin::Column::PolicyJson,
            Expr::value(serde_json::to_string(&policy).unwrap()),
        )
        .filter(origin::Column::Id.eq(original.id))
        .exec(&store.db)
        .await
        .unwrap();
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("followup-native", &output)
        .await
        .unwrap();
    assert!(
        output_archive::find_on(&store.db, "root", &output)
            .await
            .unwrap()
            .is_some()
    );
    let row = native::Entity::find_by_id("followup-native")
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let usage: TokenUsage = serde_json::from_str(row.usage_json.as_deref().unwrap()).unwrap();
    assert!(row.model_output_envelope_id.is_some());
    assert_eq!(
        (usage.input_tokens, usage.output_tokens),
        (Some(13), Some(5))
    );
}

#[tokio::test]
async fn already_retained_answer_identity_does_not_require_loading_the_retired_memory_body_again() {
    let (store, mut session, parent, proof) = parent("derived-memory-cleared-body").await;
    let memory = &proof.usage.memories[0].memory_id;
    let cleared_at = chrono::Utc::now().timestamp_millis() - 1;
    // Simulate the completed body-only cleanup; this is not a GC/quota test.
    revision::Entity::update_many()
        .col_expr(revision::Column::Status, Expr::value("superseded"))
        .col_expr(revision::Column::ExpiresAtMs, Expr::value(Some(cleared_at)))
        .col_expr(revision::Column::Body, Expr::value(""))
        .col_expr(
            revision::Column::Context,
            Expr::value(Option::<String>::None),
        )
        .filter(revision::Column::MemoryId.eq(memory))
        .exec(&store.db)
        .await
        .unwrap();
    fact::Entity::update_many()
        .col_expr(fact::Column::Status, Expr::value("superseded"))
        .col_expr(fact::Column::ValidUntilMs, Expr::value(Some(cleared_at)))
        .col_expr(fact::Column::Body, Expr::value(""))
        .col_expr(fact::Column::Context, Expr::value(Option::<String>::None))
        .filter(fact::Column::MemoryId.eq(memory))
        .exec(&store.db)
        .await
        .unwrap();
    evidence::Entity::update_many()
        .col_expr(evidence::Column::Body, Expr::value(""))
        .col_expr(
            evidence::Column::BodyClearedAtMs,
            Expr::value(Some(cleared_at)),
        )
        .filter(evidence::Column::MemoryId.eq(memory))
        .exec(&store.db)
        .await
        .unwrap();
    let (_, output) = admit_followup(&store, &mut session, &parent, PARENT_TEXT).await;
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("followup-native", &output)
        .await
        .unwrap();
    assert!(
        output_archive::find_on(&store.db, "root", &output)
            .await
            .unwrap()
            .is_some()
    );
    let row = evidence::Entity::find()
        .filter(evidence::Column::MemoryId.eq(memory))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(row.body.is_empty());
    assert!(row.body_cleared_at_ms.is_some());
}
