use super::retrieval_native::Frame;
use super::*;
use crate::entity::{
    agent_memory as memory, agent_memory_revision as revision, agent_memory_source as evidence,
};
use desk_agent_protocol::ai_assistant::{
    memory::{MemoryDeleteRequest, MemoryHistoryRequest, MemoryStatus},
    source::SourceReadPurpose,
};
use desk_diagnose_core::{
    ai_assistant::data_tools,
    chat::ToolCall,
    native_ids::{self, Kind},
    seam::SessionSeam,
};
use sea_orm::{PaginatorTrait, sea_query::Expr};

async fn seed_versions(
    store: &TestStore,
    session: &desk_diagnose_core::session::PersistedAgentSession,
    count: u64,
) -> String {
    let memory_id = memory::Entity::find()
        .filter(memory::Column::ActorId.eq(&session.actor_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap()
        .memory_id;
    let head = memory::Entity::find()
        .filter(memory::Column::MemoryId.eq(&memory_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let original = revision::Entity::find()
        .filter(revision::Column::MemoryId.eq(&memory_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let source = evidence::Entity::find()
        .filter(evidence::Column::MemoryId.eq(&memory_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    for index in 1..=count {
        let name = if index == 1 {
            "sss".into()
        } else if index == count {
            "yyy".into()
        } else {
            format!("name-{index}")
        };
        let body = format!("用户要求称呼为 {name}");
        let context = Some(format!("用户第 {index} 次提出的偏好称呼"));
        let hash = super::super::memory::content_hash(&body, context.as_deref());
        let mut version: revision::ActiveModel = original.clone().into();
        if index > 1 {
            version.id = sea_orm::ActiveValue::NotSet;
        }
        version.revision = Set(index as i64);
        version.body = Set(body.clone());
        version.context = Set(context.clone());
        version.content_sha256 = Set(hash.clone());
        version.status = Set(if index == count {
            "active"
        } else {
            "superseded"
        }
        .into());
        version.created_at_ms = Set(now - ((count - index + 1) * 1000) as i64);
        version.expires_at_ms = Set((index < count).then_some(now + 86_400_000));
        if index == 1 {
            version.update(&store.db).await.unwrap();
        } else {
            version.insert(&store.db).await.unwrap();
        }
        let source_id = if index == 1 {
            source.source_id.clone()
        } else {
            native_ids::stable(Kind::AssistantSource, &(&memory_id, index))
        };
        let mut retained: evidence::ActiveModel = source.clone().into();
        if index > 1 {
            retained.id = sea_orm::ActiveValue::NotSet;
        }
        retained.source_id = Set(source_id.clone());
        retained.memory_revision = Set(index as i64);
        if index == 1 {
            retained.update(&store.db).await.unwrap();
        } else {
            retained.insert(&store.db).await.unwrap();
        }
        super::owner_memory::rebuild_evidence_proof(store, &source_id).await;
        if index == count {
            let mut current: memory::ActiveModel = head.clone().into();
            current.revision = Set(index as i64);
            current.body = Set(body);
            current.context = Set(context);
            current.content_sha256 = Set(hash);
            current.updated_at_ms = Set(now);
            current.update(&store.db).await.unwrap();
        }
    }
    memory_id
}

async fn frame(label: &str, count: u64, limit: u32) -> Frame {
    // The accepted model input contains owner text only; revision fixtures are
    // installed before its tool output without changing source generations.
    Frame::with_call_scope(
        label,
        true,
        None,
        true,
        false,
        None,
        async |store, session| {
            let memory_id = seed_versions(store, session, count).await;
            vec![ToolCall {
                id: "history-read".into(),
                name: data_tools::READ_MEMORY_HISTORY.into(),
                arguments_json: serde_json::json!({"memory_id":memory_id,"limit":limit})
                    .to_string(),
            }]
        },
    )
    .await
}
fn request(frame: &Frame) -> MemoryHistoryRequest {
    match frame.context().arguments {
        data_tools::ParsedDataCall::ReadMemoryHistory(request) => request.clone(),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn history_reads_exact_old_bodies_and_background_with_native_proof_and_idempotent_replay() {
    let frame = frame("memory-history-originals", 2, 10).await;
    let host = crate::agent_session_store::SignalAgentSessionStore::new(frame.store.db.clone())
        .with_client_metadata(
            None,
            desk_diagnose_core::session::AgentSessionSurface::AiAssistant,
        );
    let before = memory::Entity::find().all(&frame.store.db).await.unwrap();
    let result = host
        .memory()
        .unwrap()
        .read_memory_history(frame.context(), &request(&frame))
        .await
        .unwrap();
    assert_eq!(
        result
            .result
            .versions
            .iter()
            .map(|v| (v.revision, v.status))
            .collect::<Vec<_>>(),
        [(2, MemoryStatus::Active), (1, MemoryStatus::Superseded)]
    );
    assert!(result.result.versions[0].content.contains("yyy"));
    assert!(result.result.versions[1].content.contains("sss"));
    assert_eq!(
        result.result.versions[1].context.as_deref(),
        Some("用户第 1 次提出的偏好称呼")
    );
    assert!(
        result.result.versions[0].recorded_at_unix_ms
            > result.result.versions[1].recorded_at_unix_ms
    );
    assert!(!result.result.has_more && !result.result.partial);
    let proof = result.projection.as_ref().unwrap();
    assert_eq!(proof.usage.purpose, SourceReadPurpose::ModelMemory);
    assert_eq!(proof.usage.memories.len(), 2);
    assert!(proof.usage.memories.iter().all(|v| v.purpose
        == desk_agent_protocol::ai_assistant::memory::MemoryUsePurpose::HistoricalReference));
    proof
        .validate_cached_result(&serde_json::to_string(&result.result).unwrap())
        .unwrap();
    let charged = quota::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    let replay = host
        .memory()
        .unwrap()
        .read_memory_history(frame.context(), &request(&frame))
        .await
        .unwrap();
    assert_eq!(result.result, replay.result);
    assert_eq!(result.projection, replay.projection);
    assert_eq!(
        quota::Entity::find()
            .one(&frame.store.db)
            .await
            .unwrap()
            .unwrap(),
        charged
    );
    assert_eq!(
        memory::Entity::find().all(&frame.store.db).await.unwrap(),
        before
    );
    let current = host
        .memory()
        .unwrap()
        .current_memories(
            &frame.session,
            frame.context().destination,
            "after-history",
            32768,
            frame.now,
        )
        .await
        .unwrap();
    assert_eq!(current.records.len(), 1);
    assert!(current.records[0].content.contains("yyy"));
    assert_eq!(current.records[0].revision, 2);
}

#[tokio::test]
async fn another_host_resumes_the_persisted_history_cursor_and_reads_its_old_evidence_after_real_model_acceptance()
 {
    let frame = frame("memory-history-pages", 3, 1).await;
    let first =
        super::super::memory_history::read(&frame.store.db, frame.context(), &request(&frame))
            .await
            .unwrap();
    assert_eq!(first.result.versions[0].revision, 3);
    assert!(first.result.has_more);
    let frame = frame.after_search(first, None, |_, page| vec![ToolCall {
        id: "history-next".into(), name: data_tools::READ_MEMORY_HISTORY.into(),
        arguments_json: serde_json::json!({"memory_id":page.memory_id,"limit":1,"cursor":page.next_cursor}).to_string(),
    }]).await;
    let head = memory::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    let mut later: revision::ActiveModel = revision::Entity::find()
        .filter(revision::Column::MemoryId.eq(&head.memory_id))
        .filter(revision::Column::Revision.eq(3_i64))
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap()
        .into();
    revision::Entity::update_many()
        .filter(revision::Column::MemoryId.eq(&head.memory_id))
        .filter(revision::Column::Revision.eq(3_i64))
        .col_expr(revision::Column::Status, Expr::value("superseded"))
        .col_expr(
            revision::Column::ExpiresAtMs,
            Expr::value(frame.now as i64 + 86_400_000),
        )
        .exec(&frame.store.db)
        .await
        .unwrap();
    let body = "A newer preference after the query";
    let hash = super::super::memory::content_hash(body, head.context.as_deref());
    later.id = sea_orm::ActiveValue::NotSet;
    later.revision = Set(4);
    later.body = Set(body.into());
    later.content_sha256 = Set(hash.clone());
    later.insert(&frame.store.db).await.unwrap();
    memory::Entity::update_many()
        .filter(memory::Column::Id.eq(head.id))
        .col_expr(memory::Column::Revision, Expr::value(4_i64))
        .col_expr(memory::Column::Body, Expr::value(body))
        .col_expr(memory::Column::ContentSha256, Expr::value(hash))
        .exec(&frame.store.db)
        .await
        .unwrap();
    let other_host =
        crate::agent_session_store::SignalAgentSessionStore::new(frame.store.db.clone())
            .with_client_metadata(
                None,
                desk_diagnose_core::session::AgentSessionSurface::AiAssistant,
            );
    let second = other_host
        .memory()
        .unwrap()
        .read_memory_history(frame.context(), &request(&frame))
        .await
        .unwrap();
    assert_eq!(second.result.current_revision, 3);
    assert_eq!(second.result.versions[0].revision, 2);
    let frame = frame
        .after_search(second, None, |_, page| {
            vec![ToolCall {
                id: "history-evidence".into(),
                name: data_tools::READ_MEMORY_SOURCE.into(),
                arguments_json: serde_json::json!({"source_id":page.versions[0].source_ids[0]})
                    .to_string(),
            }]
        })
        .await;
    let data_tools::ParsedDataCall::ReadMemorySource { source_id, .. } = frame.context().arguments
    else {
        unreachable!()
    };
    let result = super::super::memory_source::read(&frame.store.db, frame.context(), source_id)
        .await
        .unwrap();
    assert_eq!(result.result.revision, 2);
    assert!(result.result.content.is_some());
}

#[tokio::test]
async fn expired_or_corrupt_versions_are_reported_as_gaps_and_never_recovered_from_identity_only_proofs()
 {
    for corrupt in [false, true] {
        let frame = frame(&format!("memory-history-gap-{corrupt}"), 2, 10).await;
        let query = revision::Entity::update_many().filter(revision::Column::Revision.eq(1));
        if corrupt {
            query
                .col_expr(revision::Column::Body, Expr::value("wrong old body"))
                .exec(&frame.store.db)
                .await
                .unwrap();
        } else {
            query
                .col_expr(revision::Column::ExpiresAtMs, Expr::value(1_i64))
                .exec(&frame.store.db)
                .await
                .unwrap();
        }
        let result =
            super::super::memory_history::read(&frame.store.db, frame.context(), &request(&frame))
                .await
                .unwrap();
        assert_eq!(result.result.versions.len(), 1);
        assert_eq!(result.result.versions[0].revision, 2);
        assert!(result.result.partial);
        assert!(!result.result.has_more);
        assert!(
            result
                .result
                .reasons
                .iter()
                .any(|r| r == "history_versions_unavailable")
        );
    }
}

#[tokio::test]
async fn forgotten_memory_and_other_accounts_are_inaccessible_without_new_result_snapshots() {
    for foreign in [false, true] {
        let frame = frame(&format!("memory-history-denied-{foreign}"), 2, 10).await;
        let request = request(&frame);
        if foreign {
            memory::Entity::update_many()
                .col_expr(memory::Column::ActorId, Expr::value("2"))
                .filter(memory::Column::MemoryId.eq(&request.memory_id))
                .exec(&frame.store.db)
                .await
                .unwrap();
        } else {
            frame
                .store
                .forget_memory(
                    1,
                    &MemoryDeleteRequest {
                        memory_id: request.memory_id.clone(),
                        expected_revision: 2,
                        operation_id: "forget-history".into(),
                    },
                    &AssistantDataPolicy::default(),
                    frame.now,
                )
                .await
                .unwrap();
        }
        let before = crate::entity::agent_assistant_read_projection::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap();
        assert!(
            super::super::memory_history::read(&frame.store.db, frame.context(), &request)
                .await
                .is_err()
        );
        assert_eq!(
            crate::entity::agent_assistant_read_projection::Entity::find()
                .count(&frame.store.db)
                .await
                .unwrap(),
            before
        );
    }
}

#[tokio::test]
async fn complete_history_versions_that_exceed_the_response_budget_fail_without_a_nonadvancing_cursor()
 {
    let frame = Frame::with_call_scope(
        "memory-history-small-budget",
        true,
        Some(128),
        true,
        false,
        None,
        async |store, session| {
            let memory_id = seed_versions(store, session, 2).await;
            vec![ToolCall {
                id: "bounded-history".into(),
                name: data_tools::READ_MEMORY_HISTORY.into(),
                arguments_json: serde_json::json!({"memory_id":memory_id}).to_string(),
            }]
        },
    )
    .await;
    let before = quota::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    let error =
        super::super::memory_history::read(&frame.store.db, frame.context(), &request(&frame))
            .await
            .err()
            .unwrap();
    let error = error.into_agent_error();
    assert!(error.safe_for_model);
    assert_eq!(
        quota::Entity::find()
            .one(&frame.store.db)
            .await
            .unwrap()
            .unwrap(),
        before
    );
    assert_eq!(
        desk_assistant_data_store::entity::agent_assistant_retrieval_query::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap(),
        0
    );
}

#[test]
fn memory_history_read_and_replay_fit_one_and_two_mib_stacks() {
    const STACK: &str = "LRD_TEST_MEMORY_HISTORY_STACK_BYTES";
    if let Some(bytes) = std::env::var_os(STACK) {
        let bytes: usize = bytes.to_str().unwrap().parse().unwrap();
        assert!([1024 * 1024, 2 * 1024 * 1024].contains(&bytes));
        let fixture_runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let frame = fixture_runtime.block_on(frame("memory-history-stack", 3, 3));
        let polling_runtime = fixture_runtime.handle().clone();
        std::thread::Builder::new()
            .name("memory-history-stack".into())
            .stack_size(bytes)
            .spawn(move || {
                polling_runtime.block_on(async move {
                    let first = super::super::memory_history::read(
                        &frame.store.db,
                        frame.context(),
                        &request(&frame),
                    )
                    .await
                    .unwrap();
                    assert_eq!(first.result.versions.len(), 3);
                    let replay = super::super::memory_history::read(
                        &frame.store.db,
                        frame.context(),
                        &request(&frame),
                    )
                    .await
                    .unwrap();
                    assert_eq!(first.result, replay.result);
                    assert_eq!(std::thread::current().name(), Some("memory-history-stack"));
                });
            })
            .unwrap()
            .join()
            .unwrap();
        return;
    }
    for bytes in [1024 * 1024, 2 * 1024 * 1024] {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "memory_history_read_and_replay_fit_one_and_two_mib_stacks",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(STACK, bytes.to_string())
            .env_remove("RUST_MIN_STACK")
            .status()
            .unwrap();
        assert!(status.success(), "memory history failed on {bytes} bytes");
    }
}
