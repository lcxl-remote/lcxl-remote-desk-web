use desk_diagnose_core::model_observability::{
    InputConclusion,InputIssue,ObservationAlias,ObservationEvent,ObservationPayload,
    ObservationRelation,PermissionOutcome,Stage,aggregate::{contribution,Count},
};

async fn seed_observed_schedule_waits(
    db: &sea_orm::DatabaseConnection,
    parent: &PersistedAgentSession,
    child_conversation: &str,
) -> Vec<ObservationAlias> {
    let mut aliases=Vec::new();
    for conversation in [parent.conversation_id.as_str(),child_conversation] {
        let row=agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(conversation))
            .one(db).await.unwrap().unwrap();
        let mut session=PersistedAgentSession::decode_json(&row.state_json).unwrap();
        let request=desk_diagnose_core::permission_tools::build_permission_request(
            &desk_diagnose_core::chat::ToolCall { id:"private-provider-call".into(),name:"request_permissions".into(),
                arguments_json:r#"{"items":[{"item_id":"capture","tool_name":"read_current_screen","exact_input":{"display":"private-display"},"suggested_ttl_seconds":300,"suggested_max_uses":1,"reason":"private-owner-reason"}]}"#.into() },
            &desk_diagnose_core::ai_assistant::ai_assistant_provider_registry(),
            "server-scheduled-permission".into(),session.input_revision,chrono::Utc::now().to_rfc3339(),
        ).unwrap();
        aliases.push(ObservationAlias::permission_request(conversation,&request.request_id).unwrap());
        session.permission_requests.push(request);
        agent_session::Entity::update_many().set(agent_session::ActiveModel {
            state_json:Set(session.encode_json_for_storage().unwrap()),..Default::default()
        }).filter(agent_session::Column::Id.eq(row.id)).exec(db).await.unwrap();
    }
    aliases
}

fn assert_observed_schedule_waits_ended(events: &[ObservationEvent],aliases:&[ObservationAlias]) {
    assert_eq!(events.len(),aliases.len());
    for alias in aliases {
        let matching=events.iter().filter(|event|event.relation==Some(ObservationRelation::ResolveTool(alias.clone()))).collect::<Vec<_>>();
        assert_eq!(matching.len(),1);
        let event=matching[0];
        assert!(event.is_bounded());
        let ObservationPayload::Tool(tool)=&event.payload else { panic!("original input permission fact required") };
        assert_eq!(tool.permission,PermissionOutcome::Cancelled);
        assert_eq!(tool.conclusion,InputConclusion::Unknown);
        assert_eq!(tool.issue,InputIssue::None);
        assert!(!tool.stages.contains_key(&Stage::Dispatch));
        let counts=contribution(&event.payload,false);
        assert_eq!(counts.get(Count::InputRejected),0);
        assert_eq!(counts.get(Count::PermissionDenied),0);
        assert_eq!(counts.get(Count::OperationsDispatched),0);
    }
    let encoded=serde_json::to_string(events).unwrap();
    for secret in ["private-provider-call","private-display","private-owner-reason"] { assert!(!encoded.contains(secret)); }
}

#[tokio::test]
async fn healthy_scheduled_child_wait_does_not_end_permission_or_create_execution_facts() {
    let (db,schedule,_,parent,task_id)=children::scheduled_test_wait_fixture().await;
    let child=child_row::Entity::find().filter(child_row::Column::TaskId.eq(task_id)).one(&db).await.unwrap().unwrap();
    seed_observed_schedule_waits(&db,&parent,&child.child_conversation_id).await;
    let before=original_run(&db,&parent.conversation_id).await;
    let mut events=Vec::new();
    assert!(!schedule.settle_fresh_children_wait_with_observer(&parent.conversation_id,|event|events.push(event)).await.unwrap());
    assert!(events.is_empty());
    assert_eq!(original_run(&db,&parent.conversation_id).await,before);
    let retained=child_row::Entity::find_by_id(child.id).one(&db).await.unwrap().unwrap();
    assert_eq!(retained,child);
}

#[tokio::test]
async fn committed_scheduled_cancellation_ends_waits_and_observer_panic_does_not_change_settlement() {
    for observer_fails in [false,true] {
        let (db,schedule,_,parent,task_id)=children::scheduled_test_wait_fixture().await;
        let child=child_row::Entity::find().filter(child_row::Column::TaskId.eq(task_id)).one(&db).await.unwrap().unwrap();
        let aliases=seed_observed_schedule_waits(&db,&parent,&child.child_conversation_id).await;
        let before=original_run(&db,&parent.conversation_id).await;
        run::Entity::update_many().set(run::ActiveModel {
            cancel_requested_at:Set(Some(chrono::Utc::now().timestamp_millis())),..Default::default()
        }).filter(run::Column::Id.eq(before.id)).exec(&db).await.unwrap();
        let mut events=Vec::new();
        assert!(schedule.settle_fresh_children_wait_with_observer(&parent.conversation_id,|event| {
            if observer_fails { panic!("metrics unavailable"); }
            events.push(event);
        }).await.unwrap());
        let settled=original_run(&db,&parent.conversation_id).await;
        assert_eq!(settled.status,"cancelled");
        assert!(settled.finished_at.is_some());
        assert!(schedule.read(1,&settled.schedule_id).await.unwrap().active_run_id.is_none());
        let retained=child_row::Entity::find_by_id(child.id).one(&db).await.unwrap().unwrap();
        assert_eq!(retained.state,"cancelled");
        if observer_fails { assert!(events.is_empty()); } else { assert_observed_schedule_waits_ended(&events,&aliases); }
        assert!(!schedule.settle_fresh_children_wait_with_observer(&parent.conversation_id,|_|panic!("terminal replay must not submit")).await.unwrap());
        assert_eq!(original_run(&db,&parent.conversation_id).await,settled);
        assert!(!settled.task_snapshot_json.contains("permission_ends"));
    }
}
