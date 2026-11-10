use super::*;
use crate::entity::{
    agent_assistant_history as history, agent_memory as memories, agent_memory_revision as versions,
};
use desk_agent_protocol::ai_assistant::memory::{MemoryWriteOutcome, MemoryWriteRequest};
use desk_diagnose_core::ai_assistant::data_tools::ParsedDataCall;
use sea_orm::PaginatorTrait;
async fn write_frame(label: &str, accepted: bool, update: bool, unseen: bool) -> Frame {
    write_frame_with_memory(label, accepted, update, unseen, true).await
}
async fn write_frame_with_memory(
    label: &str,
    accepted: bool,
    update: bool,
    unseen: bool,
    include_memory: bool,
) -> Frame {
    Frame::with_call_scope(label, accepted, None, true, include_memory, None, async |store, session| {
        let source = history::Entity::find().filter(history::Column::NativeId.eq("interactive-owner-input"))
            .one(&store.db).await.unwrap().unwrap();
        let mut arguments = serde_json::json!({"operation": if update {"update"} else {"remember"},
            "content":"用户偏好中文且简明的回复", "context":"用于日常回答的风格偏好",
            "source_refs":[{"kind":"input","id":source.source_id(),"revision":source.revision,"role":"evidence"}]});
        if update {
            let seen = &session.assistant_data.as_ref().unwrap().current_memories[0];
            arguments["memory_id"] = seen.memory_id.clone().into();
            arguments["expected_revision"] = (seen.revision + u64::from(unseen)).into();
        }
        vec![ToolCall { id: "interactive-write-call".into(), name: data_tools::MANAGE_MEMORIES.into(), arguments_json: arguments.to_string() }]
    }).await
}
fn request(frame: &Frame) -> MemoryWriteRequest {
    let ParsedDataCall::ManageMemory(value) = data_tools::parse(&frame.calls[0]).unwrap() else {
        unreachable!()
    };
    value
}

#[tokio::test]
async fn native_memory_writes_commit_exact_content_and_replay_without_another_model_call() {
    for update in [false, true] {
        let frame = write_frame(&format!("direct-{update}"), true, update, false).await;
        let native_before = native::Entity::find().all(&frame.store.db).await.unwrap();
        let original = request(&frame);
        let before = versions::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap();
        let result = crate::assistant_data_store::memory_write::write(
            &frame.store.db,
            frame.context(),
            &original,
        )
        .await
        .unwrap();
        assert_eq!(
            result.result.outcome,
            MemoryWriteOutcome::Applied,
            "{:?}",
            result.result
        );
        let committed = result.result.memory.as_ref().unwrap();
        let row = memories::Entity::find()
            .filter(memories::Column::MemoryId.eq(&committed.memory_id))
            .one(&frame.store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.body, original.draft().unwrap().content);
        assert_eq!(row.context, original.draft().unwrap().context);
        assert_eq!(
            versions::Entity::find()
                .count(&frame.store.db)
                .await
                .unwrap(),
            before + 1
        );
        let reference = desk_diagnose_core::ai_assistant::data_control::CurrentMemoryUse {
            memory_id: committed.memory_id.clone(),
            revision: committed.revision,
            purpose: desk_agent_protocol::ai_assistant::memory::MemoryUsePurpose::CurrentFact,
        };
        let data = frame.session.assistant_data.as_ref().unwrap();
        let (sources, _, _) = super::super::super::model_sources::memory_sources_on(
            &frame.store.db,
            &frame.session.actor_id,
            &data.source_device_ref,
            &reference,
            frame.now,
        )
        .await
        .unwrap();
        assert!(!sources.is_empty());
        for source in sources {
            let policy: desk_agent_protocol::ai_assistant::source::SourceUsePolicy =
                serde_json::from_str(&source.policy_json).unwrap();
            assert!(
                !policy.independent_fact_allowed,
                "saved model text is derived, not an original assertion"
            );
            super::super::super::model_sources::evidence_on(
                &frame.store.db,
                &frame.session.actor_id,
                &source,
                false,
                &frame.destination,
                frame.now,
            )
            .await
            .expect("a committed derived snapshot must remain model-readable");
        }
        let quota_before_replay = crate::entity::agent_assistant_quota::Entity::find()
            .all(&frame.store.db)
            .await
            .unwrap();
        let replay = crate::assistant_data_store::memory_write::write(
            &frame.store.db,
            frame.context(),
            &original,
        )
        .await
        .unwrap();
        assert_eq!(replay.result, result.result);
        assert_eq!(
            versions::Entity::find()
                .count(&frame.store.db)
                .await
                .unwrap(),
            before + 1
        );
        assert_eq!(
            crate::entity::agent_assistant_quota::Entity::find()
                .all(&frame.store.db)
                .await
                .unwrap(),
            quota_before_replay
        );
        assert_eq!(
            native::Entity::find().all(&frame.store.db).await.unwrap(),
            native_before
        );
    }
}
#[tokio::test]
async fn stale_update_is_a_tool_result_and_never_changes_the_memory() {
    let frame = write_frame("direct-unobserved", true, true, true).await;
    let before = memories::Entity::find().all(&frame.store.db).await.unwrap();
    let result = crate::assistant_data_store::memory_write::write(
        &frame.store.db,
        frame.context(),
        &request(&frame),
    )
    .await
    .unwrap();
    assert_eq!(result.result.outcome, MemoryWriteOutcome::Conflict);
    assert_eq!(result.result.reason.as_deref(), Some("stale_target"));
    assert_eq!(
        memories::Entity::find().all(&frame.store.db).await.unwrap(),
        before
    );
}

#[tokio::test]
async fn quota_failure_rolls_back_head_version_evidence_index_and_commit() {
    use crate::entity::{
        agent_assistant_quota as quota, agent_assistant_search_document as documents,
        agent_memory_commit as commits, agent_memory_source as sources,
    };
    let frame = write_frame("direct-quota-rollback", true, false, false).await;
    let before = memories::Entity::find().all(&frame.store.db).await.unwrap();
    let versions = versions::Entity::find().all(&frame.store.db).await.unwrap();
    let sources = sources::Entity::find().all(&frame.store.db).await.unwrap();
    let documents = documents::Entity::find()
        .all(&frame.store.db)
        .await
        .unwrap();
    let commits = commits::Entity::find().all(&frame.store.db).await.unwrap();
    quota::Entity::update_many()
        .col_expr(
            quota::Column::MemoryBodyBytes,
            sea_orm::sea_query::Expr::value(i64::MAX),
        )
        .exec(&frame.store.db)
        .await
        .unwrap();
    let quota = quota::Entity::find().all(&frame.store.db).await.unwrap();
    assert!(
        crate::assistant_data_store::memory_write::write(
            &frame.store.db,
            frame.context(),
            &request(&frame)
        )
        .await
        .is_err()
    );
    assert_eq!(
        memories::Entity::find().all(&frame.store.db).await.unwrap(),
        before
    );
    assert_eq!(
        versions::Entity::find().all(&frame.store.db).await.unwrap(),
        versions
    );
    assert_eq!(
        sources::Entity::find().all(&frame.store.db).await.unwrap(),
        sources
    );
    assert_eq!(
        documents::Entity::find()
            .all(&frame.store.db)
            .await
            .unwrap(),
        documents
    );
    assert_eq!(
        commits::Entity::find().all(&frame.store.db).await.unwrap(),
        commits
    );
    assert_eq!(
        quota::Entity::find().all(&frame.store.db).await.unwrap(),
        quota
    );
}

#[test]
fn synchronous_memory_writes_fit_one_and_two_mib_stacks() {
    for bytes in [1024 * 1024, 2 * 1024 * 1024] {
        actix_web::rt::System::new().block_on(async {
            let frame = write_frame("direct-memory-stack", true, true, false).await;
            let (done, ready) = tokio::sync::oneshot::channel();
            let thread = std::thread::Builder::new()
                .stack_size(bytes)
                .spawn(move || {
                    actix_web::rt::System::new().block_on(async move {
                        let request = request(&frame);
                        let future = crate::assistant_data_store::memory_write::write(
                            &frame.store.db,
                            frame.context(),
                            &request,
                        );
                        assert!(std::mem::size_of_val(&future) <= 16 * 1024);
                        let result = Box::pin(future).await.unwrap();
                        assert_eq!(result.result.outcome, MemoryWriteOutcome::Applied);
                        let replay = crate::assistant_data_store::memory_write::write(
                            &frame.store.db,
                            frame.context(),
                            &request,
                        )
                        .await
                        .unwrap();
                        assert_eq!(replay.result, result.result);
                        done.send(()).unwrap();
                    });
                })
                .unwrap();
            ready.await.unwrap();
            thread.join().unwrap();
        });
    }
}

#[tokio::test]
async fn unobserved_existing_update_target_cannot_be_overwritten() {
    let frame = Frame::with_call_scope("direct-unobserved-existing", true, None, true, false, None, async |store, _| {
        let target = memories::Entity::find().one(&store.db).await.unwrap().unwrap();
        vec![ToolCall { id:"unobserved-write".into(), name:data_tools::MANAGE_MEMORIES.into(),
            arguments_json:serde_json::json!({"operation":"update","memory_id":target.memory_id,"expected_revision":target.revision,
                "content":"Do not replace an unseen memory"}).to_string() }]
    }).await;
    let before = memories::Entity::find().all(&frame.store.db).await.unwrap();
    let result = crate::assistant_data_store::memory_write::write(
        &frame.store.db,
        frame.context(),
        &request(&frame),
    )
    .await
    .unwrap();
    assert_eq!(result.result.outcome, MemoryWriteOutcome::Rejected);
    assert_eq!(result.result.reason.as_deref(), Some("target_not_observed"));
    assert_eq!(
        memories::Entity::find().all(&frame.store.db).await.unwrap(),
        before
    );
}

#[tokio::test]
async fn commit_gc_waits_for_the_original_output_and_refunds_exact_metadata_once() {
    use crate::entity::{agent_assistant_model_output as outputs, agent_memory_commit as commits};
    let frame = write_frame("direct-receipt-gc", true, false, false).await;
    crate::assistant_data_store::memory_write::write(
        &frame.store.db,
        frame.context(),
        &request(&frame),
    )
    .await
    .unwrap();
    let row = commits::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    let expected = desk_assistant_data_store::memory_write::receipts::metadata_bytes(&row).unwrap();
    let now = row.expires_at_ms + 1;
    let collect = desk_assistant_data_store::memory_write::receipts::collect_on;
    assert_eq!(
        collect(&frame.store.db, &row.actor_id, now, 10)
            .await
            .unwrap(),
        0
    );
    outputs::Entity::delete_many()
        .filter(outputs::Column::PhysicalCallId.eq(&row.source_call_id))
        .exec(&frame.store.db)
        .await
        .unwrap();
    assert_eq!(
        collect(&frame.store.db, &row.actor_id, now, 10)
            .await
            .unwrap(),
        expected
    );
    assert_eq!(
        collect(&frame.store.db, &row.actor_id, now, 10)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn owner_evidence_is_saved_when_other_context_cannot_be_retained() {
    // A control result without a registered leaf leaves the output with no whole-context policy.
    use crate::entity::agent_assistant_model_output as archived;
    let frame = write_frame("direct-owner-evidence", true, false, false).await;
    for row in archived::Entity::find().all(&frame.store.db).await.unwrap() {
        let id = row.id;
        let mut proof =
            *crate::assistant_data_store::output_archive::decode_row_on(&frame.store.db, row)
                .await
                .unwrap()
                .proof;
        proof.retained_policy = None;
        proof
            .input_policies
            .leaves
            .iter_mut()
            .for_each(|leaf| *leaf = None);
        proof.input_policies.policies.clear();
        crate::assistant_data_store::output_archive::rewrite_for_test(&frame.store.db, id, &proof)
            .await
            .unwrap();
    }
    let result = crate::assistant_data_store::memory_write::write(
        &frame.store.db,
        frame.context(),
        &request(&frame),
    )
    .await
    .unwrap();
    assert_eq!(
        result.result.outcome,
        MemoryWriteOutcome::Applied,
        "{:?}",
        result.result
    );
    assert_eq!(result.result.reason.as_deref(), Some("committed"));
}

#[tokio::test]
async fn owner_input_references_resolve_to_the_captured_history_row() {
    let frame = Frame::with_call_scope(
        "input-reference",
        true,
        None,
        true,
        false,
        None,
        async |_store, _session| {
            let arguments = serde_json::json!({"operation":"remember",
                "content":"用户偏好中文且简明的回复", "context":"用于日常回答的风格偏好",
                "source_refs":[{"kind":"input","id":"input:0","role":"evidence"}]});
            vec![ToolCall {
                id: "interactive-write-call".into(),
                name: data_tools::MANAGE_MEMORIES.into(),
                arguments_json: arguments.to_string(),
            }]
        },
    )
    .await;
    let source = history::Entity::find()
        .filter(history::Column::NativeId.eq("interactive-owner-input"))
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    let result = crate::assistant_data_store::memory_write::write(
        &frame.store.db,
        frame.context(),
        &request(&frame),
    )
    .await
    .unwrap();
    assert_eq!(
        result.result.outcome,
        MemoryWriteOutcome::Applied,
        "{:?}",
        result.result
    );
    let memory_id = &result.result.memory.as_ref().unwrap().memory_id;
    let evidence = crate::entity::agent_memory_source::Entity::find()
        .filter(crate::entity::agent_memory_source::Column::MemoryId.eq(memory_id))
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    // `input:0` names the newest owner input before the producing call.
    assert_eq!(evidence.origin_id, source.origin_id);
    assert!(evidence.known_sources_json.contains(&source.source_id()));
}
