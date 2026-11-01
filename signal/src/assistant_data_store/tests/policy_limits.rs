use super::*;

#[tokio::test]
async fn configured_policy_is_shared_cas_protected_and_applies_to_new_personalization() {
    let store = setup().await;
    let original = crate::assistant_data_config::policy(&store.db).await;
    let mut policy = original.clone();
    policy.max_memory_records = 7;
    policy.limits.conversation.question_bytes = 131072;
    policy.limits.conversation.citations = 80;
    policy.limits.conversation.citation_bytes = 131072;
    policy.limits.conversation.citation_page_answers = 150;
    policy.limits.conversation.page_bytes = 1024 * 1024;

    policy.limits.retrieval.snippet_bytes = 2048;
    policy.limits.retrieval.episode.query_devices = 20000;
    policy.limits.retrieval.episode.query_cursors = 64;
    policy.limits.retrieval.episode.query_state_bytes = 2097152;
    policy.limits.retrieval.episode.query_seen_sources = 1000;
    policy.limits.retrieval.episode.query_delivered_calls = 64;
    policy.limits.retrieval.episode.query_ttl_ms = 60_000;
    policy.limits.retrieval.read_messages = 60;
    policy.limits.retrieval.episode.response_bytes = 128 * 1024;
    policy.limits.retrieval.episode.unused_projection_ttl_ms = 3_600_000;
    policy.limits.memory.user_instructions_bytes = 16384;
    policy.limits.memory.current_facts = 12;
    policy.limits.retrieval.query_groups = 6;
    policy.limits.retrieval.history_keyword_scan_ms = 7000;
    policy.limits.retrieval.owner_scan_documents = 256;
    policy.limits.processing =
        desk_agent_protocol::ai_assistant::limits::AssistantProcessingLimits {
            status_coverage: 80,
            native_status_rows: 40,
            native_status_items: 60,
            native_status_model_items: 8,
            native_status_source_refs: 10,

            output_gc_interval_ms: 71,
            output_gc_actor_batch_size: 35,
            output_gc_batch_size: 129,
            native_retention_page_size: 3,
            history_capture_batch_size: 129,
            history_scan_page_size: 7,
            current_memory_page_size: 2,
        };
    let request = desk_agent_protocol::ai_assistant::user_instructions::AssistantDataPolicyUpdate {
        expected_revision: original.revision,
        policy,
    };
    let saved = crate::assistant_data_config::update_policy(&store.db, &request)
        .await
        .unwrap()
        .policy;
    assert_eq!(saved.revision, original.revision + 1);
    assert_eq!(saved.max_memory_records, 7);
    assert_eq!(crate::assistant_data_config::policy(&store.db).await, saved);
    assert!(
        crate::assistant_data_config::update_policy(&store.db, &request)
            .await
            .is_err()
    );
    let instructions =
        desk_agent_protocol::ai_assistant::user_instructions::UserInstructionsUpdate {
            expected_revision: 1,
            content: "中".repeat(3000),
        };
    let remembered = crate::assistant_data_config::update_instructions(&store.db, &instructions)
        .await
        .unwrap();
    let mut lowered = saved.clone();
    lowered.limits.memory.user_instructions_bytes = 1;
    let lowered = crate::assistant_data_config::update_policy(
        &store.db,
        &desk_agent_protocol::ai_assistant::user_instructions::AssistantDataPolicyUpdate {
            expected_revision: saved.revision,
            policy: lowered,
        },
    )
    .await
    .unwrap()
    .policy;
    assert_eq!(
        crate::assistant_data_config::instructions(&store.db).await,
        remembered
    );
    assert!(
        instructions
            .validate_with_limits(&lowered.limits.memory)
            .is_err()
    );
}

#[tokio::test]
async fn invalid_policy_is_not_published_and_warnings_do_not_block_persistence() {
    use desk_agent_protocol::ai_assistant::user_instructions::AssistantDataPolicyUpdate;
    let store = setup().await;
    let original = crate::assistant_data_config::policy(&store.db).await;
    let mut invalid = original.clone();
    invalid.limits.retrieval.episode.response_bytes = 4;
    assert!(
        crate::assistant_data_config::update_policy(
            &store.db,
            &AssistantDataPolicyUpdate {
                expected_revision: original.revision,
                policy: invalid
            }
        )
        .await
        .is_err()
    );
    assert_eq!(
        crate::assistant_data_config::policy(&store.db).await,
        original
    );
    let mut draft = original.clone();
    draft.limits.retrieval.relevance_hits = 100;
    draft.limits.retrieval.episode.read_bytes = 1;
    let request = AssistantDataPolicyUpdate {
        expected_revision: original.revision,
        policy: draft,
    };
    let saved = crate::assistant_data_config::update_policy(&store.db, &request)
        .await
        .unwrap();
    assert_eq!(saved.issues.len(), 1);
    assert_eq!(saved.issues[0].effective_limit, Some(40));
    assert_eq!(saved.policy.revision, original.revision + 1);
    assert_eq!(
        crate::assistant_data_config::policy(&store.db).await,
        saved.policy
    );
    assert!(
        crate::assistant_data_config::update_policy(&store.db, &request)
            .await
            .is_err()
    );
    assert_eq!(
        crate::assistant_data_config::policy(&store.db).await,
        saved.policy
    );
}
