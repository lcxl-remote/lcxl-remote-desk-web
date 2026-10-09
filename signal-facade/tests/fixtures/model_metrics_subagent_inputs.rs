use super::*;
use desk_diagnose_core::{
    chat::{ToolCall,ToolSpec},
    model_observability::{
        Attribution,ConfigurationScope,InputConclusion,InputIssue,ObservationContext,ObservationEvent,
        ObservationPayload,ObservabilitySeam,Origin,PermissionOutcome,Protocol,Purpose,Stage,StageOutcome,Surface,
        aggregate::{contribution,merge,Count,MergeResult},tool::ToolBatch,
    },
    subagent::tools::{self,Operation},
};
use std::{collections::BTreeMap,sync::{Arc,Mutex}};

#[derive(Default)]
struct Recorder(Mutex<Vec<ObservationEvent>>);
impl ObservabilitySeam for Recorder {
    fn submit(&self,event:ObservationEvent) { self.0.lock().unwrap().push(event); }
}

fn batch(call: &ToolCall,seam:Arc<dyn ObservabilitySeam>) -> ToolBatch {
    let context=ObservationContext::new("server-model-call".into(),1_000,Attribution {
        provider_id:"provider".into(),model_id:"model".into(),model_name:"Model".into(),
        configuration_revision:"1".into(),contract_revision:"1".into(),surface:Surface::Assistant,
        purpose:Purpose::Agent,origin:Origin::User,configuration_scope:ConfigurationScope::Local,protocol:Protocol::OpenAiChatCompletions,
    },seam);
    ToolBatch::new(Some(context),std::slice::from_ref(call),&BTreeMap::from([(call.name.clone(),ToolSpec {
        name:call.name.clone(),description:String::new(),parameters_schema:serde_json::json!({"type":"object"}),
    })]))
}

fn merged_tool(recorder: &Recorder) -> ObservationEvent {
    let events=recorder.0.lock().unwrap();
    assert!(events.iter().all(|event|matches!(&event.payload,ObservationPayload::Tool(_))));
    let mut merged=events[0].clone();
    for event in &events[1..] { assert_ne!(merge(&mut merged,event),MergeResult::Conflict); }
    merged
}

#[tokio::test]
async fn missing_or_expired_subagent_references_reject_input_without_work_or_business_mutation() {
    for case in ["missing_status","missing_wait_member","stale_control","invalid_cursor"] {
        let db=database().await;
        let (mut parent,calls)=super::creation::runnable_parent(&db).await;
        let store=SubAgentStore::new(db.clone());
        let task=store.spawn_for_turn(&parent,&calls[0],&super::creation::spawn_request()).await.unwrap();
        let (name,args,issue)=match case {
            "missing_status"=>(tools::STATUS,serde_json::json!({"task_id":"private-missing-task"}),InputIssue::UnknownReference),
            "missing_wait_member"=>(tools::WAIT,serde_json::json!({"task_ids":[&task.task_id,"private-missing-task"],"mode":"all_terminal"}),InputIssue::UnknownReference),
            "stale_control"=>(tools::CANCEL,serde_json::json!({"task_id":&task.task_id,"expected_input_revision":1,"expected_control_revision":2}),InputIssue::ReferenceExpired),
            _=>(tools::LIST,serde_json::json!({"cursor":"private-invalid-cursor"}),InputIssue::UnknownReference),
        };
        let call=super::main_tools::committed_call(&db,&mut parent,"private-provider-call",name,args).await;
        let before=parent.clone();
        let task_before=load(&db,&task.task_id).await;
        let recorder=Arc::new(Recorder::default());
        let batch=batch(&call,recorder.clone());
        let operation=tools::parse_observed(&parent,&call,&batch.input(0)).unwrap();
        assert!(store.execute_main_tool_observed(&mut parent,&call,operation,"server-feedback",&batch.input(0)).await.is_err());
        assert_eq!(parent,before);
        assert_eq!(load(&db,&task.task_id).await,task_before);
        let event=merged_tool(&recorder);
        let ObservationPayload::Tool(tool)=&event.payload else { panic!("tool observation required") };
        assert_eq!(tool.conclusion,InputConclusion::Rejected);
        assert_eq!(tool.issue,issue);
        assert_eq!(tool.permission,PermissionOutcome::NotReached);
        assert_eq!(tool.stages[&Stage::Reference],StageOutcome::Failed);
        assert_eq!(tool.stages[&Stage::Dispatch],StageOutcome::NotReached);
        let counts=contribution(&event.payload,false);
        assert_eq!(counts.get(Count::Tools),1);
        assert_eq!(counts.get(Count::InputRejected),1);
        assert_eq!(counts.get(Count::ReferenceFailed),1);
        assert_eq!(counts.get(Count::ReferencePassed),0);
        let encoded=serde_json::to_string(&*recorder.0.lock().unwrap()).unwrap();
        assert!(!encoded.contains("private-provider-call") && !encoded.contains("private-missing-task") && !encoded.contains("private-invalid-cursor") && !encoded.contains(&task.task_id));
    }
}

#[tokio::test]
async fn valid_status_has_zero_native_operations_and_panicking_metrics_preserve_cached_receipt() {
    struct Panics;
    impl ObservabilitySeam for Panics { fn submit(&self,_event:ObservationEvent) { panic!("metrics unavailable"); } }
    let db=database().await;
    let (mut parent,calls)=super::creation::runnable_parent(&db).await;
    let store=SubAgentStore::new(db.clone());
    let task=store.spawn_for_turn(&parent,&calls[0],&super::creation::spawn_request()).await.unwrap();
    let call=super::main_tools::committed_call(&db,&mut parent,"private-status-call",tools::STATUS,serde_json::json!({"task_id":&task.task_id})).await;
    let recorder=Arc::new(Recorder::default());
    let observation_batch=batch(&call,recorder.clone());
    let receipt=store.execute_main_tool_observed(&mut parent,&call,Operation::Status { task_id:task.task_id.clone() },"status-result",&observation_batch.input(0)).await.unwrap();
    let event=merged_tool(&recorder);
    let ObservationPayload::Tool(tool)=&event.payload else { panic!("tool observation required") };
    assert_eq!(tool.conclusion,InputConclusion::Accepted);
    assert_eq!(tool.stages[&Stage::Reference],StageOutcome::Passed);
    assert_eq!(tool.stages[&Stage::Dispatch],StageOutcome::NotReached);
    assert_eq!(contribution(&event.payload,false).get(Count::OperationsDispatched),0);
    let before=parent.clone();
    let panic_batch=batch(&call,Arc::new(Panics));
    let repeated=store.execute_main_tool_observed(&mut parent,&call,Operation::Status { task_id:task.task_id.clone() },"unused-repeat",&panic_batch.input(0)).await.unwrap();
    assert_eq!(repeated.payload,receipt.payload);
    assert_eq!(repeated.result_message_id,receipt.result_message_id);
    assert_eq!(parent,before);
    assert!(!parent.encode_json_for_storage().unwrap().contains("server-model-call"));
}


#[tokio::test]
async fn subagent_capacity_and_unavailable_database_leave_parameter_conclusion_unknown() {
    for unavailable in [false,true] {
        let db=database().await;
        let (mut parent,calls)=super::creation::runnable_parent(&db).await;
        configure_limit(&db,1).await;
        let store=SubAgentStore::new(db.clone());
        store.spawn_for_turn(&parent,&calls[0],&super::creation::spawn_request()).await.unwrap();
        let call=&calls[1];
        let before=parent.clone();
        let recorder=Arc::new(Recorder::default());
        let batch=batch(call,recorder.clone());
        if unavailable { db.clone().close().await.unwrap(); }
        let result=store.execute_main_tool_observed(&mut parent,call,Operation::Spawn(super::creation::spawn_request()),"unused-result",&batch.input(0)).await;
        assert!(result.is_err());
        assert_eq!(parent,before);
        let event=merged_tool(&recorder);
        let ObservationPayload::Tool(tool)=&event.payload else { panic!("tool observation required") };
        assert_eq!(tool.conclusion,InputConclusion::Unknown);
        assert_eq!(tool.issue,InputIssue::None);
        assert_eq!(tool.permission,PermissionOutcome::NotReached);
        assert_eq!(tool.stages[&Stage::Dispatch],StageOutcome::NotReached);
        assert_eq!(contribution(&event.payload,false).get(Count::InputRejected),0);
    }
}

async fn seed_permission_wait(db: &sea_orm::DatabaseConnection,conversation_id: &str) -> PersistedAgentSession {
    let row=session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(conversation_id))
        .one(db).await.unwrap().unwrap();
    let mut child=PersistedAgentSession::decode_json(&row.state_json).unwrap();
    let request=desk_diagnose_core::permission_tools::build_permission_request(
        &ToolCall { id:"private-child-provider-call".into(),name:"request_permissions".into(),
            arguments_json:r#"{"items":[{"item_id":"capture","tool_name":"read_current_screen","exact_input":{"display":"private-display"},"suggested_ttl_seconds":300,"suggested_max_uses":1,"reason":"private-permission-reason"}]}"#.into() },
        &desk_diagnose_core::ai_assistant::ai_assistant_provider_registry(),
        "server-child-permission".into(),child.input_revision,"1970-01-01T00:00:00Z".into(),
    ).unwrap();
    child.permission_requests.push(request);
    session_row::Entity::update_many()
        .set(session_row::ActiveModel { state_json:Set(child.encode_json_for_storage().unwrap()),..Default::default() })
        .filter(session_row::Column::Id.eq(row.id)).exec(db).await.unwrap();
    child
}

#[tokio::test]
async fn prepared_child_controls_end_only_original_waits_after_commit_and_not_on_replay() {
    use desk_agent_protocol::ai_assistant::subagent::{AiAssistantSubAgentControl,SubAgentControlAction};
    use desk_diagnose_core::model_observability::{ObservationAlias,ObservationRelation};
    for adjust in [false,true] {
        let db=database().await;
        seed_parent(&db).await;
        let task=seed_task(&db,"a","goal-a").await;
        let sibling=seed_task(&db,"b","goal-b").await;
        let child=seed_permission_wait(&db,&task.child_conversation_id).await;
        let sibling_session=seed_permission_wait(&db,&sibling.child_conversation_id).await;
        let request=AiAssistantSubAgentControl { client_request_id:"owner-control".into(),task_id:task.binding.task_id.clone(),
            expected_input_revision:task.binding.input_revision,expected_control_revision:task.binding.control_revision,
            action:if adjust { SubAgentControlAction::Adjust { message:"private-new-objective".into() } } else { SubAgentControlAction::Cancel },
        };
        let txn=db.begin().await.unwrap();
        let parent=parent_on(&txn,"root","1","1").await.unwrap();
        let rolled_back=super::super::task_control::prepare_owner_control_on(&txn,&parent,&request,100).await.unwrap();
        txn.rollback().await.unwrap();
        drop(rolled_back);
        assert_eq!(load(&db,&task.binding.task_id).await,task);
        let row=session_row::Entity::find().filter(session_row::Column::ConversationId.eq(&child.conversation_id)).one(&db).await.unwrap().unwrap();
        assert_eq!(PersistedAgentSession::decode_json(&row.state_json).unwrap(),child);
        let txn=db.begin().await.unwrap();
        let parent=parent_on(&txn,"root","1","1").await.unwrap();
        let prepared=super::super::task_control::prepare_owner_control_on(&txn,&parent,&request,100).await.unwrap();
        let receipt=serde_json::to_string(&prepared.outcome).unwrap();
        assert!(!receipt.contains("permission_ends") && !receipt.contains("server-child-permission"));
        txn.commit().await.unwrap();
        let mut events=Vec::new();
        prepared.permission_ends.submit(100,|event|events.push(event));
        assert_eq!(events.len(),1);
        assert_eq!(events[0].relation,Some(ObservationRelation::ResolveTool(
            ObservationAlias::permission_request(&child.conversation_id,"server-child-permission").unwrap(),
        )));
        let ObservationPayload::Tool(tool)=&events[0].payload else { panic!("permission fact required") };
        assert_eq!(tool.permission,if adjust { PermissionOutcome::Revoked } else { PermissionOutcome::Cancelled });
        assert_eq!(tool.conclusion,InputConclusion::Unknown);
        assert_eq!(tool.issue,InputIssue::None);
        assert!(!tool.stages.contains_key(&Stage::Dispatch));
        let current=session_row::Entity::find().filter(session_row::Column::ConversationId.eq(&child.conversation_id)).one(&db).await.unwrap().unwrap();
        let current=PersistedAgentSession::decode_json(&current.state_json).unwrap();
        assert_eq!(current.input_revision,if adjust { child.input_revision+1 } else { child.input_revision });
        if adjust { assert_eq!(current.permission_requests[0].state,desk_diagnose_core::dynamic_run::PermissionRequestState::NeedsRevalidation); }
        let other=session_row::Entity::find().filter(session_row::Column::ConversationId.eq(&sibling_session.conversation_id)).one(&db).await.unwrap().unwrap();
        assert_eq!(PersistedAgentSession::decode_json(&other.state_json).unwrap(),sibling_session);
        let txn=db.begin().await.unwrap();
        let parent=parent_on(&txn,"root","1","1").await.unwrap();
        let replay=super::super::task_control::prepare_owner_control_on(&txn,&parent,&request,200).await.unwrap();
        assert_eq!(replay.outcome,prepared.outcome);
        txn.commit().await.unwrap();
        replay.permission_ends.submit(200,|event|events.push(event));
        assert_eq!(events.len(),1);
        let encoded=serde_json::to_string(&events).unwrap();
        for secret in ["private-child-provider-call","private-display","private-permission-reason","private-new-objective"] { assert!(!encoded.contains(secret)); }
    }
}

#[tokio::test]
async fn goal_source_ends_are_transactional_isolated_and_not_inferred_from_a_late_clock() {
    use desk_diagnose_core::model_observability::{ObservationAlias,ObservationRelation};
    for (state,now,expected) in [
        (GoalState::Paused(GoalPauseReason::Owner),20_000,None),
        (GoalState::Queued,20_000,Some(PermissionOutcome::Expired)),
        (GoalState::Cancelled,100,Some(PermissionOutcome::Cancelled)),
        (GoalState::Failed,100,Some(PermissionOutcome::Cancelled)),
    ] {
        let db=database().await;
        seed_parent(&db).await;
        let task=seed_task(&db,"a","goal-a").await;
        let sibling=seed_task(&db,"b","goal-b").await;
        let child=seed_permission_wait(&db,&task.child_conversation_id).await;
        let other=seed_permission_wait(&db,&sibling.child_conversation_id).await;
        let txn=db.begin().await.unwrap();
        let rolled_back=apply_goal_source_on(&txn,"root","1","1","goal-a",state,now).await.unwrap();
        txn.rollback().await.unwrap();
        drop(rolled_back);
        assert_eq!(load(&db,&task.binding.task_id).await,task);
        assert_eq!(load(&db,&sibling.binding.task_id).await,sibling);
        let row=session_row::Entity::find().filter(session_row::Column::ConversationId.eq(&child.conversation_id)).one(&db).await.unwrap().unwrap();
        assert_eq!(PersistedAgentSession::decode_json(&row.state_json).unwrap(),child);

        let txn=db.begin().await.unwrap();
        let prepared=apply_goal_source_on(&txn,"root","1","1","goal-a",state,now).await.unwrap();
        txn.commit().await.unwrap();
        let mut events=Vec::new();
        prepared.submit(now,|event|events.push(event));
        assert_eq!(events.len(),usize::from(expected.is_some()));
        if let Some(outcome)=expected {
            let event=&events[0];
            assert!(event.is_bounded());
            assert_eq!(event.relation,Some(ObservationRelation::ResolveTool(
                ObservationAlias::permission_request(&child.conversation_id,"server-child-permission").unwrap())));
            let ObservationPayload::Tool(tool)=&event.payload else { panic!("permission fact required") };
            assert_eq!(tool.permission,outcome);
            assert_eq!(tool.conclusion,InputConclusion::Unknown);
            assert_eq!(tool.issue,InputIssue::None);
            assert!(!tool.stages.contains_key(&Stage::Dispatch));
        } else {
            let row=session_row::Entity::find().filter(session_row::Column::ConversationId.eq(&child.conversation_id)).one(&db).await.unwrap().unwrap();
            let current=PersistedAgentSession::decode_json(&row.state_json).unwrap();
            assert_eq!(current.permission_requests[0].state,desk_diagnose_core::dynamic_run::PermissionRequestState::Pending);
        }
        assert_eq!(load(&db,&sibling.binding.task_id).await,sibling);
        let row=session_row::Entity::find().filter(session_row::Column::ConversationId.eq(&other.conversation_id)).one(&db).await.unwrap().unwrap();
        assert_eq!(PersistedAgentSession::decode_json(&row.state_json).unwrap(),other);
        let txn=db.begin().await.unwrap();
        let replay=apply_goal_source_on(&txn,"root","1","1","goal-a",state,now+1).await.unwrap();
        txn.commit().await.unwrap();
        replay.submit(now+1,|_|panic!("unchanged or terminal source cannot create another end"));
        let encoded=serde_json::to_string(&events).unwrap();
        for secret in ["private-child-provider-call","private-display","private-permission-reason"] { assert!(!encoded.contains(secret)); }
    }
}

#[tokio::test]
async fn task_failure_and_deadline_settlement_keep_distinct_permission_outcomes() {
    use desk_diagnose_core::model_observability::permission::PendingEnds;
    let db=database().await;
    seed_parent(&db).await;
    let mut task=seed_task(&db,"a","goal-a").await;
    let child=seed_permission_wait(&db,&task.child_conversation_id).await;
    task.fail("private-failure-containing-deadline-word","1970-01-01T00:00:00.100Z").unwrap();
    let before=child.clone();
    for (deadline_reached,expected) in [(false,PermissionOutcome::Unavailable),(true,PermissionOutcome::Expired)] {
        let mut events=Vec::new();
        PendingEnds::task_control(&child,&task,deadline_reached).submit(30_000,|event|events.push(event));
        assert_eq!(events.len(),1);
        let ObservationPayload::Tool(tool)=&events[0].payload else { panic!("permission fact required") };
        assert_eq!(tool.permission,expected);
        assert_eq!(tool.conclusion,InputConclusion::Unknown);
        assert_eq!(tool.issue,InputIssue::None);
        let counts=contribution(&events[0].payload,false);
        assert_eq!(counts.get(Count::InputRejected),0);
        assert_eq!(counts.get(Count::PermissionDenied),0);
        assert!(!tool.stages.contains_key(&Stage::Dispatch));
        assert!(!serde_json::to_string(&events).unwrap().contains("private-failure-containing-deadline-word"));
        PendingEnds::task_control(&child,&task,deadline_reached).submit(30_000,|_|panic!("metrics unavailable"));
        assert_eq!(child,before);
    }
}

#[tokio::test]
async fn root_removal_captures_only_original_pending_waits_after_the_business_commit() {
    use desk_diagnose_core::{dynamic_run::PermissionRequestState,model_observability::{ObservationAlias,ObservationRelation}};
    let db=database().await;
    seed_parent(&db).await;
    let task=seed_task(&db,"a","goal-a").await;
    let sibling=seed_task(&db,"b","goal-b").await;
    let child=seed_permission_wait(&db,&task.child_conversation_id).await;
    let other=seed_permission_wait(&db,&sibling.child_conversation_id).await;
    let mut parent=seed_permission_wait(&db,"root").await;
    parent.input_revision=2; parent.begin_focus_epoch(2,[]).unwrap();
    parent.permission_requests[0].input_revision=2;
    parent.permission_requests[0].state=PermissionRequestState::Pending;
    let mut approved=parent.permission_requests[0].clone();
    approved.request_id="server-approved-permission".into(); approved.state=PermissionRequestState::Approved;
    parent.permission_requests.push(approved);
    let mut older=parent.permission_requests[0].clone();
    older.request_id="server-old-permission".into(); older.input_revision=1;
    older.state=PermissionRequestState::NeedsRevalidation;
    parent.permission_requests.push(older);
    session_row::Entity::update_many().set(session_row::ActiveModel {
        state_json:Set(parent.encode_json_for_storage().unwrap()),..Default::default()
    }).filter(session_row::Column::ConversationId.eq("root")).exec(&db).await.unwrap();

    let txn=db.begin().await.unwrap();
    let rolled_back=close_root_on(&txn,&parent,100).await.unwrap();
    assert!(deleted_on(&txn,"root").await.unwrap());
    txn.rollback().await.unwrap();
    drop(rolled_back);
    assert!(!deleted_on(&db,"root").await.unwrap());
    assert_eq!(load(&db,&task.binding.task_id).await,task);
    assert_eq!(load(&db,&sibling.binding.task_id).await,sibling);
    let txn=db.begin().await.unwrap();
    let prepared=close_root_on(&txn,&parent,200).await.unwrap();
    txn.commit().await.unwrap();
    let mut events=Vec::new();
    prepared.submit(200,|event|events.push(event));
    assert_eq!(events.len(),3);
    let aliases=events.iter().map(|event|match &event.relation {
        Some(ObservationRelation::ResolveTool(alias))=>alias.clone(),_=>panic!("original request relation required"),
    }).collect::<Vec<_>>();
    for conversation in [&parent.conversation_id,&child.conversation_id,&other.conversation_id] {
        assert!(aliases.contains(&ObservationAlias::permission_request(conversation,"server-child-permission").unwrap()));
    }
    assert!(!aliases.contains(&ObservationAlias::permission_request("root","server-approved-permission").unwrap()));
    assert!(!aliases.contains(&ObservationAlias::permission_request("root","server-old-permission").unwrap()));
    for event in &events {
        assert!(event.is_bounded());
        let ObservationPayload::Tool(tool)=&event.payload else { panic!("permission fact required") };
        assert_eq!(tool.permission,PermissionOutcome::Cancelled);
        assert_eq!(tool.conclusion,InputConclusion::Unknown);
        assert_eq!(tool.issue,InputIssue::None);
        assert!(!tool.stages.contains_key(&Stage::Dispatch));
    }
    assert_eq!(load(&db,&task.binding.task_id).await.state,SubAgentState::Cancelled);
    assert_eq!(load(&db,&sibling.binding.task_id).await.state,SubAgentState::Cancelled);
    let encoded=serde_json::to_string(&events).unwrap();
    for secret in ["private-child-provider-call","private-display","private-permission-reason"] { assert!(!encoded.contains(secret)); }
    assert!(!parent.encode_json_for_storage().unwrap().contains("permission_ends"));
}
