use super::*;
use crate::assistant_data_store::{owner_source, visible_history};
use crate::entity::{
    agent_assistant_history as history, agent_assistant_history_coverage as coverage,
    agent_assistant_origin as origins,
};
use desk_agent_protocol::ai_assistant::{
    source::{HistoryDependency, SourceAvailability},
    user_instructions::AssistantDataSettingsUpdate,
};
use desk_agent_protocol::data_lineage::DataEnvelope;
use desk_diagnose_core::chat::{ChatMessage, ChatRole};

const TEXT: &str = "Independent answer without retrieved memory";

#[tokio::test]
async fn a_rotated_lease_rejects_the_visible_save_before_any_new_history_or_coverage_is_committed()
{
    let (store, mut session, _) = fixture("visible-rotated-lease", true).await;
    let old = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut current = PersistedAgentSession::decode_json(&old.state_json).unwrap();
    current.lease_token += 1;
    agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::LeaseToken,
            Expr::value(current.lease_token as i64),
        )
        .col_expr(
            agent_session::Column::StateJson,
            Expr::value(current.encode_json_for_storage().unwrap()),
        )
        .filter(agent_session::Column::Id.eq(old.id))
        .exec(&store.db)
        .await
        .unwrap();
    let takeover = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    assert!(host(&store).save(&mut session).await.is_err());
    assert_eq!(session.version, old.version);
    assert_eq!(
        agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        takeover
    );
    assert_eq!(
        quota::Entity::find().one(&store.db).await.unwrap().unwrap(),
        before
    );
    assert!(answer(&store).await.is_none());
    assert_eq!(
        coverage::Entity::find()
            .filter(coverage::Column::Reason.ne("indexed"))
            .filter(coverage::Column::SourceCount.gt(0))
            .count(&store.db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn an_original_owner_export_to_another_model_keeps_the_same_logical_owner_source() {
    let (store, mut session, _) =
        fixture_with_export("visible-original-owner-export", true, true).await;
    host(&store).save(&mut session).await.unwrap();
    let row = answer(&store).await.unwrap();
    let source = origins::Entity::find()
        .filter(origins::Column::OriginId.eq(&row.origin_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let policy: desk_agent_protocol::ai_assistant::source::SourceUsePolicy =
        serde_json::from_str(&source.policy_json).unwrap();
    assert!(policy.fixed_destinations.is_empty());
    assert_eq!(row.content, TEXT);
    let dependencies: Vec<HistoryDependency> =
        serde_json::from_str(&row.dependencies_json).unwrap();
    let input = history::Entity::find()
        .filter(history::Column::NativeId.eq("plain-owner-input"))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(dependencies.is_empty());
    assert!(!input.source_id().is_empty());
}

#[tokio::test]
async fn a_revoked_original_input_does_not_withdraw_an_already_accepted_answer() {
    let (store, mut session, _) = fixture("visible-revoked-input", true).await;
    let original = origins::Entity::find()
        .filter(origins::Column::NativeId.eq("plain-owner-input"))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    // Exercise the source-revocation boundary, not the whole conversation-delete
    // workflow. The accepted native result and its actual usage are unchanged.
    origins::Entity::update_many()
        .col_expr(
            origins::Column::DeletedAtMs,
            Expr::value(Some(chrono::Utc::now().timestamp_millis())),
        )
        .col_expr(
            origins::Column::DeletionGeneration,
            Expr::value(original.deletion_generation + 1),
        )
        .filter(origins::Column::Id.eq(original.id))
        .exec(&store.db)
        .await
        .unwrap();
    host(&store).save(&mut session).await.unwrap();
    assert!(answer(&store).await.is_some());
}

async fn fixture(
    label: &str,
    capture_input: bool,
) -> (TestStore, Box<PersistedAgentSession>, DataEnvelope) {
    fixture_with_export(label, capture_input, false).await
}

async fn fixture_with_export(
    label: &str,
    capture_input: bool,
    export: bool,
) -> (TestStore, Box<PersistedAgentSession>, DataEnvelope) {
    let (store, mut session, _, candidate, _) = prepare_plain(label, export).await;
    if capture_input {
        capture_plain_input(&store, &session).await;
    }
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("plain-native", &candidate)
        .await
        .unwrap();
    let mut message = ChatMessage::text("actual-visible-answer", ChatRole::Assistant, TEXT);
    message.turn_id = session.current_turn_id.clone();
    message.data_envelope = Some(candidate.clone());
    session.conversation.push(message);
    (store, session, candidate)
}

fn host(store: &TestStore) -> crate::agent_session_store::SignalAgentSessionStore {
    crate::agent_session_store::SignalAgentSessionStore::new(store.db.clone())
}

async fn answer(store: &TestStore) -> Option<history::Model> {
    history::Entity::find()
        .filter(history::Column::Kind.eq("assistant_message"))
        .one(&store.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn actual_session_save_retains_visible_body_native_time_and_independent_lifetime() {
    let (store, mut session, output) = fixture("visible-save", true).await;
    let original = output_archive::find_on(&store.db, "root", &output)
        .await
        .unwrap()
        .unwrap();
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    let old_version = session.version;
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
    host(&store).save(&mut session).await.unwrap();
    assert_eq!(session.version, old_version + 1);
    let row = answer(&store).await.unwrap();
    assert_eq!(
        (
            row.native_id.as_str(),
            row.role.as_str(),
            row.content.as_str()
        ),
        ("actual-visible-answer", "assistant", TEXT)
    );
    assert_eq!(row.recorded_at_ms, original.row.completed_at_ms);
    assert_eq!(row.statement_timezone, original.proof.statement_timezone);
    assert_eq!(
        quota::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .history_records,
        before.history_records + 2
    );
    let dependencies: Vec<HistoryDependency> =
        serde_json::from_str(&row.dependencies_json).unwrap();
    assert!(dependencies.is_empty());

    assert_eq!(
        row.expires_at_ms,
        row.recorded_at_ms
            + i64::from(AssistantDataPolicy::default().history_retention_days) * 86_400_000
    );
    let view = owner_source::history_source_on(
        &store.db,
        "1",
        &row,
        &AssistantDataPolicy::default(),
        chrono::Utc::now().timestamp_millis() as u64,
    )
    .await
    .unwrap();
    assert_eq!(view.availability, SourceAvailability::Available);
    assert_eq!(
        view.locator.unwrap().message_id.as_deref(),
        Some("actual-visible-answer")
    );
    let counted = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    session.finish_turn(
        desk_diagnose_core::session::TurnState::Idle,
        chrono::Utc::now().to_rfc3339(),
    );
    host(&store).save(&mut session).await.unwrap();
    assert_eq!(
        quota::Entity::find().one(&store.db).await.unwrap().unwrap(),
        counted
    );
    assert_eq!(
        history::Entity::find()
            .filter(history::Column::Kind.eq("assistant_message"))
            .count(&store.db)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn missing_original_input_identity_records_a_gap_without_promoting_model_labels_to_owner_history()
 {
    let (store, mut session, _) = fixture("visible-missing-input", false).await;
    let original = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    host(&store).save(&mut session).await.unwrap();
    assert!(answer(&store).await.is_none());
    assert_eq!(
        quota::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .history_records,
        original.history_records + 1 // independently authenticated native inventory
    );
    assert_eq!(
        history::Entity::find()
            .filter(history::Column::Kind.eq("native_status"))
            .count(&store.db)
            .await
            .unwrap(),
        1
    );
    let gap = coverage::Entity::find()
        .filter(coverage::Column::Reason.eq("omitted_policy"))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(gap.source_count, 1);
    let saved = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let decoded = PersistedAgentSession::decode_json(&saved.state_json).unwrap();
    assert_eq!(decoded.conversation.last().unwrap().text, TEXT);
    host(&store).save(&mut session).await.unwrap();
    assert_eq!(
        coverage::Entity::find_by_id(gap.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .source_count,
        1
    );
}

#[tokio::test]
async fn history_capacity_and_late_index_failure_preserve_the_real_saved_answer_and_only_record_their_own_gaps()
 {
    for capacity in [true, false] {
        let (store, mut session, _) = fixture(
            if capacity {
                "visible-capacity"
            } else {
                "visible-index-failure"
            },
            true,
        )
        .await;
        let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
        if capacity {
            let mut policy = crate::assistant_data_config::policy(&store.db).await;
            policy.max_history_records = before.history_records as u64;
            crate::assistant_data_config::update_policy(
                &store.db,
                &desk_agent_protocol::ai_assistant::user_instructions::AssistantDataPolicyUpdate {
                    expected_revision: policy.revision,
                    policy,
                },
            )
            .await
            .unwrap();
        } else {
            store.db.execute_unprepared("CREATE TRIGGER reject_visible_history_index BEFORE INSERT ON agent_assistant_search_document WHEN NEW.source_kind='history' AND EXISTS (SELECT 1 FROM agent_assistant_history h WHERE h.id=NEW.history_id AND h.kind='assistant_message') BEGIN SELECT RAISE(ABORT, 'injected visible history index failure'); END").await.unwrap();
        }
        let old_version = session.version;
        host(&store).save(&mut session).await.unwrap();
        assert_eq!(session.version, old_version + 1);
        assert!(answer(&store).await.is_none());
        let after = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
        if capacity {
            assert_eq!(after, before);
        } else {
            // The injected failure targets the answer only. Its independent
            // native inventory is admitted and charged once.
            let status = history::Entity::find()
                .filter(history::Column::Kind.eq("native_status"))
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(after.history_records, before.history_records + 1);
            assert_eq!(
                after.history_body_bytes,
                before.history_body_bytes + status.content.len() as i64
            );
            assert_eq!(after.memory_records, before.memory_records);
            assert_eq!(after.memory_body_bytes, before.memory_body_bytes);
            assert_eq!(after.revision, before.revision + 1);
        }
        let reason = if capacity {
            "omitted_capacity"
        } else {
            "omitted_error"
        };
        assert_eq!(
            coverage::Entity::find()
                .filter(coverage::Column::Reason.eq(reason))
                .one(&store.db)
                .await
                .unwrap()
                .unwrap()
                .source_count,
            if capacity { 2 } else { 1 } // the index fault targets only the answer
        );
        let native = native::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let usage: TokenUsage =
            serde_json::from_str(native.usage_json.as_deref().unwrap()).unwrap();
        assert_eq!(
            (usage.input_tokens, usage.output_tokens),
            (Some(12), Some(4))
        );
    }
}

#[tokio::test]
async fn failure_to_record_the_coverage_gap_rolls_back_the_original_session_cas_and_history_side_effects()
 {
    let (store, mut session, _) = fixture("visible-final-gap-failure", true).await;
    let before = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let quota = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    store.db.execute_unprepared("CREATE TRIGGER reject_visible_history BEFORE INSERT ON agent_assistant_history WHEN NEW.kind='assistant_message' BEGIN SELECT RAISE(ABORT, 'injected visible history failure'); END").await.unwrap();
    store.db.execute_unprepared("CREATE TRIGGER reject_visible_gap BEFORE UPDATE ON agent_assistant_history_coverage WHEN NEW.reason='omitted_error' BEGIN SELECT RAISE(ABORT, 'injected gap failure'); END").await.unwrap();
    assert!(host(&store).save(&mut session).await.is_err());
    assert_eq!(session.version, before.version);
    assert_eq!(
        agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        before
    );
    assert_eq!(
        quota::Entity::find().one(&store.db).await.unwrap().unwrap(),
        quota
    );
    assert!(answer(&store).await.is_none());
    let native = native::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let usage: TokenUsage = serde_json::from_str(native.usage_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        (usage.input_tokens, usage.output_tokens),
        (Some(12), Some(4))
    );
}

#[tokio::test]
async fn a_saved_output_cannot_be_rebound_as_a_fresh_message_and_wrong_native_text_is_never_committed()
 {
    let (store, mut session, _) = fixture("visible-no-rebind", true).await;
    let original = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    session
        .conversation
        .last_mut()
        .unwrap()
        .text
        .push_str(" changed");
    assert!(host(&store).save(&mut session).await.is_err());
    assert_eq!(
        agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        original
    );
    session.conversation.last_mut().unwrap().text = TEXT.into();
    host(&store).save(&mut session).await.unwrap();
    let saved = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut replay = session.conversation.last().unwrap().clone();
    replay.message_id = "rebinding-an-old-answer".into();
    session.conversation.push(replay);
    assert!(host(&store).save(&mut session).await.is_err());
    assert_eq!(
        agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        saved
    );
    assert_eq!(
        history::Entity::find()
            .filter(history::Column::Kind.eq("assistant_message"))
            .count(&store.db)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn preparation_preserves_an_exact_raw_json_fence_even_when_a_later_native_write_only_changes_whitespace()
 {
    let (store, session, _) = fixture("visible-raw-json-fence", true).await;
    let txn = crate::db::begin_write(&store.db, agent_session::Entity)
        .await
        .unwrap();
    let prepared = visible_history::prepare_on(&txn, &session).await.unwrap();
    agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::StateJson,
            Expr::value(format!(" {} ", prepared.original.state_json)),
        )
        .filter(agent_session::Column::Id.eq(prepared.original.id))
        .exec(&txn)
        .await
        .unwrap();
    assert!(prepared.finish_preparation_on(&txn).await.is_err());
    txn.rollback().await.unwrap();
    assert!(answer(&store).await.is_none());
}

#[tokio::test]
async fn the_native_session_update_precedes_history_quota_and_no_original_session_write_follows_it()
{
    let (store, mut session, _) = fixture("visible-save-phase-order", true).await;
    for statement in [
        "CREATE TABLE visible_phase_order (phase INTEGER NOT NULL)",
        "INSERT INTO visible_phase_order VALUES (0)",
        "CREATE TRIGGER visible_native_phase AFTER UPDATE ON agent_session BEGIN UPDATE visible_phase_order SET phase=1; END",
        "CREATE TRIGGER visible_quota_after_native BEFORE UPDATE ON agent_assistant_quota WHEN (SELECT phase FROM visible_phase_order)!=1 BEGIN SELECT RAISE(ABORT, 'visible quota before native save'); END",
        "CREATE TRIGGER visible_final_quota_phase AFTER UPDATE ON agent_assistant_quota BEGIN UPDATE visible_phase_order SET phase=2; END",
        "CREATE TRIGGER visible_no_native_after_quota BEFORE UPDATE ON agent_session WHEN (SELECT phase FROM visible_phase_order)>=2 BEGIN SELECT RAISE(ABORT, 'native session write after visible quota'); END",
    ] {
        store.db.execute_unprepared(statement).await.unwrap();
    }
    host(&store).save(&mut session).await.unwrap();
    assert!(answer(&store).await.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn main_delegation_save_keeps_history_atomic_on_small_stacks() {
    const STACK: &str = "LRD_TEST_MAIN_HISTORY_STACK_BYTES";
    let Some(bytes) = std::env::var_os(STACK) else {
        for bytes in [1024 * 1024, 2 * 1024 * 1024] {
            assert!(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "main_delegation_save_keeps_history_atomic_on_small_stacks",
                        "--nocapture",
                        "--test-threads=1"
                    ])
                    .env(STACK, bytes.to_string())
                    .env_remove("RUST_MIN_STACK")
                    .status()
                    .unwrap()
                    .success()
            );
        }
        return;
    };
    let bytes: usize = bytes.to_str().unwrap().parse().unwrap();
    assert!([1024 * 1024, 2 * 1024 * 1024].contains(&bytes));
    for case in 0..3 {
        // Keep schema/model fixtures and their pool runtime outside the bounded
        // product thread. The native save and history run together inside it.
        let (store, session, _) = fixture(&format!("main-history-{bytes}-{case}"), true).await;
        let original = agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let quota_before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
        if case == 1 {
            store.db.execute_unprepared("CREATE TRIGGER reject_main_history BEFORE INSERT ON agent_assistant_history WHEN NEW.kind='assistant_message' BEGIN SELECT RAISE(ABORT, 'injected main history failure'); END").await.unwrap();
            store.db.execute_unprepared("CREATE TRIGGER reject_main_gap BEFORE UPDATE ON agent_assistant_history_coverage WHEN NEW.reason='omitted_error' BEGIN SELECT RAISE(ABORT, 'injected main gap failure'); END").await.unwrap();
        } else if case == 2 {
            for statement in [
                "CREATE TABLE main_history_phase (phase INTEGER NOT NULL)",
                "INSERT INTO main_history_phase VALUES (0)",
                "CREATE TRIGGER main_native_phase AFTER UPDATE ON agent_session BEGIN UPDATE main_history_phase SET phase=1; END",
                "CREATE TRIGGER main_quota_after_native BEFORE UPDATE ON agent_assistant_quota WHEN (SELECT phase FROM main_history_phase)!=1 BEGIN SELECT RAISE(ABORT, 'main quota before native save'); END",
                "CREATE TRIGGER main_final_quota AFTER UPDATE ON agent_assistant_quota BEGIN UPDATE main_history_phase SET phase=2; END",
                "CREATE TRIGGER main_no_native_after_quota BEFORE UPDATE ON agent_session WHEN (SELECT phase FROM main_history_phase)>=2 BEGIN SELECT RAISE(ABORT, 'main write after quota'); END",
            ] {
                store.db.execute_unprepared(statement).await.unwrap();
            }
        }
        let db = store.db.clone();
        let pool_runtime = tokio::runtime::Handle::current();
        let (result, session) = std::thread::Builder::new()
            .name("main-history-stack".into())
            .stack_size(bytes)
            .spawn(move || {
                // Keep connection-return tasks on the pool's live runtime.
                // block_on still polls the product future on this bounded thread.
                pool_runtime.block_on(async move {
                    assert_eq!(std::thread::current().name(), Some("main-history-stack"));
                    let mut session = session;
                    let result = crate::agent_subagent_store::save_main_delegation_session(
                        &db,
                        &mut session,
                    )
                    .await;
                    assert_eq!(std::thread::current().name(), Some("main-history-stack"));
                    if case == 0 && result.is_ok() {
                        let retained = history::Entity::find()
                            .filter(history::Column::Kind.eq("assistant_message"))
                            .one(&db)
                            .await
                            .unwrap()
                            .unwrap();
                        let quota_before_replay =
                            quota::Entity::find().one(&db).await.unwrap().unwrap();
                        crate::agent_subagent_store::save_main_delegation_session(
                            &db,
                            &mut session,
                        )
                        .await
                        .unwrap();
                        assert_eq!(
                            history::Entity::find()
                                .filter(history::Column::Kind.eq("assistant_message"))
                                .one(&db)
                                .await
                                .unwrap()
                                .unwrap(),
                            retained
                        );
                        assert_eq!(
                            quota::Entity::find().one(&db).await.unwrap().unwrap(),
                            quota_before_replay
                        );
                    }
                    (result, session)
                })
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(result.is_err(), case == 1, "case {case}: {result:?}");
        let saved = agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        if case == 1 {
            assert_eq!(saved, original);
            assert_eq!(session.version, original.version);
            assert_eq!(
                quota::Entity::find().one(&store.db).await.unwrap().unwrap(),
                quota_before
            );
            assert!(answer(&store).await.is_none());
        } else {
            assert_eq!(
                session.version,
                original.version + if case == 0 { 2 } else { 1 }
            );
            assert_eq!(saved.version, session.version);
            let retained = answer(&store).await.unwrap();
            assert_eq!(retained.content, TEXT);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ai_task_prompt_keeps_original_copy_policy_and_rolls_back_on_small_stacks() {
    for capture_input in [true, false] {
        for stack_bytes in [1024 * 1024, 2 * 1024 * 1024] {
            let (store, session, output) = fixture("native-ai-task-prompt", capture_input).await;
            let schema = sea_orm::Schema::new(store.db.get_database_backend());
            store
                .db
                .execute(
                    schema
                        .create_table_from_entity(crate::entity::agent_schedule::Entity)
                        .if_not_exists(),
                )
                .await
                .unwrap();
            let draft: desk_agent_protocol::schedule::ScheduleDraft = serde_json::from_value(serde_json::json!({
                "client_create_key": "native-prompt-policy", "kind": "fresh_task", "target_device_id": session.device_id,
                "title": "Task prompt policy", "prompt": "Review the original requirement", "locale": null, "model_id": null,
                "spec": { "schema_version": 1, "rule": { "kind": "daily", "utc_time": "06:00:00" } },
                "source_conversation_id": null, "requirement_revision": null,
                "creation_source": "ai_proposal", "time_confirmation": null
            })).unwrap();
            let mut task = crate::schedule_store::ScheduleStore::new(store.db.clone())
                .create_draft(1, &draft, chrono::Utc::now().timestamp_millis())
                .await
                .unwrap();
            assert!(task.prompt_retention_json.is_none());
            let worker = std::thread::Builder::new().name("ai-task-prompt-small-stack".into()).stack_size(stack_bytes).spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                runtime.block_on(async move {
                    assert_eq!(std::thread::current().name(), Some("ai-task-prompt-small-stack"));
                    let parent = session.conversation.iter().find(|message| message.data_envelope.as_ref() == Some(&output)).unwrap();
                    let txn = store.db.begin().await.unwrap();
                    let mut temporary = session.clone();
                    temporary.assistant_data.as_mut().unwrap().temporary = true;
                    let mut ignored = task.clone();
                    desk_diagnose_core::future::send_phase(|| crate::schedule_store::ScheduleStore::record_prompt_retention_on(
                        &txn, &mut ignored, &temporary, parent)).await.unwrap();
                    assert!(ignored.prompt_retention_json.is_none());
                    desk_diagnose_core::future::send_phase(|| crate::schedule_store::ScheduleStore::record_prompt_retention_on(
                        &txn, &mut task, &session, parent)).await.unwrap();
                    if !capture_input {
                        assert!(task.prompt_retention_json.is_none());
                        txn.rollback().await.unwrap();
                        return;
                    }
                    let raw = task.prompt_retention_json.as_deref().unwrap();
                    let proof: desk_diagnose_core::schedule::published_input::TaskPromptRetention = serde_json::from_str(raw).unwrap();
                    proof.validate(&session.actor_id, &session.device_id).unwrap();
                    assert_eq!(proof.policy.no_copy, !capture_input);
                    assert_eq!(proof.policy.derived_visible_text_allowed, capture_input);
                    assert!(!proof.policy.independent_fact_allowed);
                    assert!(proof.dependencies.is_empty());
                    assert_eq!(proof.model_source.unwrap().output_envelope_id, output.envelope_id);
                    assert!(!raw.contains(TEXT));
                    txn.rollback().await.unwrap();
                    let original = crate::entity::agent_schedule::Entity::find_by_id(task.id).one(&store.db).await.unwrap().unwrap();
                    assert!(original.prompt_retention_json.is_none());
                    assert_eq!(std::thread::current().name(), Some("ai-task-prompt-small-stack"));
                });
            }).unwrap();
            tokio::task::spawn_blocking(move || worker.join().unwrap())
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn source_navigation_resolves_the_actual_native_result_after_full_owner_session_decode() {
    use desk_diagnose_core::chat::ToolCallRef;
    let (store, mut session, envelope) = fixture("source-result-navigation", true).await;
    let mut source = history::Entity::find()
        .filter(history::Column::NativeId.eq("plain-owner-input"))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    source.native_id = "history-index-only-id".into();
    session.conversation.push(ChatMessage::assistant_tool_calls(
        "original-request",
        "",
        vec![ToolCallRef {
            id: "original-call".into(),
            name: "read_device_status".into(),
            arguments_json: "{}".into(),
        }],
    ));
    let mut message = ChatMessage::tool_result(
        "original-result-message",
        "original-call",
        "original result",
    );
    message.data_envelope = Some(envelope.clone());
    session.conversation.push(message);
    let json = session.encode_json_for_storage().unwrap();
    PersistedAgentSession::decode_json(&json).unwrap();
    agent_session::Entity::update_many()
        .col_expr(agent_session::Column::StateJson, Expr::value(json))
        .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
        .exec(&store.db)
        .await
        .unwrap();
    assert_eq!(
        owner_source::original_result_message_on(&store.db, "1", &source, &envelope.envelope_id)
            .await
            .unwrap()
            .as_deref(),
        Some("original-result-message")
    );
    assert_eq!(
        owner_source::original_result_message_on(&store.db, "2", &source, &envelope.envelope_id)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        owner_source::original_result_message_on(
            &store.db,
            "1",
            &source,
            "missing-original-envelope"
        )
        .await
        .unwrap(),
        None
    );
    let mut wrong_device = source.clone();
    wrong_device.device_ref = "other-source-device".into();
    assert_eq!(
        owner_source::original_result_message_on(
            &store.db,
            "1",
            &wrong_device,
            &envelope.envelope_id
        )
        .await
        .unwrap(),
        None
    );
    agent_session::Entity::update_many()
        .col_expr(agent_session::Column::StateJson, Expr::value("{}"))
        .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
        .exec(&store.db)
        .await
        .unwrap();
    assert_eq!(
        owner_source::original_result_message_on(&store.db, "1", &source, &envelope.envelope_id)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn native_inventory_save_retains_a_verified_empty_snapshot_once_without_execution_authority()
{
    let (store, mut session, _) = fixture("native-inventory-empty", true).await;
    let before = session.conversation.clone();
    host(&store).save(&mut session).await.unwrap();
    let rows = history::Entity::find()
        .filter(history::Column::Kind.eq("native_status"))
        .all(&store.db)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    let metadata = crate::assistant_data_store::history_native::read(row)
        .unwrap()
        .unwrap();
    let summary = metadata
        .status_snapshot
        .as_ref()
        .unwrap()
        .summary(
            &row.source_id(),
            row.revision as u64,
            desk_agent_protocol::ai_assistant::limits::AssistantProcessingLimits::default()
                .native_status_model_items as usize,
        )
        .unwrap();
    assert!(summary.no_native_task);
    assert!(summary.items.is_empty());
    assert!(!summary.has_more);
    assert_eq!(
        row.recorded_at_ms as u64,
        session
            .assistant_data
            .as_ref()
            .unwrap()
            .current_input_recorded_at_unix_ms
    );
    assert_eq!(row.statement_timezone, "UTC");
    assert!(metadata.time.is_none());
    assert_eq!(session.conversation, before);
    let owner = owner_source::history_source_on(
        &store.db,
        &session.actor_id,
        row,
        &AssistantDataPolicy::default(),
        chrono::Utc::now().timestamp_millis() as u64,
    )
    .await
    .unwrap();
    assert!(matches!(
        owner.availability,
        SourceAvailability::Available | SourceAvailability::Partial
    ));
    host(&store).save(&mut session).await.unwrap();
    assert_eq!(
        history::Entity::find()
            .filter(history::Column::Kind.eq("native_status"))
            .all(&store.db)
            .await
            .unwrap(),
        rows
    );
}

#[tokio::test]
async fn native_inventory_save_keeps_a_closed_main_trigger_observable_without_model_tool_access() {
    let (store, mut session, _) = fixture("native-inventory-closed", true).await;
    session.trigger_origin = desk_diagnose_core::session::TriggerOrigin::WorkCompletion {
        kind: desk_diagnose_core::session::WorkKind::CapabilityProvider,
    };
    assert!(!desk_diagnose_core::ai_assistant::data_access::eligible_origin(&session));
    // An original settled-work trigger remains personal history; its origin
    // still grants neither account tools nor a new model-answer producer.
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::StateJson,
            sea_orm::sea_query::Expr::value(session.encode_json_for_storage().unwrap()),
        )
        .filter(agent_session::Column::Id.eq(row.id))
        .filter(agent_session::Column::Version.eq(row.version))
        .exec(&store.db)
        .await
        .unwrap();
    let before = session.conversation.clone();
    host(&store).save(&mut session).await.unwrap();
    assert_eq!(session.conversation, before);
    let rows = history::Entity::find()
        .filter(history::Column::Kind.eq("native_status"))
        .all(&store.db)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!desk_diagnose_core::ai_assistant::data_access::eligible_origin(&session));
}
