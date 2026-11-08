use super::super::keyword_search::{self, HistorySearchAuthorization};
use super::model_sources::{access, destination};
use super::*;
use crate::entity::{agent_assistant_history as history, agent_assistant_origin as origin};
use desk_agent_protocol::ai_assistant::{
    history::{
        AssistantDeviceScope, ConversationSearchRequest, HistoryTimeField, HistoryTimeRange,
    },
    source::{AssistantSourceKind, HistoryDependency, SourceAvailability, SourceUsePolicy},
};
use desk_diagnose_core::{
    ai_assistant::{
        data_access::{AssistantDataToolAccess, owner_history_scope},
        source::{HistoryCapture, HistoryRetentionProof, owner_text_policy},
    },
    model_message_labels::model_bound_user_message,
    native_ids::{self, Kind},
};
use sea_orm::sea_query::Expr;
use std::{collections::BTreeSet, time::Duration};

pub(super) struct Frame {
    pub(super) access: AssistantDataToolAccess,
    pub(super) policy: AssistantDataPolicy,
    pub(super) scope: desk_agent_protocol::ai_assistant::history::HistoryAccessScope,
    pub(super) device: String,
    pub(super) hash: String,
    pub(super) generation: u64,
    pub(super) destination: desk_agent_protocol::data_lineage::DestinationIdentity,
}
impl Frame {
    pub(super) async fn new(store: &TestStore, device: &str) -> Self {
        let access = access(store).await;
        let scope = *owner_history_scope(
            "1",
            "current-root",
            device,
            10,
            1000,
            "episode",
            1,
            &access.settings,
            "UTC",
            false,
        )
        .unwrap()
        .unwrap();
        Self {
            access,
            scope,
            device: device.into(),
            hash: "a".repeat(64),
            policy: AssistantDataPolicy::default(),
            generation: unsigned(
                runtime_on(&store.db, "1")
                    .await
                    .unwrap()
                    .history_data_generation,
            )
            .unwrap(),
            destination: destination("current-model"),
        }
    }
    pub(super) fn borrowed(&self) -> HistorySearchAuthorization<'_> {
        HistorySearchAuthorization {
            actor_id: "1",
            current_device_ref: &self.device,
            scope_sha256: &self.hash,
            scope: &self.scope,
            access: &self.access,
            platform: &self.policy,
            destination: &self.destination,
            corpus_generation: self.generation,
            now_unix_ms: 5000,
        }
    }
}
fn request() -> ConversationSearchRequest {
    serde_json::from_value(
        serde_json::json!({ "queries": ["REPORT 邮件"], "time_range": null, "cursor": null }),
    )
    .unwrap()
}
pub(super) async fn device(store: &TestStore, label: &str) -> String {
    let (_, _, original) = super::owner_memory::seed(store, 1, label).await;
    origin::Entity::find()
        .filter(origin::Column::OriginId.eq(original))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap()
        .device_ref
}
pub(super) async fn capture(
    store: &TestStore,
    device: &str,
    root: &str,
    label: &str,
    text: &str,
    dependencies: &[HistoryDependency],
) -> String {
    capture_at(store, device, root, label, text, dependencies, 1000).await
}
pub(super) async fn capture_at(
    store: &TestStore,
    device: &str,
    root: &str,
    label: &str,
    text: &str,
    dependencies: &[HistoryDependency],
    recorded_at: u64,
) -> String {
    let sources =
        capture_many_at(store, device, root, label, text, dependencies, recorded_at).await;
    assert_eq!(sources.len(), 1);
    sources[0].clone()
}
pub(super) async fn capture_many_at(
    store: &TestStore,
    device: &str,
    root: &str,
    label: &str,
    text: &str,
    dependencies: &[HistoryDependency],
    recorded_at: u64,
) -> Vec<String> {
    capture_many_native_at(
        store,
        device,
        root,
        label,
        text,
        dependencies,
        recorded_at,
        None,
    )
    .await
}
#[allow(clippy::too_many_arguments)]
pub(super) async fn capture_many_native_at(
    store: &TestStore,
    device: &str,
    root: &str,
    label: &str,
    text: &str,
    dependencies: &[HistoryDependency],
    recorded_at: u64,
    native: Option<&desk_diagnose_core::ai_assistant::native_history::NativeHistoryMetadata>,
) -> Vec<String> {
    let policy = AssistantDataPolicy::default();
    let mut source_policy = owner_text_policy(1, false);
    if native.is_some() {
        source_policy.class =
            desk_agent_protocol::ai_assistant::source::SourcePolicyClass::CommunicationReceipt;
        source_policy.independent_fact_allowed = false;
    }
    let native_id = native_ids::stable(Kind::Message, &(label, root));
    let message =
        model_bound_user_message(native_id, text.into(), destination("original-model")).unwrap();
    let state = runtime_on(&store.db, "1").await.unwrap();
    let config = store.db.config_read().await;
    let binding = crate::entity::agent_assistant_device_binding::Entity::find()
        .filter(crate::entity::agent_assistant_device_binding::Column::BindingId.eq(device))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let txn = store.db.begin().await.unwrap();
    ensure_runtime_on(&txn, "1").await.unwrap();
    let outcome = super::super::history::capture_on(
        &txn,
        &HistoryCapture {
            native,
            actor_id: "1",
            device_ref: device,
            directory: Some(
                desk_diagnose_core::ai_assistant::source::HistorySourceDirectory {
                    instance_id: &config.metadata.instance,
                    audience: &binding.audience,
                    device_name: binding.device_name.as_deref(),
                },
            ),
            conversation_id: root,
            root_conversation_id: root,
            native_id: &message.message_id,
            kind: if native.is_some() {
                AssistantSourceKind::Receipt
            } else {
                AssistantSourceKind::Input
            },
            role: if native.is_some() { "tool" } else { "user" },
            accepted_input_seq: state.input_seq as u64,
            content: text,
            envelope: message.data_envelope.as_ref().unwrap(),
            source_policy: &source_policy,
            dependencies,
            recorded_at_unix_ms: recorded_at,
            statement_timezone: "UTC",
            revision: 1,
        },
        &policy,
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    outcome.source_ids
}
pub(super) async fn private_source(store: &TestStore, source: &str) {
    let row = history::Entity::find()
        .filter(history::Column::SourceId.eq(source))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let original = origin::Entity::find()
        .filter(origin::Column::OriginId.eq(&row.origin_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut policy: SourceUsePolicy = serde_json::from_str(&original.policy_json).unwrap();
    policy.no_export = true;
    let mut proof: HistoryRetentionProof = serde_json::from_str(&row.retention_proof_json).unwrap();
    proof.source_policy = policy.clone();
    origin::Entity::update_many()
        .col_expr(
            origin::Column::PolicyJson,
            Expr::value(serde_json::to_string(&policy).unwrap()),
        )
        .filter(origin::Column::OriginId.eq(&row.origin_id))
        .exec(&store.db)
        .await
        .unwrap();
    history::Entity::update_many()
        .col_expr(
            history::Column::RetentionProofJson,
            Expr::value(serde_json::to_string(&proof).unwrap()),
        )
        .filter(history::Column::SourceId.eq(source))
        .exec(&store.db)
        .await
        .unwrap();
}

#[tokio::test]
async fn actual_index_pages_normalize_queries_and_keep_current_and_all_device_scopes_separate() {
    let store = setup().await;
    let a = device(&store, "keyword-A").await;
    let b = device(&store, "keyword-B").await;
    let source_a = capture(&store, &a, "root-A", "A", "REPORT 邮件 已发送", &[]).await;
    let source_b = capture(&store, &b, "root-B", "B", "report 邮件 相关文件", &[]).await;
    capture(
        &store,
        &a,
        "current-root",
        "current",
        "report 邮件 current conversation",
        &[],
    )
    .await;
    let frame = Frame::new(&store, &a).await;
    let devices = BTreeSet::from([a, b]);
    let mut query = request();
    query.queries.push("邮件 report".into());
    let txn = store.db.begin().await.unwrap();
    let result = keyword_search::scan_on(
        &txn,
        &frame.borrowed(),
        &query,
        &devices,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert_eq!(result.ranking.effective_queries.len(), 1);
    assert_eq!(result.ranking.documents.len(), 1);
    assert_eq!(result.ranking.documents[0].document.source_id, source_a);
    assert!(result.reasons.contains(&"candidate_statistics".into()));
    query.scope = AssistantDeviceScope::AllMyDevices;
    let result = keyword_search::scan_on(
        &txn,
        &frame.borrowed(),
        &query,
        &devices,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert_eq!(
        result
            .ranking
            .documents
            .iter()
            .map(|value| value.document.source_id.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([source_a, source_b.clone()])
    );
    query.conversation_ids = vec!["root-B".into()];
    let selected = keyword_search::scan_on(
        &txn,
        &frame.borrowed(),
        &query,
        &devices,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert_eq!(selected.ranking.documents.len(), 1);
    assert_eq!(selected.ranking.documents[0].document.source_id, source_b);
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn owner_viewable_no_export_sources_never_participate_in_df_lengths_or_ranking() {
    let store = setup().await;
    let a = device(&store, "keyword-private").await;
    capture(
        &store,
        &a,
        "allowed-one",
        "allowed-one",
        "report email 邮件",
        &[],
    )
    .await;
    capture(&store, &a, "allowed-two", "allowed-two", "report 邮件", &[]).await;
    let hidden = capture(
        &store,
        &a,
        "private-root",
        "private",
        &"report 邮件 ".repeat(200),
        &[],
    )
    .await;
    private_source(&store, &hidden).await;
    assert_eq!(
        store
            .owner_source(1, &hidden, &AssistantDataPolicy::default(), 5000)
            .await
            .unwrap()
            .availability,
        SourceAvailability::Available
    );
    let frame = Frame::new(&store, &a).await;
    let devices = BTreeSet::from([a]);
    let txn = store.db.begin().await.unwrap();
    let result = keyword_search::scan_on(
        &txn,
        &frame.borrowed(),
        &request(),
        &devices,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert_eq!(result.ranking.statistics_document_count, 2);
    assert_eq!(result.ranking.documents.len(), 2);
    assert!(
        result
            .ranking
            .documents
            .iter()
            .all(|candidate| candidate.document.source_id != hidden)
    );
    assert!(result.reasons.contains(&"source_unavailable".into()));
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn original_dependency_deletion_blocks_a_matching_derived_source() {
    let store = setup().await;
    let a = device(&store, "keyword-dependencies").await;
    let original = capture(&store, &a, "original", "original", "source background", &[]).await;
    let frame = Frame::new(&store, &a).await;
    let row = history::Entity::find()
        .filter(history::Column::SourceId.eq(&original))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let source = keyword_search::readable_history_on(&store.db, &frame.borrowed(), &original, 1)
        .await
        .unwrap();
    capture(
        &store,
        &a,
        "derived",
        "derived",
        "report 邮件 derived conclusion",
        &source.usage.history_dependencies,
    )
    .await;
    origin::Entity::update_many()
        .col_expr(origin::Column::DeletedAtMs, Expr::value(Some(2000_i64)))
        .filter(origin::Column::OriginId.eq(row.origin_id))
        .exec(&store.db)
        .await
        .unwrap();
    let frame = Frame::new(&store, &a).await;
    let txn = store.db.begin().await.unwrap();
    let result = keyword_search::scan_on(
        &txn,
        &frame.borrowed(),
        &request(),
        &BTreeSet::from([a]),
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert!(result.ranking.documents.is_empty());
    assert_eq!(result.ranking.statistics_document_count, 0);
    assert!(result.reasons.contains(&"source_unavailable".into()));
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn work_and_time_caps_are_partial_and_snapshot_watermarks_do_not_include_new_sources() {
    let store = setup().await;
    let a = device(&store, "keyword-caps").await;
    let first = capture(&store, &a, "first", "first", "report 邮件", &[]).await;
    let old_frame = Frame::new(&store, &a).await;
    for index in 0..41 {
        capture(
            &store,
            &a,
            &format!("root-{index}"),
            &format!("source-{index}"),
            "report 邮件",
            &[],
        )
        .await;
    }
    let devices = BTreeSet::from([a.clone()]);
    let query = request();
    let txn = store.db.begin().await.unwrap();
    let old = keyword_search::scan_on(
        &txn,
        &old_frame.borrowed(),
        &query,
        &devices,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert_eq!(old.ranking.documents.len(), 1);
    assert_eq!(old.ranking.documents[0].document.source_id, first);
    txn.rollback().await.unwrap();
    let mut frame = Frame::new(&store, &a).await;
    frame.policy.max_retrieval_document_visits = 40;
    let txn = store.db.begin().await.unwrap();
    let bounded = keyword_search::scan_on(
        &txn,
        &frame.borrowed(),
        &query,
        &devices,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert_eq!(bounded.ranking.statistics_document_count, 40);
    assert!(bounded.reasons.contains(&"document_work_limit".into()));
    let empty = keyword_search::scan_on(&txn, &frame.borrowed(), &query, &devices, Duration::ZERO)
        .await
        .unwrap();
    assert!(empty.ranking.documents.is_empty());
    assert!(empty.reasons.contains(&"search_time_limit".into()));
    frame.access.settings.model_history_search_enabled = false;
    assert!(
        keyword_search::scan_on(
            &txn,
            &frame.borrowed(),
            &query,
            &devices,
            Duration::from_secs(60)
        )
        .await
        .is_err()
    );
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn an_individual_selected_source_rechecks_current_control_generations() {
    let store = setup().await;
    let a = device(&store, "keyword-final-control").await;
    let source = capture(&store, &a, "source", "source", "report 邮件", &[]).await;
    let frame = Frame::new(&store, &a).await;
    assert!(
        keyword_search::readable_history_on(&store.db, &frame.borrowed(), &source, 1)
            .await
            .is_ok()
    );
    let old = runtime_on(&store.db, "1").await.unwrap();
    runtime::Entity::update_many()
        .col_expr(
            runtime::Column::ControlGeneration,
            Expr::value(increment(old.control_generation).unwrap()),
        )
        .filter(runtime::Column::ActorId.eq("1"))
        .exec(&store.db)
        .await
        .unwrap();
    assert!(
        keyword_search::readable_history_on(&store.db, &frame.borrowed(), &source, 1)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn native_action_ranges_keep_intervals_and_do_not_fall_back_to_recorded_time() {
    let store = setup().await;
    let a = device(&store, "keyword-native-time").await;
    let metadata = receipt_metadata("report 邮件");
    let interval = capture_many_native_at(
        &store,
        &a,
        "interval",
        "interval",
        "report 邮件",
        &[],
        199,
        Some(&metadata),
    )
    .await
    .remove(0);
    capture_at(&store, &a, "unknown", "unknown", "report 邮件", &[], 150).await;
    let frame = Frame::new(&store, &a).await;
    let mut query = request();
    query.time_field = HistoryTimeField::NativeActionAt;
    query.time_range = Some(HistoryTimeRange {
        from_unix_ms: 120,
        until_unix_ms: 180,
    });
    let txn = store.db.begin().await.unwrap();
    let result = keyword_search::scan_on(
        &txn,
        &frame.borrowed(),
        &query,
        &BTreeSet::from([a]),
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert_eq!(result.ranking.documents.len(), 1);
    assert_eq!(result.ranking.documents[0].document.source_id, interval);
    assert!(
        result
            .reasons
            .contains(&"native_action_time_interval".into())
    );
    txn.rollback().await.unwrap();
}

fn receipt_metadata(
    text: &str,
) -> desk_diagnose_core::ai_assistant::native_history::NativeHistoryMetadata {
    use desk_diagnose_core::ai_assistant::native_history::{
        NativeHistoryMetadata, NativeHistoryTime, NativeTimeBasis,
    };
    NativeHistoryMetadata {
        format_version: 1,
        native_proof_sha256: "a".repeat(64),
        projected_content_sha256: format!("{:x}", Sha256::digest(text.as_bytes())),
        projection_partial: false,
        time: Some(NativeHistoryTime::Interval {
            from_unix_ms: 100,
            until_unix_ms: 200,
            basis: NativeTimeBasis::ProviderAckObserved,
        }),
        status_snapshot: None,
    }
}

#[tokio::test]
async fn indexed_native_columns_and_state_cannot_be_changed_without_the_original_retention_proof() {
    for case in 0..3 {
        let store = setup().await;
        let device = device(&store, &format!("native-metadata-tamper-{case}")).await;
        let metadata = receipt_metadata("report 邮件");
        let source = capture_many_native_at(
            &store,
            &device,
            "native-root",
            "native-time",
            "report 邮件",
            &[],
            199,
            Some(&metadata),
        )
        .await
        .remove(0);
        let frame = Frame::new(&store, &device).await;
        assert!(
            keyword_search::readable_history_on(&store.db, &frame.borrowed(), &source, 1)
                .await
                .is_ok()
        );
        let mut change =
            history::Entity::update_many().filter(history::Column::SourceId.eq(&source));
        change = match case {
            0 => change.col_expr(
                history::Column::NativeActionAtMs,
                Expr::value(Some(150_i64)),
            ),
            1 => change.col_expr(
                history::Column::TimeBasis,
                Expr::value(Some("native_recorded".to_string())),
            ),
            _ => change.col_expr(
                history::Column::StateJson,
                Expr::value(Option::<String>::None),
            ),
        };
        change.exec(&store.db).await.unwrap();
        assert!(
            keyword_search::readable_history_on(&store.db, &frame.borrowed(), &source, 1)
                .await
                .is_err()
        );
        assert_eq!(
            store
                .owner_source(1, &source, &AssistantDataPolicy::default(), 5000)
                .await
                .unwrap()
                .availability,
            SourceAvailability::Unavailable
        );
    }
}

// Fixed lexical recall corpus, not a model-accuracy or business-success claim.
// All five references below are owner statements; a matching email discussion
// cannot be returned as an authenticated send receipt or completed native task.
#[tokio::test]
async fn fixed_owner_question_corpus_retrieves_original_discussions_at_the_history_capacity() {
    #[derive(serde::Deserialize)]
    struct Case {
        root: String,
        text: String,
        question: String,
        queries: Vec<String>,
        low_overlap: bool,
        bilingual: bool,
    }
    let cases: Vec<Case> = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../diagnose-core/tests/fixtures/assistant-retrieval-cases.json"
    )))
    .unwrap();
    assert_eq!(cases.len(), 60);
    assert_eq!(cases.iter().filter(|case| case.low_overlap).count(), 24);
    assert_eq!(cases.iter().filter(|case| case.bilingual).count(), 12);
    let store = setup().await;
    let device = device(&store, "scenario-device").await;
    let mut references = std::collections::BTreeMap::new();
    for case in &cases {
        if !references.contains_key(&case.root) {
            let source = capture_at(
                &store,
                &device,
                &case.root,
                &case.root,
                &case.text,
                &[],
                2000,
            )
            .await;
            references.insert(case.root.clone(), source);
        }
    }
    for index in references.len()..10_000 {
        capture_at(
            &store,
            &device,
            &format!("noise-root-{index}"),
            &format!("noise-{index}"),
            "普通会话 帮助日常设置 unrelated ordinary settings",
            &[],
            1000,
        )
        .await;
    }
    let frame = Frame::new(&store, &device).await;
    let devices = BTreeSet::from([device]);
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    assert_eq!(before.history_records, 10_000);
    let mut elapsed = Vec::new();
    let mut reciprocal = 0_f64;
    for case in &cases {
        let mut query = request();
        query.queries = case.queries.clone();
        query.limit = 5;
        let txn = store.db.begin().await.unwrap();
        let started = std::time::Instant::now();
        let result = keyword_search::scan_on(
            &txn,
            &frame.borrowed(),
            &query,
            &devices,
            Duration::from_millis(500),
        )
        .await
        .unwrap();
        elapsed.push(started.elapsed().as_micros());
        let rank = result
            .ranking
            .documents
            .iter()
            .take(5)
            .position(|hit| &hit.document.source_id == references.get(&case.root).unwrap());
        assert!(rank.is_some(), "{}: {:?}", case.question, case.queries);
        reciprocal += 1.0 / (rank.unwrap() + 1) as f64;
        assert!(result.ranking.documents.len() <= 20);
        txn.rollback().await.unwrap();
    }
    assert_eq!(
        quota::Entity::find().one(&store.db).await.unwrap().as_ref(),
        Some(&before)
    );
    elapsed.sort_unstable();
    eprintln!(
        "assistant_search_fixture documents=10000 gold_queries=60 low_overlap_prompts=24 bilingual=12 recall_at_5=60/60 mrr={} p95_us={} max_us={} budget_ms=500 model_calls=0",
        reciprocal / 60.0,
        elapsed[56],
        elapsed[59]
    );
}
