use super::*;
use crate::entity::agent_subagent_inbox as inbox;

#[tokio::test]
async fn ui_read_is_subject_scoped_monotonic_and_independent_of_model_consumption() {
    let db = database().await;
    seed_parent(&db).await;
    let mut task = seed_task(&db, "ui-attention", "goal-ui").await;
    let group = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq("ui-attention"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let group = decode_group(&group).unwrap();
    let row = run_row::Entity::find()
        .filter(run_row::Column::TaskId.eq(&task.binding.task_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    task.fail("unable-to-complete", "1970-01-01T00:00:01Z")
        .unwrap();
    replace_run_on(&db, &row, &task, 1000).await.unwrap();
    synchronize_control_on(&db, &task, 1000).await.unwrap();
    append_state_event_on(&db, &group, &task, 1000)
        .await
        .unwrap();
    let parent = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let mut parent = PersistedAgentSession::decode_json(&parent.state_json).unwrap();
    parent.client_conversation_id = Some("original-parent-intent".into());
    parent.version += 1;
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(parent.encode_json_for_storage().unwrap()),
            version: Set(parent.version),
            ..Default::default()
        })
        .filter(session_row::Column::ConversationId.eq("root"))
        .exec(&db)
        .await
        .unwrap();
    let projection = presentation_on(&db, &parent).await.unwrap();
    assert_eq!(projection.attention_count, 1);
    assert_eq!(projection.attention_tasks[0].task_id, task.binding.task_id);
    let approval = desk_signal_facade::controller::ai_assistant_session::AiAssistantAttentionItemDto::from_command_approval(
        &task.child_conversation_id, "1", "original-child-approval", 1000, Some(5000));
    let mut global = vec![approval];
    augment_owner_attention_on(&db, "1", None, &mut global)
        .await
        .unwrap();
    assert_eq!(global.len(), 2);
    assert!(global.iter().all(|item| item.session_id == "root"
        && item.client_conversation_id.as_deref() == Some("original-parent-intent")));
    let result_identity = global.iter().find(|item| item.reason == desk_signal_facade::controller::ai_assistant_session::AiAssistantAttentionReason::SubAgentResult).unwrap().attention_id.clone();
    let mut replay = Vec::new();
    augment_owner_attention_on(&db, "1", None, &mut replay)
        .await
        .unwrap();
    assert_eq!(replay[0].attention_id, result_identity);
    let mut inaccessible = Vec::new();
    augment_owner_attention_on(&db, "other-owner", None, &mut inaccessible)
        .await
        .unwrap();
    assert!(inaccessible.is_empty());
    augment_owner_attention_on(
        &db,
        "1",
        Some(&std::collections::HashSet::from(["other-device".into()])),
        &mut inaccessible,
    )
    .await
    .unwrap();
    assert!(inaccessible.is_empty());
    let store = SubAgentStore::new(db.clone());
    assert!(
        store
            .mark_ui_read_for_owner(
                "root",
                "other-owner",
                "1",
                &task.binding.task_id,
                task.state_revision
            )
            .await
            .is_err()
    );
    assert!(
        store
            .mark_ui_read_for_owner(
                "root",
                "1",
                "other-device",
                &task.binding.task_id,
                task.state_revision
            )
            .await
            .is_err()
    );
    assert!(
        store
            .mark_ui_read_for_owner(
                "root",
                "1",
                "1",
                &task.binding.task_id,
                task.state_revision + 1
            )
            .await
            .is_err()
    );
    assert!(
        inbox::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .ui_read_at_ms
            .is_none()
    );
    assert!(
        store
            .mark_ui_read_for_owner("root", "1", "1", &task.binding.task_id, task.state_revision)
            .await
            .unwrap()
    );
    let first = inbox::Entity::find().one(&db).await.unwrap().unwrap();
    assert!(first.ui_read_at_ms.is_some());
    assert!(first.model_observed_at_ms.is_none());
    assert!(first.interpreted_at_ms.is_none());
    assert!(
        !store
            .mark_ui_read_for_owner("root", "1", "1", &task.binding.task_id, task.state_revision)
            .await
            .unwrap()
    );
    assert_eq!(
        inbox::Entity::find().one(&db).await.unwrap().unwrap(),
        first
    );
    assert_eq!(
        presentation_on(&db, &parent).await.unwrap().attention_count,
        0
    );
    let mut global_after_read = Vec::new();
    augment_owner_attention_on(&db, "1", None, &mut global_after_read)
        .await
        .unwrap();
    assert!(global_after_read.is_empty());
    assert_eq!(load(&db, &task.binding.task_id).await, task);
    assert_eq!(
        session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq("root"))
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .version,
        parent.version
    );
}

#[tokio::test]
async fn reading_an_old_revision_cannot_hide_a_new_owner_approval_notification() {
    let db = database().await;
    seed_parent(&db).await;
    let mut task = seed_task(&db, "ui-progress", "goal-ui-progress").await;
    let group = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq("ui-progress"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let group = decode_group(&group).unwrap();
    append_state_event_on(&db, &group, &task, 1).await.unwrap();
    let displayed = task.state_revision;
    let row = run_row::Entity::find()
        .filter(run_row::Column::TaskId.eq(&task.binding.task_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    task.synchronize_dependencies(
        vec![
            desk_diagnose_core::subagent::state::TaskDependency::DirectoryApproval {
                directory_request_id: "new-directory-request".into(),
            },
        ],
        false,
        "1970-01-01T00:00:01Z",
    )
    .unwrap();
    replace_run_on(&db, &row, &task, 1000).await.unwrap();
    append_state_event_on(&db, &group, &task, 1000)
        .await
        .unwrap();
    let store = SubAgentStore::new(db.clone());
    store
        .mark_ui_read_for_owner("root", "1", "1", &task.binding.task_id, displayed)
        .await
        .unwrap();
    let newer = inbox::Entity::find()
        .filter(inbox::Column::StateRevision.eq(task.state_revision as i64))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(newer.ui_read_at_ms.is_none());
    assert!(newer.model_observed_at_ms.is_none());
}
