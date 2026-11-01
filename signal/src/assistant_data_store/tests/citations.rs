use super::super::citations;
use super::*;
use crate::entity::agent_assistant_citation as citation;
use desk_diagnose_core::{
    ai_assistant::{citation::for_answer, seam::AssistantReadRoot},
    native_ids::{self, Kind},
};

#[tokio::test]
async fn citations_owner_projection_pins_body_and_scope_and_expired_gc_refunds_only_real_rows() {
    let store = setup().await;
    let policy = AssistantDataPolicy::default();
    store.settings(1, &policy).await.unwrap();
    ensure_quota_on(&store.db, "1").await.unwrap();
    let root = AssistantReadRoot {
        source_id: native_ids::stable(Kind::AssistantSource, &"citation-source"),
        source_revision: 2,
    };
    let text = format!("result 〔来源:{}〕", root.source_id);
    let projection = for_answer("answer", &text, &[root], &Default::default());
    let row = citations::new_row(
        "1",
        "root",
        "answer",
        &text,
        "original-output",
        &projection,
        100,
        200,
    )
    .unwrap();
    let before = quota::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap()
        .metadata_bytes;
    let txn = store.db.begin().await.unwrap();
    citations::write_on(&txn, "1", vec![row], &policy)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let after = quota::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap()
        .metadata_bytes;
    assert!(after > before);
    let answers = vec![("answer".into(), text.clone())];
    assert_eq!(
        citations::load_on(
            &store.db,
            "1",
            "root",
            &answers,
            150,
            desk_agent_protocol::ai_assistant::limits::AssistantConversationLimits::default()
                .citation_page_answers
        )
        .await
        .unwrap()["answer"]
            .citations
            .len(),
        1
    );
    for (actor, conversation, text, time) in [
        ("2", "root", text.clone(), 150),
        ("1", "other", text.clone(), 150),
        ("1", "root", "changed answer".into(), 150),
        ("1", "root", text.clone(), 200),
    ] {
        assert!(
            citations::load_on(
                &store.db,
                actor,
                conversation,
                &[("answer".into(), text)],
                time,
                desk_agent_protocol::ai_assistant::limits::AssistantConversationLimits::default()
                    .citation_page_answers
            )
            .await
            .unwrap()
            .is_empty()
        );
    }
    let outcome = super::super::output_archive::gc::collect_actor(&store.db, 200, 64)
        .await
        .unwrap();
    assert_eq!(outcome.refunded_metadata_bytes, after - before);
    assert!(
        citation::Entity::find()
            .all(&store.db)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        quota::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .metadata_bytes,
        before
    );
    assert_eq!(
        super::super::output_archive::gc::collect_actor(&store.db, 200, 64)
            .await
            .unwrap()
            .refunded_metadata_bytes,
        0
    );
}

#[tokio::test]
async fn citations_quota_failure_and_outer_rollback_do_not_publish_partial_metadata() {
    let store = setup().await;
    let mut policy = AssistantDataPolicy::default();
    store.settings(1, &policy).await.unwrap();
    ensure_quota_on(&store.db, "1").await.unwrap();
    let root = AssistantReadRoot {
        source_id: native_ids::stable(Kind::AssistantSource, &"bounded-citation"),
        source_revision: 1,
    };
    let text = format!("〔来源:{}〕", root.source_id);
    let projection = for_answer("answer", &text, &[root], &Default::default());
    let row = citations::new_row(
        "1",
        "root",
        "answer",
        &text,
        "output",
        &projection,
        100,
        200,
    )
    .unwrap();
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    policy.max_metadata_bytes = before.metadata_bytes as u64 + 1;
    let txn = store.db.begin().await.unwrap();
    assert!(
        citations::write_on(&txn, "1", vec![row.clone()], &policy)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    assert!(
        citation::Entity::find()
            .all(&store.db)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        quota::Entity::find().one(&store.db).await.unwrap().unwrap(),
        before
    );
    let txn = store.db.begin().await.unwrap();
    citations::write_on(&txn, "1", vec![row], &AssistantDataPolicy::default())
        .await
        .unwrap();
    txn.rollback().await.unwrap();
    assert!(
        citation::Entity::find()
            .all(&store.db)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        quota::Entity::find().one(&store.db).await.unwrap().unwrap(),
        before
    );
}

#[tokio::test]
async fn citations_corrupt_optional_metadata_stays_inert_without_refunds_or_immediate_rescan() {
    let store = setup().await;
    let policy = AssistantDataPolicy::default();
    store.settings(1, &policy).await.unwrap();
    ensure_quota_on(&store.db, "1").await.unwrap();
    let root = AssistantReadRoot {
        source_id: native_ids::stable(Kind::AssistantSource, &"corrupt-citation"),
        source_revision: 1,
    };
    let text = format!("〔来源:{}〕", root.source_id);
    let row = citations::new_row(
        "1",
        "root",
        "answer",
        &text,
        "output",
        &for_answer("answer", &text, &[root], &Default::default()),
        100,
        200,
    )
    .unwrap();
    let txn = store.db.begin().await.unwrap();
    citations::write_on(&txn, "1", vec![row], &policy)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    citation::Entity::update_many()
        .col_expr(
            citation::Column::CitationsJson,
            sea_orm::sea_query::Expr::value("not a projection"),
        )
        .exec(&store.db)
        .await
        .unwrap();
    assert!(
        citations::load_on(
            &store.db,
            "1",
            "root",
            &[("answer".into(), text)],
            150,
            desk_agent_protocol::ai_assistant::limits::AssistantConversationLimits::default()
                .citation_page_answers
        )
        .await
        .unwrap()
        .is_empty()
    );
    let plan = citations::collect_expired_on(&store.db, "1", 200, 1)
        .await
        .unwrap();
    assert_eq!(plan.invalid, 1);
    assert_eq!(plan.refunded_bytes, 0);
    assert!(!plan.has_more);
    assert_eq!(
        citation::Entity::find().all(&store.db).await.unwrap().len(),
        1
    );
    assert_eq!(
        quota::Entity::find().one(&store.db).await.unwrap().unwrap(),
        before
    );
}

#[tokio::test]
async fn larger_citations_survive_lower_projection_limits_and_configured_load_batches() {
    let store = setup().await;
    let mut policy = AssistantDataPolicy::default();
    policy.limits.conversation.citations = 80;
    policy.limits.conversation.citation_bytes = 131072;
    store.settings(1, &policy).await.unwrap();
    ensure_quota_on(&store.db, "1").await.unwrap();
    let roots = (0..80)
        .map(|index| AssistantReadRoot {
            source_id: native_ids::stable(Kind::AssistantSource, &index),
            source_revision: 1,
        })
        .collect::<Vec<_>>();
    let text = roots
        .iter()
        .map(|root| format!("〔来源:{}〕", root.source_id))
        .collect::<String>();
    let mut rows = Vec::new();
    let mut answers = Vec::new();
    for index in 0..101 {
        let answer = format!("answer-{index}");
        let projection = for_answer(&answer, &text, &roots, &policy.limits.conversation);
        assert_eq!(projection.citations.len(), 80);
        rows.push(
            citations::new_row(
                "1",
                "root",
                &answer,
                &text,
                &format!("output-{index}"),
                &projection,
                100,
                200,
            )
            .unwrap(),
        );
        answers.push((answer, text.clone()));
    }
    let txn = store.db.begin().await.unwrap();
    citations::write_on(&txn, "1", rows, &policy).await.unwrap();
    txn.commit().await.unwrap();
    let current = crate::assistant_data_config::policy(&store.db).await;
    let mut policy = current.clone();
    policy.limits.conversation.citations = 1;
    policy.limits.conversation.citation_bytes = 1;
    policy.limits.conversation.citation_page_answers = 7;
    let lowered = crate::assistant_data_config::update_policy(
        &store.db,
        &desk_agent_protocol::ai_assistant::user_instructions::AssistantDataPolicyUpdate {
            expected_revision: current.revision,
            policy,
        },
    )
    .await
    .unwrap()
    .policy;
    let loaded = citations::load_on(
        &store.db,
        "1",
        "root",
        &answers,
        150,
        lowered.limits.conversation.citation_page_answers,
    )
    .await
    .unwrap();
    assert_eq!(loaded.len(), 101);
    assert!(
        loaded
            .values()
            .all(|value| value.citations.len() == 80 && !value.incomplete)
    );
}
