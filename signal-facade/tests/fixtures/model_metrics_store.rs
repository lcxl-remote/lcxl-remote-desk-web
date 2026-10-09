use super::*;
use std::collections::BTreeMap;
use sea_orm::ActiveModelTrait;
use desk_signal_facade::service::model_metrics::{ResolvedQuery, timestamp};
use desk_diagnose_core::model_observability::aggregate::Count;

const NOW: i64 = 1_800_000_000_000;

#[tokio::test]
async fn record_drill_down_filters_observed_states_and_derives_current_input_counts() {
    let store = fixture_store(NOW-DAY).await;
    let mut returned = call_event("count-call", NOW, RequestOutcome::Returned);
    let ObservationPayload::Call(call) = &mut returned.payload else { unreachable!(); };
    call.generated_tool_count=Some(2); call.timing.first_content_ms=Some(50);
    call.usage.normalized.input_tokens=Some(9_007_199_254_740_993);
    call.usage.normalized.output_tokens=Some(7);
    let mut failed = call_event("slow-failure", NOW+1_000, RequestOutcome::Timeout);
    let ObservationPayload::Call(call) = &mut failed.payload else { unreachable!(); };
    call.timing.duration_ms=Some(7_000);
    let mut inputs: Vec<_> = [InputConclusion::Accepted, InputConclusion::Rejected].into_iter().enumerate().map(|(index,conclusion)| {
        let mut input=correction_input(&format!("count-call.tool.{index}"), NOW, conclusion);
        input.relation=None; input.call_id=Some("count-call".into());
        let ObservationPayload::Tool(tool)=&mut input.payload else { unreachable!(); }; tool.ordinal=index as u32; tool.permission=PermissionOutcome::Waiting; tool.stage_duration_ms=Some(12);
        input
    }).collect();
    let mut operation=returned.clone(); operation.object_id="count-call.operation.0".into(); operation.event_id="count-call.operation.terminal".into(); operation.call_id=Some("count-call".into()); operation.phase=ObservationPhase::Completed;
    operation.payload=ObservationPayload::Operation(OperationSnapshot { tool_observation_id:Some(inputs[0].object_id.clone()),tool_key:Some("read_file".into()),ordinal:0,dispatched:Some(true),outcome:OperationOutcome::Unknown,duration_ms:Some(100) });
    let mut attempt=returned.clone(); attempt.object_id="count-call.attempt.0".into(); attempt.event_id="count-call.attempt.terminal".into(); attempt.call_id=Some("count-call".into());
    attempt.payload=ObservationPayload::Attempt(AttemptSnapshot {ordinal:0,outbound_at_ms:NOW,outcome:RequestOutcome::Returned,http_status:Some(200),timing:Timing {duration_ms:Some(90),first_content_ms:Some(40),..Default::default()} });
    let mut events=vec![returned,failed,operation,attempt]; events.extend(inputs.clone());
    store.persist(&events,NOW+2_000).await.unwrap();
    assert_eq!(store.aggregate(NOW+2_000).await.unwrap().discarded,0);
    let range=query(NOW-HOUR,NOW+HOUR);
    let page=store.calls(&range,NOW+3_000).await.unwrap();
    let row=page.records.iter().find(|row|row.id=="count-call").unwrap();
    assert_eq!(row.tool_count.as_deref(),Some("2")); assert_eq!(row.input_rejected_count.as_deref(),Some("1")); assert_eq!(row.tool_counts_status,SampleStatus::Complete);
    assert_eq!(row.input_tokens.as_deref(),Some("9007199254740993"));
    let unknown=page.records.iter().find(|row|row.id=="slow-failure").unwrap(); assert!(unknown.tool_count.is_none()); assert_eq!(unknown.tool_counts_status,SampleStatus::Unknown);
    let mut selected=range.clone(); selected.calls.outcome=Some("request_error".into()); selected.calls.min_duration_ms=Some(5_000);
    assert_eq!(store.calls(&selected,NOW+3_000).await.unwrap().records[0].id,"slow-failure");
    assert!(store.overview(&selected,NOW+3_000).await.is_err()); assert!(store.groups(&selected,false,NOW+3_000).await.is_err());
    selected.calls=desk_signal_facade::service::model_metrics::CallFilters {kind:Some(MetricRecordKind::Tool),outcome:Some("rejected".into()),permission:Some(PermissionOutcome::Waiting),..Default::default()};
    assert_eq!(store.calls(&selected,NOW+3_000).await.unwrap().records.len(),1);
    selected.calls=desk_signal_facade::service::model_metrics::CallFilters {kind:Some(MetricRecordKind::Operation),outcome:Some("unknown".into()),dispatched:Some(true),..Default::default()};
    assert_eq!(store.calls(&selected,NOW+3_000).await.unwrap().records.len(),1);
    selected.calls=desk_signal_facade::service::model_metrics::CallFilters {kind:Some(MetricRecordKind::Attempt),latency:Some(MetricLatency::FirstContent),min_duration_ms:Some(40),..Default::default()};
    assert_eq!(store.calls(&selected,NOW+3_000).await.unwrap().records.len(),1);
    selected.calls.min_duration_ms=Some(41); assert!(store.calls(&selected,NOW+3_000).await.unwrap().records.is_empty());
    let detail=store.call_detail("count-call").await.unwrap().unwrap(); assert_eq!(detail.call.input_rejected_count.as_deref(),Some("1"));
    // Repeated or later permission facts replace the original input, never add a child.
    inputs[1].event_id="count-call.tool.1.permission".into(); inputs[1].sequence+=1;
    let ObservationPayload::Tool(tool)=&mut inputs[1].payload else { unreachable!(); }; tool.permission=PermissionOutcome::Denied;
    store.persist(&[inputs[1].clone(),inputs[1].clone()],NOW+4_000).await.unwrap(); store.aggregate(NOW+4_000).await.unwrap();
    let detail=store.call_detail("count-call").await.unwrap().unwrap(); assert_eq!(detail.call.tool_count.as_deref(),Some("2")); assert_eq!(detail.call.input_rejected_count.as_deref(),Some("1"));
    assert_resource_accounting(&store).await;
    // Simulate absent retained compact evidence: don't invent complete zero counts.
    compact::Entity::delete_by_id(&inputs[0].object_id).exec(&store.db).await.unwrap();
    let detail=store.call_detail("count-call").await.unwrap().unwrap(); assert_eq!(detail.call.tool_count.as_deref(),Some("1")); assert_eq!(detail.call.tool_counts_status,SampleStatus::Partial);
}

#[tokio::test]
async fn grouping_sorts_before_top_n_and_preserves_tool_paths_models_and_stages() {
    let store = fixture_store(NOW-DAY).await;
    let mut calls: Vec<_> = (0..20).map(|index| call_event(&format!("frequent-{index}"), NOW, RequestOutcome::Returned)).collect();
    let mut failed = call_event("rare-failure", NOW, RequestOutcome::HttpError);
    failed.attribution.model_id = "model-failed".into();
    calls.push(failed);
    let mut inputs = Vec::new();
    for (index, model) in ["model-1", "model-failed"].into_iter().enumerate() {
        let mut input = correction_input(&format!("field-input-{index}"), NOW, InputConclusion::Rejected);
        input.relation = None;
        input.attribution.model_id = model.into();
        input.attribution.configuration_revision = "2".into();
        let ObservationPayload::Tool(tool) = &mut input.payload else { unreachable!(); };
        tool.schema_path = Some("$.items[].count".into()); tool.stage_duration_ms = Some(12);
        inputs.push(input);
    }
    calls.extend(inputs.clone());
    store.persist(&calls, NOW+100).await.unwrap();
    assert_eq!(store.aggregate(NOW+100).await.unwrap().discarded, 0);
    let mut range = query(NOW-HOUR, NOW+HOUR); range.limit = 1; range.group_sort = Some(MetricGroupSort::RequestFailureRate);
    let models = store.groups(&range, false, NOW+2_000).await.unwrap();
    assert_eq!(models.groups[0].model_id.as_deref(), Some("model-failed"));
    assert_eq!(models.groups[0].configurations[0].revisions, vec!["1", "2"]);
    assert!(!models.groups[0].configurations_limited);
    assert_eq!(count(models.other.as_ref().unwrap(), Count::Calls), Some("20"));
    range.group_sort = Some(MetricGroupSort::InputRejected);
    let tools = store.groups(&range, true, NOW+2_000).await.unwrap();
    let group = &tools.groups[0];
    assert_eq!(group.associated_models.len(), 2);
    assert!(group.provider_id.is_none() && group.model_id.is_none());
    assert_eq!(group.associated_models[0].tool_inputs, "1");
    assert_eq!(group.summary.schema_paths[0].path, "$.items[].count");
    assert_eq!(group.summary.schema_paths[0].count, "2");
    assert_eq!(group.summary.other_schema_errors.as_deref(), Some("0"));
    assert_eq!(group.summary.stages.iter().find(|row| row.stage == "schema" && row.outcome == "failed").unwrap().count, "2");
    assert_eq!(group.summary.other_duration.iter().find(|row| row.kind == "tool").unwrap().summary.count, "2");
    let mut late = inputs[0].clone(); late.sequence += 1; late.event_id = "late-field-permission".into();
    let ObservationPayload::Tool(tool) = &mut late.payload else { unreachable!(); }; tool.permission = PermissionOutcome::Denied;
    store.persist(&[late.clone(), late], NOW+3_000).await.unwrap();
    store.aggregate(NOW+3_000).await.unwrap();
    let tools = store.groups(&range, true, NOW+4_000).await.unwrap();
    assert_eq!(tools.groups[0].summary.schema_paths[0].count, "2");
    assert_eq!(count(&tools.groups[0].summary, Count::PermissionDenied), Some("1"));
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn call_pagination_keeps_reception_fence_and_rejects_reused_filters() {
    let store = fixture_store(NOW-DAY).await;
    let calls: Vec<_> = (1..=3).map(|index| call_event(&format!("page-call-{index}"), NOW+index*1_000, RequestOutcome::Returned)).collect();
    store.persist(&calls, NOW+4_000).await.unwrap(); store.aggregate(NOW+4_000).await.unwrap();
    let mut range = query(NOW-HOUR, NOW+HOUR); range.limit = 1;
    let first = store.calls(&range, NOW+5_000).await.unwrap();
    assert_eq!(first.records[0].id, "page-call-3");
    let late = call_event("late-old-call", NOW+1_500, RequestOutcome::Returned);
    store.persist(&[late], NOW+6_000).await.unwrap(); store.aggregate(NOW+6_000).await.unwrap();
    range.cursor = first.next_cursor;
    let second = store.calls(&range, NOW+7_000).await.unwrap();
    assert_eq!(second.records[0].id, "page-call-2");
    assert_eq!(second.received_before, first.received_before);
    range.cursor = second.next_cursor;
    let third = store.calls(&range, NOW+7_000).await.unwrap();
    assert_eq!(third.records[0].id, "page-call-1"); assert!(third.next_cursor.is_none());
    range.provider_id = Some("another-provider".into());
    assert!(store.calls(&range, NOW+7_000).await.is_err());
}

async fn assert_resource_accounting(store: &Store) {
    let row=settings::Entity::find_by_id(1).one(&store.db).await.unwrap().unwrap();
    let events=event::Entity::find().all(&store.db).await.unwrap();
    let compact=compact::Entity::find().all(&store.db).await.unwrap();
    let details=detail::Entity::find().all(&store.db).await.unwrap();
    let rollups=rollup::Entity::find().all(&store.db).await.unwrap();
    assert_eq!((row.event_rows,row.compact_rows,row.detail_rows,row.rollup_rows),
        (events.len() as i64,compact.len() as i64,details.len() as i64,rollups.len() as i64));
    let bytes=events.iter().map(|row| row.storage_bytes).chain(compact.iter().map(|row| row.storage_bytes))
        .chain(details.iter().map(|row| row.storage_bytes)).chain(rollups.iter().map(|row| row.storage_bytes)).sum::<i64>();
    assert_eq!(row.storage_used_bytes,bytes);
    assert!(bytes>=0);
}

async fn minimum_row_budgets(store: &Store) {
    let mut config=store.load_settings().await.unwrap();
    config.event_row_budget=1_000; config.detail_row_budget=1_000;
    config.compact_row_budget=1_000; config.rollup_row_budget=1_000;
    store.save_settings(config,NOW).await.unwrap().unwrap();
}

async fn thousand_calls(store: &Store) {
    for batch in 0..10 {
        let events: Vec<_>=(0..100).map(|index| call_event(&format!("call-{}",batch*100+index),NOW,RequestOutcome::Returned)).collect();
        assert_eq!(store.persist(&events,NOW+500).await.unwrap(),0);
        assert_eq!(store.aggregate(NOW+500).await.unwrap().applied,100);
    }
}

fn call_event(id: &str, started: i64, outcome: RequestOutcome) -> ObservationEvent {
    ObservationEvent {
        schema_version: EVENT_SCHEMA_VERSION, event_id: format!("{id}.terminal"), object_id: id.into(), call_id: None,
        phase: ObservationPhase::Terminal, sequence: 0, started_at_ms: started, occurred_at_ms: started+100,relation:None,
        attribution: Attribution {
            provider_id:"provider-1".into(), model_id:"model-1".into(), model_name:"Test model".into(),
            configuration_revision:"1".into(), contract_revision:"1".into(),
            purpose:Purpose::Agent, surface:Surface::Assistant, origin:Origin::User,
            configuration_scope:ConfigurationScope::Local, protocol:Protocol::OpenAiChatCompletions,
        },
        payload:ObservationPayload::Call(CallSnapshot { outcome, output:if outcome==RequestOutcome::Returned { OutputOutcome::Accepted } else { OutputOutcome::NotEvaluated },
            timing:Timing { duration_ms:Some(100),..Default::default() },..Default::default() }),
    }
}

fn query(from: i64, to: i64) -> ResolvedQuery {
    ResolvedQuery::resolve(&MetricsQuery { from:Some(timestamp(from)),to:Some(timestamp(to)),..Default::default() },NOW+DAY).unwrap()
}

#[tokio::test]
async fn unsent_unresolved_calls_keep_one_unknown_group_and_do_not_change_model_quality_denominators() {
    let store = fixture_store(NOW-DAY).await;
    let mut unsent = call_event("unresolved-preflight", NOW, RequestOutcome::NotStarted);
    unsent.attribution = Attribution::unresolved(Purpose::Agent, Surface::Assistant, Origin::User);
    if let ObservationPayload::Call(call) = &mut unsent.payload {
        call.not_started_reason = Some(NotStartedReason::Configuration);
    }
    let returned = call_event("actual-model-call", NOW, RequestOutcome::Returned);
    store.persist(&[unsent.clone(), returned], NOW+1_000).await.unwrap();
    store.aggregate(NOW+1_000).await.unwrap();
    let range = query(NOW-HOUR, NOW+HOUR);
    let overview = store.overview(&range, NOW+2_000).await.unwrap();
    assert_eq!(count(&overview.summary, Count::Calls), Some("2"));
    assert_eq!(count(&overview.summary, Count::NotStarted), Some("1"));
    let request = overview.summary.rates.iter().find(|rate| rate.key == "request_failure").unwrap();
    assert_eq!((request.numerator.as_deref(), request.denominator.as_deref()), (Some("0"), Some("1")));
    let groups = store.groups(&range, false, NOW+2_000).await.unwrap();
    assert_eq!(groups.groups.len(), 2);
    let unknown = groups.groups.iter().find(|group| group.model_id.is_none()).unwrap();
    assert!(unknown.provider_id.is_none() && unknown.model_name.is_none());
    assert_eq!(count(&unknown.summary, Count::Calls), Some("1"));
    let rate = unknown.summary.rates.iter().find(|rate| rate.key == "request_failure").unwrap();
    assert_eq!(rate.denominator.as_deref(), Some("0"));
    assert!(rate.value.is_none());
    let mut selected = range.clone();
    selected.model_id = Some("model-1".into());
    let known = store.overview(&selected, NOW+2_000).await.unwrap();
    assert_eq!(count(&known.summary, Count::Calls), Some("1"));
    assert_eq!(count(&known.summary, Count::NotStarted), None);
    let restored = detail::Entity::find_by_id(&unsent.object_id).one(&store.db).await.unwrap().unwrap();
    let event: ObservationEvent = decode(&restored.snapshot_json).unwrap();
    let dto = record(&event, false);
    assert!(dto.model_id.is_empty() && dto.provider_id.is_empty() && dto.model_name.is_empty());
    assert_eq!(dto.not_started_reason.as_deref(), Some("configuration"));
    store.persist(&[unsent], NOW+3_000).await.unwrap();
    store.aggregate(NOW+3_000).await.unwrap();
    assert_eq!(count(&store.overview(&range, NOW+4_000).await.unwrap().summary, Count::Calls), Some("2"));
    assert_resource_accounting(&store).await;
}

fn bound_tool(alias: ObservationAlias) -> ObservationEvent {
    let mut event=call_event("original-call.tool.0",NOW,RequestOutcome::Returned);
    event.call_id=Some("original-call".into()); event.event_id="original-tool.binding".into();
    event.phase=ObservationPhase::Stage; event.sequence=1;
    event.relation=Some(ObservationRelation::Bind(alias));
    event.payload=ObservationPayload::Tool(ToolSnapshot {
        ordinal:0,tool_key:"execute_confirmed_ui_action".into(),
        stages:BTreeMap::from([(Stage::Schema,StageOutcome::Passed)]),conclusion:InputConclusion::Unknown,issue:InputIssue::None,
        schema_path:None,permission:PermissionOutcome::NotReached,correction_of:None,correction_status:CorrectionStatus::Uncorrelated,correction_input:None,
        argument_bytes:1,stage_duration_ms:None,
    });
    event
}

fn deferred_operation(alias: ObservationAlias,outcome: OperationOutcome) -> ObservationEvent {
    ObservationEvent::deferred_operation(alias,0,ObservationPhase::Completed,0,NOW+1_000,NOW+2_000,
        OperationSnapshot { tool_observation_id:None,tool_key:None,ordinal:0,dispatched:Some(true),outcome,duration_ms:None })
}

fn operation_dispatch(alias: ObservationAlias) -> ObservationEvent {
    ObservationEvent::deferred_operation(alias,0,ObservationPhase::Dispatched,0,NOW+1_000,NOW+1_000,
        OperationSnapshot { tool_observation_id:None,tool_key:None,ordinal:0,dispatched:Some(true),
            outcome:OperationOutcome::Pending,duration_ms:None })
}

fn operation_result_without_start(alias: ObservationAlias,phase: ObservationPhase,outcome: OperationOutcome,received: i64) -> ObservationEvent {
    ObservationEvent::deferred_started_operation(alias,0,phase,0,received,
        OperationSnapshot { tool_observation_id:None,tool_key:None,ordinal:0,dispatched:Some(true),outcome,duration_ms:None })
}

#[tokio::test]
async fn completion_before_dispatch_uses_original_cohort_without_mutable_business_time() {
    let store=fixture_store(NOW-DAY).await;
    let alias=ObservationAlias::provider_work("work-cohort").unwrap();
    store.persist(&[bound_tool(alias.clone())],NOW+100).await.unwrap();
    store.aggregate(NOW+100).await.unwrap();
    let received=NOW+3*HOUR;
    let completion=operation_result_without_start(alias.clone(),ObservationPhase::Completed,OperationOutcome::Verified,received);
    store.persist(std::slice::from_ref(&completion),received).await.unwrap();
    assert_eq!(store.aggregate(received).await.unwrap().applied,0);
    assert_eq!(detail::Entity::find().filter(detail::Column::Kind.eq("operation")).count(&store.db).await.unwrap(),0);
    store.persist(&[operation_dispatch(alias)],received+100).await.unwrap();
    let result=store.aggregate(received+100).await.unwrap();
    assert_eq!((result.applied,result.discarded),(2,0));
    let row=detail::Entity::find_by_id(&completion.object_id).one(&store.db).await.unwrap().unwrap();
    let event: ObservationEvent=decode(&row.snapshot_json).unwrap();
    assert_eq!(event.started_at_ms,NOW+1_000);
    assert_eq!(event.occurred_at_ms,received);
    assert_eq!(event.attribution.model_id,"model-1");
    let initial=store.overview(&query(NOW-HOUR,NOW+HOUR),received+HOUR).await.unwrap();
    assert_eq!(count(&initial.summary,Count::OperationsVerified),Some("1"));
    let completion_period=store.overview(&query(NOW+2*HOUR,NOW+4*HOUR),received+HOUR).await.unwrap();
    assert_eq!(count(&completion_period.summary,Count::OperationsVerified),None);
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn lost_dispatch_does_not_fabricate_a_result_time_cohort() {
    let store=fixture_store(NOW-DAY).await;
    let alias=ObservationAlias::provider_work("lost-dispatch").unwrap();
    store.persist(&[bound_tool(alias.clone())],NOW+100).await.unwrap();
    store.aggregate(NOW+100).await.unwrap();
    let completion=operation_result_without_start(alias,ObservationPhase::Completed,OperationOutcome::Verified,NOW+2_000);
    store.persist(std::slice::from_ref(&completion),NOW+2_000).await.unwrap();
    assert_eq!(store.aggregate(NOW+2_001).await.unwrap().discarded,0);
    assert_eq!(store.aggregate(NOW+32_001).await.unwrap().discarded,0);
    assert!(detail::Entity::find_by_id(&completion.object_id).one(&store.db).await.unwrap().is_none());
    let row=event::Entity::find().filter(event::Column::EventId.eq(&completion.event_id)).one(&store.db).await.unwrap().unwrap();
    assert!(row.applied && row.association_pending && row.discarded_reason.is_none());
    let gaps=store.unassociated(&unassociated_query(NOW,NOW+HOUR,50),NOW+32_001).await.unwrap();
    assert_eq!(gaps.records.len(),1);
    assert_eq!(gaps.records[0].missing,AssociationGapKind::OperationStart);
    assert_eq!(gaps.records[0].state,AssociationGapState::Unavailable);
    assert!(gaps.records[0].started_at.is_none());
    assert_eq!(gaps.records[0].original_model.as_ref().unwrap().model_id,"model-1");
    assert_eq!(gaps.records[0].tool_observation_id.as_deref(),Some("original-call.tool.0"));
    assert_eq!(count(&store.overview(&query(NOW,NOW+HOUR),NOW+HOUR).await.unwrap().summary,Count::OperationsVerified),None);
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn unresolved_links_cannot_starve_a_dispatch_and_its_waiting_completion() {
    let store=fixture_store(NOW-DAY).await;
    let missing=ObservationAlias::source_message("uncollected-message",0).unwrap();
    let links: Vec<_>=(0..100).map(|ordinal|ObservationEvent::link_alias(missing.clone(),
        ObservationAlias::provider_work(&format!("unrelated-{ordinal}")).unwrap(),NOW+100)).collect();
    store.persist(&links,NOW+100).await.unwrap();
    assert_eq!(store.aggregate(NOW+100).await.unwrap().applied,0);
    let alias=ObservationAlias::provider_work("ready-work").unwrap();
    let completion=operation_result_without_start(alias.clone(),ObservationPhase::Completed,OperationOutcome::Verified,NOW+2_000);
    store.persist(&[bound_tool(alias.clone()),completion.clone(),operation_dispatch(alias)],NOW+2_000).await.unwrap();
    let result=store.aggregate(NOW+2_001).await.unwrap();
    assert_eq!((result.applied,result.discarded),(3,0));
    assert!(detail::Entity::find_by_id(&completion.object_id).one(&store.db).await.unwrap().is_some());
    assert_eq!(event::Entity::find().filter(event::Column::AssociationPending.eq(true)).count(&store.db).await.unwrap(),100);
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn terminal_result_refines_unknown_and_late_progress_cannot_undo_it() {
    for late_progress in [false,true] {
        let store=fixture_store(NOW-DAY).await;
        let alias=ObservationAlias::provider_work("refined-work").unwrap();
        store.persist(&[bound_tool(alias.clone()),operation_dispatch(alias.clone())],NOW+1_100).await.unwrap();
        store.aggregate(NOW+1_100).await.unwrap();
        let unknown=operation_result_without_start(alias.clone(),ObservationPhase::Progress,OperationOutcome::Unknown,NOW+2_000);
        let terminal=operation_result_without_start(alias,ObservationPhase::Completed,OperationOutcome::Verified,NOW+3_000);
        let (first,second)=if late_progress { (terminal,unknown) } else { (unknown,terminal) };
        for event in [&first,&second] {
            store.persist(std::slice::from_ref(event),NOW+3_100).await.unwrap();
            assert_eq!(store.aggregate(NOW+3_100).await.unwrap().discarded,0);
        }
        let row=detail::Entity::find_by_id(&first.object_id).one(&store.db).await.unwrap().unwrap();
        let observed: ObservationEvent=decode(&row.snapshot_json).unwrap();
        assert_eq!(observed.started_at_ms,NOW+1_000);
        assert!(matches!(observed.payload,ObservationPayload::Operation(OperationSnapshot { outcome:OperationOutcome::Verified,.. })));
        let overview=store.overview(&query(NOW-HOUR,NOW+HOUR),NOW+HOUR).await.unwrap();
        assert_eq!(count(&overview.summary,Count::OperationsDispatched),Some("1"));
        assert_eq!(count(&overview.summary,Count::OperationsVerified),Some("1"));
        assert_eq!(count(&overview.summary,Count::OperationsUnknown).unwrap_or("0"),"0");
        store.persist(&[first,second],NOW+4_000).await.unwrap();
        assert_eq!(store.aggregate(NOW+4_000).await.unwrap().applied,0);
        assert_resource_accounting(&store).await;
    }
}

#[tokio::test]
async fn late_dispatch_observation_cannot_reverse_a_recorded_revocation() {
    let store=fixture_store(NOW-DAY).await;
    let alias=ObservationAlias::provider_work("revoked-work").unwrap();
    let original=bound_tool(alias.clone());
    store.persist(std::slice::from_ref(&original),NOW+100).await.unwrap();
    store.aggregate(NOW+100).await.unwrap();
    let revoked=ObservationEvent::deferred_tool(alias.clone(),ObservationPhase::Completed,0,NOW+3_000,
        BTreeMap::from([(Stage::Permission,StageOutcome::Failed)]),PermissionOutcome::Revoked,None,InputIssue::None);
    store.persist(&[revoked],NOW+3_001).await.unwrap(); store.aggregate(NOW+3_001).await.unwrap();
    let dispatch=ObservationEvent::deferred_tool(alias,ObservationPhase::Dispatched,0,NOW+2_000,
        BTreeMap::from([(Stage::Preflight,StageOutcome::Passed),(Stage::Permission,StageOutcome::Passed),(Stage::Dispatch,StageOutcome::Passed)]),
        PermissionOutcome::Approved,Some(InputConclusion::Accepted),InputIssue::None);
    store.persist(&[dispatch],NOW+3_002).await.unwrap();
    assert_eq!(store.aggregate(NOW+3_002).await.unwrap().discarded,0);
    let mut ordinary=original.clone(); ordinary.relation=None;
    ordinary.event_id="ordinary-progress-after-revocation".into(); ordinary.phase=ObservationPhase::Stage;
    ordinary.sequence=99; ordinary.occurred_at_ms=NOW+4_000;
    let ObservationPayload::Tool(snapshot)=&mut ordinary.payload else { panic!("tool required") };
    snapshot.permission=PermissionOutcome::Approved; snapshot.conclusion=InputConclusion::Accepted;
    snapshot.stages.insert(Stage::Permission,StageOutcome::Passed);
    snapshot.stages.insert(Stage::Preflight,StageOutcome::Passed);
    store.persist(&[ordinary],NOW+4_000).await.unwrap();
    assert_eq!(store.aggregate(NOW+4_001).await.unwrap().discarded,0);
    let row=detail::Entity::find_by_id(&original.object_id).one(&store.db).await.unwrap().unwrap();
    let event: ObservationEvent=decode(&row.snapshot_json).unwrap();
    let ObservationPayload::Tool(tool)=event.payload else { panic!("tool required") };
    assert_eq!(tool.permission,PermissionOutcome::Revoked);
    assert_eq!(tool.stages.get(&Stage::Permission),Some(&StageOutcome::Failed));
    assert_eq!(tool.conclusion,InputConclusion::Accepted);
    let summary=store.overview(&query(NOW-HOUR,NOW+HOUR),NOW+HOUR).await.unwrap().summary;
    assert_eq!(count(&summary,Count::PermissionRevoked),Some("1"));
    assert_eq!(count(&summary,Count::PermissionDenied).unwrap_or("0"),"0");
    assert_eq!(count(&summary,Count::InputRejected).unwrap_or("0"),"0");
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn confirmed_command_reuses_original_input_and_pre_worker_denial_is_not_an_input_error() {
    use desk_diagnose_core::model_observability::operations::{command_dispatch,command_completion};
    use desk_agent_protocol::{AgentErrorKind,edge_exec::EdgeExecDisposition};
    let store=fixture_store(NOW-DAY).await;
    let work=ObservationAlias::provider_work("command-provider-work").unwrap();
    let command=ObservationAlias::command_work("command-task-77").unwrap();
    let mut original=bound_tool(work.clone());
    let ObservationPayload::Tool(tool)=&mut original.payload else { panic!("tool required") };
    tool.tool_key=desk_diagnose_core::command_confirmation::COMMAND_TOOL.into();
    store.persist(&[original,ObservationEvent::link_alias(work,command.clone(),NOW+100)],NOW+100).await.unwrap();
    store.aggregate(NOW+100).await.unwrap();
    let mut events=Vec::new();
    command_dispatch(command.clone(),NOW+1_000,|event|events.push(event));
    let denial=EdgeExecDisposition::RejectedBeforeDispatch {
        error:EdgeExecDisposition::safe_error(AgentErrorKind::PermissionDenied,"private daemon policy",false),
    };
    command_completion(command,NOW+2_000,&denial,|event|events.push(event));
    let encoded=serde_json::to_string(&events).unwrap();
    assert!(!encoded.contains("private daemon policy"));
    store.persist(&events,NOW+2_001).await.unwrap();
    assert_eq!(store.aggregate(NOW+2_001).await.unwrap().discarded,0);
    let mut selected=query(NOW-HOUR,NOW+HOUR); selected.tool=Some("exec_command".into());
    let summary=store.overview(&selected,NOW+HOUR).await.unwrap().summary;
    assert_eq!(count(&summary,Count::Tools),Some("1"));
    assert_eq!(count(&summary,Count::InputAccepted),Some("1"));
    assert_eq!(count(&summary,Count::PermissionPolicyRejected),Some("1"));
    assert_eq!(count(&summary,Count::PermissionDenied).unwrap_or("0"),"0");
    assert_eq!(count(&summary,Count::InputRejected).unwrap_or("0"),"0");
    assert_eq!(count(&summary,Count::OperationsNotDispatched),Some("1"));
    assert_eq!(count(&summary,Count::OperationsDispatched).unwrap_or("0"),"0");
    let rate=summary.rates.iter().find(|rate|rate.key=="execution_verification").unwrap();
    assert_eq!(rate.denominator.as_deref(),Some("0")); assert!(rate.value.is_none());
    store.persist(&events,NOW+3_000).await.unwrap();
    assert_eq!(store.aggregate(NOW+3_000).await.unwrap().applied,0);
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn replayed_association_and_permission_are_idempotent_without_hiding_changed_sources() {
    let store=fixture_store(NOW-DAY).await;
    let source=ObservationAlias::source_message("server-message",0).unwrap();
    let work=ObservationAlias::provider_work("same-work").unwrap();
    store.persist(&[bound_tool(source.clone())],NOW+100).await.unwrap(); store.aggregate(NOW+100).await.unwrap();
    let approval=|at|ObservationEvent::deferred_tool(work.clone(),ObservationPhase::Stage,0,at,
        BTreeMap::from([(Stage::Permission,StageOutcome::Passed)]),PermissionOutcome::Approved,None,InputIssue::None);
    let first=[ObservationEvent::link_alias(source.clone(),work.clone(),NOW+200),approval(NOW+200)];
    store.persist(&first,NOW+200).await.unwrap(); store.aggregate(NOW+200).await.unwrap();
    let before=settings::Entity::find_by_id(1).one(&store.db).await.unwrap().unwrap();
    let replay=[ObservationEvent::link_alias(source,work.clone(),NOW+1_000),approval(NOW+1_000)];
    assert_eq!(store.persist(&replay,NOW+1_000).await.unwrap(),0);
    assert_eq!(store.aggregate(NOW+1_000).await.unwrap().applied,0);
    let after=settings::Entity::find_by_id(1).one(&store.db).await.unwrap().unwrap();
    assert_eq!(before.storage_used_bytes,after.storage_used_bytes);
    assert_eq!(before.event_rows,after.event_rows);
    assert!(!after.coverage_partial);
    let summary=store.overview(&query(NOW-HOUR,NOW+HOUR),NOW+HOUR).await.unwrap().summary;
    assert_eq!(count(&summary,Count::Tools),Some("1"));
    assert_eq!(count(&summary,Count::PermissionApproved),Some("1"));
    let changed=ObservationEvent::link_alias(ObservationAlias::source_message("different-message",0).unwrap(),work,NOW+2_000);
    assert_eq!(store.persist(&[changed],NOW+2_000).await.unwrap(),1);
    assert!(settings::Entity::find_by_id(1).one(&store.db).await.unwrap().unwrap().coverage_partial);
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn command_attempts_keep_one_input_and_do_not_turn_unsent_work_into_an_operation() {
    use desk_diagnose_core::model_observability::operations::{command_dispatch,command_completion,command_not_sent,command_executed,command_route_indeterminate};
    use desk_agent_protocol::{AgentOutcome,AgentErrorKind,OperationOutput,ExecOutput,ExecOutputStreams,edge_exec::EdgeExecDisposition};
    let store=fixture_store(NOW-DAY).await;
    let work=ObservationAlias::command_work("77").unwrap();
    let unsent=ObservationAlias::command_attempt("77",1).unwrap();
    let sent=ObservationAlias::command_attempt("77",2).unwrap();
    let later=ObservationAlias::command_attempt("77",3).unwrap();
    let mut events=vec![bound_tool(work.clone()),
        ObservationEvent::link_alias(work.clone(),unsent.clone(),NOW+100),
        ObservationEvent::link_alias(work.clone(),sent.clone(),NOW+100),
        ObservationEvent::link_alias(work,later.clone(),NOW+100)];
    let failure=EdgeExecDisposition::safe_error(AgentErrorKind::SessionUnavailable,"private moved connection",true);
    command_not_sent(unsent.clone(),NOW+200,&failure,|event|events.push(event));
    command_dispatch(sent.clone(),NOW+1_000,|event|events.push(event));
    command_completion(sent.clone(),NOW+2_000,&EdgeExecDisposition::ExecutionStateUnknown { reason:"private disconnect".into() },|event|events.push(event));
    let native=AgentOutcome::Ok(OperationOutput::Exec(ExecOutput {
        started:true,exit_code:Some(0),termination_signal:None,failure:None,diagnostics:vec![],
        streams:ExecOutputStreams::Split { stdout:"private stdout".into(),stderr:String::new(),stdout_truncated:false,stderr_truncated:false },
        duration_ms:25,redactions:vec![],
    }));
    command_executed(sent.clone(),NOW+3_000,&native,|event|events.push(event));
    command_route_indeterminate(sent.clone(),NOW+3_001,|event|events.push(event));
    command_dispatch(later.clone(),NOW+4_000,|event|events.push(event));
    command_executed(later.clone(),NOW+5_000,&native,|event|events.push(event));
    store.persist(&events,NOW+5_001).await.unwrap();
    let aggregate=store.aggregate(NOW+5_001).await.unwrap();
    assert_eq!(aggregate.discarded,0);
    let summary=store.overview(&query(NOW-HOUR,NOW+HOUR),NOW+HOUR).await.unwrap().summary;
    assert_eq!(count(&summary,Count::Tools),Some("1"));
    assert_eq!(count(&summary,Count::InputAccepted),Some("1"));
    assert_eq!(count(&summary,Count::InputRejected).unwrap_or("0"),"0");
    assert_eq!(count(&summary,Count::PermissionDenied).unwrap_or("0"),"0");
    assert_eq!(count(&summary,Count::OperationsDispatched),Some("2"));
    assert_eq!(count(&summary,Count::OperationsAccepted),Some("2"));
    assert_eq!(count(&summary,Count::OperationsUnknown).unwrap_or("0"),"0");
    assert!(detail::Entity::find_by_id(unsent.operation_id(0)).one(&store.db).await.unwrap().is_none());
    for (alias,start) in [(sent,NOW+1_000),(later,NOW+4_000)] {
        let row=detail::Entity::find_by_id(alias.operation_id(0)).one(&store.db).await.unwrap().unwrap();
        let event: ObservationEvent=decode(&row.snapshot_json).unwrap();
        assert_eq!(event.started_at_ms,start);
        assert_eq!(event.call_id.as_deref(),Some("original-call"));
        assert_eq!(event.attribution.model_id,"model-1");
    }
    let encoded=serde_json::to_string(&events).unwrap();
    for private in ["private stdout","private disconnect","private moved connection"] { assert!(!encoded.contains(private)); }
    store.persist(&events,NOW+6_000).await.unwrap();
    assert_eq!(store.aggregate(NOW+6_000).await.unwrap().applied,0);
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn approval_expiry_and_cancel_end_waiting_without_becoming_owner_or_input_denial() {
    for (permission,key) in [(PermissionOutcome::Expired,Count::PermissionExpired),(PermissionOutcome::Cancelled,Count::PermissionCancelled)] {
        let store=fixture_store(NOW-DAY).await;
        let work=ObservationAlias::command_work("77").unwrap();
        let mut original=bound_tool(work.clone());
        let ObservationPayload::Tool(tool)=&mut original.payload else { panic!("tool required") };
        tool.permission=PermissionOutcome::Waiting;
        store.persist(&[original],NOW+100).await.unwrap(); store.aggregate(NOW+100).await.unwrap();
        let mut ended=Vec::new();
        desk_diagnose_core::model_observability::permission::ended(work.clone(),permission,NOW+200,|event|ended.push(event));
        store.persist(&ended,NOW+200).await.unwrap(); store.aggregate(NOW+200).await.unwrap();
        let approval=ObservationEvent::deferred_tool(work,ObservationPhase::Dispatched,0,NOW+300,
            BTreeMap::from([(Stage::Permission,StageOutcome::Passed)]),PermissionOutcome::Approved,None,InputIssue::None);
        store.persist(&[approval],NOW+300).await.unwrap(); store.aggregate(NOW+300).await.unwrap();
        let summary=store.overview(&query(NOW-HOUR,NOW+HOUR),NOW+HOUR).await.unwrap().summary;
        assert_eq!(count(&summary,key),Some("1"));
        for count_key in [Count::PermissionWaiting,Count::PermissionApproved,Count::PermissionDenied,Count::InputRejected,Count::OperationsDispatched] {
            assert_eq!(count(&summary,count_key).unwrap_or("0"),"0");
        }
        assert_resource_accounting(&store).await;
    }
}

#[tokio::test]
async fn permission_summary_replaces_waiting_with_one_exact_original_input_outcome() {
    let cases=[
        (PermissionOutcome::Approved,Count::PermissionApproved),
        (PermissionOutcome::Narrowed,Count::PermissionNarrowed),
        (PermissionOutcome::Denied,Count::PermissionDenied),
        (PermissionOutcome::Revoked,Count::PermissionRevoked),
        (PermissionOutcome::PolicyRejected,Count::PermissionPolicyRejected),
        (PermissionOutcome::Unavailable,Count::PermissionUnavailable),
        (PermissionOutcome::Expired,Count::PermissionExpired),
        (PermissionOutcome::Cancelled,Count::PermissionCancelled),
    ];
    for (permission,expected) in cases {
        let store=fixture_store(NOW-DAY).await;
        let alias=ObservationAlias::provider_work("permission-summary").unwrap();
        let mut original=bound_tool(alias.clone());
        let ObservationPayload::Tool(tool)=&mut original.payload else { panic!("tool required") };
        tool.conclusion=InputConclusion::Accepted; tool.permission=PermissionOutcome::Waiting;
        store.persist(std::slice::from_ref(&original),NOW+100).await.unwrap(); store.aggregate(NOW+100).await.unwrap();
        let decided=ObservationEvent::deferred_tool(alias.clone(),ObservationPhase::Completed,0,NOW+200,
            BTreeMap::new(),permission,None,InputIssue::None);
        store.persist(std::slice::from_ref(&decided),NOW+200).await.unwrap(); store.aggregate(NOW+200).await.unwrap();
        if permission!=PermissionOutcome::Approved {
            let replay=ObservationEvent::deferred_tool(alias,ObservationPhase::Stage,1,NOW+300,
                BTreeMap::from([(Stage::Permission,StageOutcome::Passed)]),PermissionOutcome::Approved,None,InputIssue::None);
            store.persist(&[replay],NOW+300).await.unwrap(); store.aggregate(NOW+300).await.unwrap();
        }
        let summary=store.overview(&query(NOW-HOUR,NOW+HOUR),NOW+HOUR).await.unwrap().summary;
        for (_,key) in cases { assert_eq!(count(&summary,key).unwrap_or("0"),if key==expected { "1" } else { "0" }); }
        assert_eq!(count(&summary,Count::PermissionWaiting).unwrap_or("0"),"0");
        assert_eq!(count(&summary,Count::Tools),Some("1"));
        assert_eq!(count(&summary,Count::InputAccepted),Some("1"));
        for key in [Count::InputRejected,Count::OperationsDispatched,Count::RequestErrors] {
            assert_eq!(count(&summary,key).unwrap_or("0"),"0");
        }
        let record=detail::Entity::find_by_id(&original.object_id).one(&store.db).await.unwrap().unwrap();
        let restored: ObservationEvent=decode(&record.snapshot_json).unwrap();
        assert_eq!((restored.attribution,restored.started_at_ms,restored.call_id),
            (original.attribution,original.started_at_ms,original.call_id));
        store.persist(&[decided],NOW+400).await.unwrap();
        assert_eq!(store.aggregate(NOW+400).await.unwrap().applied,0);
        assert_resource_accounting(&store).await;
    }
}

#[tokio::test]
async fn permission_decision_uses_creator_attribution_after_writer_handoff_and_request_reuse() {
    use desk_diagnose_core::{dynamic_run::{AgentRunEvent,AgentRunEventKind,PermissionDecidedEvent,
        PermissionDecisionItem,PermissionDecisionSource,PermissionItemDecision,PermissionRequestState,AGENT_RUN_EVENT_SCHEMA_VERSION},
        model_observability::permission};
    for (source,expected,late_binding) in [
        (PermissionDecisionSource::UserDecision,PermissionOutcome::Denied,false),
        (PermissionDecisionSource::AiApproval { delegation_id:"delegation-1".into(),delegation_revision:1,reviews:vec![
            desk_diagnose_core::dynamic_run::AiPermissionDecisionEvidence {
                item_id:"private-provider-tool-id".into(),candidate_id:"candidate-1".into(),candidate_expires_at_unix_ms:(NOW+10_000) as u64,
                reason_code:"risk_denied".into(),reason:"private reviewer reason".into(),
            },
        ] },PermissionOutcome::PolicyRejected,true),
        (PermissionDecisionSource::ReviewUnavailable { reason_code:"approval_ai_fault".into(),reason:"private reviewer fault".into() },PermissionOutcome::Unavailable,true),
    ] {
        let store=fixture_store(NOW-DAY).await;
        let alias=ObservationAlias::permission_request("run-1","request-1").unwrap();
        let mut original=bound_tool(alias);
        let ObservationPayload::Tool(tool)=&mut original.payload else { panic!("tool required") };
        tool.tool_key="request_permissions".into(); tool.conclusion=InputConclusion::Accepted;
        tool.permission=PermissionOutcome::Waiting;
        let mut reused=original.clone(); reused.relation=None;
        reused.object_id="later-call.tool.0".into(); reused.event_id="later-call.tool.0.stage".into(); reused.call_id=Some("later-call".into());
        reused.attribution.model_id="model-2".into(); reused.attribution.origin=Origin::PermissionResume;
        let decided=PermissionDecidedEvent {
            event:AgentRunEvent { schema_version:AGENT_RUN_EVENT_SCHEMA_VERSION,event_id:"decision-1".into(),run_id:"run-1".into(),
                event_seq:2,input_revision:1,kind:AgentRunEventKind::PermissionDecided,correlation_id:Some("request-1".into()),
                source_envelope_ids:vec![],result_envelope_ids:vec![],created_at:timestamp(NOW+200) },
            request_id:"request-1".into(),request_input_revision:1,resulting_state:PermissionRequestState::Denied,
            items:vec![PermissionDecisionItem { item_id:"private-provider-tool-id".into(),decision:PermissionItemDecision::Deny }],
            decision_source:source,
        };
        let mut observations=Vec::new();
        decided.validate().unwrap();
        permission::decided(&decided,NOW+200,|event|observations.push(event));
        assert_eq!(observations.len(),1);
        if late_binding {
            store.persist(&observations,NOW+200).await.unwrap();
            assert_eq!(store.aggregate(NOW+200).await.unwrap().applied,0);
            assert_eq!(detail::Entity::find().filter(detail::Column::Kind.eq("tool")).count(&store.db).await.unwrap(),0);
            assert_eq!(detail::Entity::find().filter(detail::Column::Kind.eq("unassociated")).count(&store.db).await.unwrap(),1);
        }
        store.persist(&[original.clone(),reused.clone()],NOW+300).await.unwrap();
        store.aggregate(NOW+300).await.unwrap();
        let other=Store::new(store.db.clone(),store.manager,"permission-writer-2".into());
        other.persist(&observations,NOW+400).await.unwrap(); other.aggregate(NOW+10_401).await.unwrap();
        let parent=detail::Entity::find_by_id(&original.object_id).one(&other.db).await.unwrap().unwrap();
        let child=detail::Entity::find_by_id(&reused.object_id).one(&other.db).await.unwrap().unwrap();
        let parent: ObservationEvent=decode(&parent.snapshot_json).unwrap();
        let child: ObservationEvent=decode(&child.snapshot_json).unwrap();
        assert_eq!((&parent.attribution,parent.started_at_ms,&parent.call_id),(&original.attribution,original.started_at_ms,&original.call_id));
        let ObservationPayload::Tool(parent)=parent.payload else { panic!("tool required") };
        let ObservationPayload::Tool(child)=child.payload else { panic!("tool required") };
        assert_eq!(parent.permission,expected); assert_eq!(child.permission,PermissionOutcome::Waiting);
        let key=match expected {
            PermissionOutcome::Denied=>Count::PermissionDenied,
            PermissionOutcome::PolicyRejected=>Count::PermissionPolicyRejected,
            PermissionOutcome::Unavailable=>Count::PermissionUnavailable,
            _=>unreachable!(),
        };
        let mut selected=query(NOW-HOUR,NOW+HOUR); selected.model_id=Some("model-1".into());
        let summary=other.overview(&selected,NOW+HOUR).await.unwrap().summary;
        assert_eq!(count(&summary,key),Some("1")); assert_eq!(count(&summary,Count::Tools),Some("1"));
        assert_eq!(count(&summary,Count::InputRejected).unwrap_or("0"),"0");
        assert_eq!(count(&summary,Count::PermissionWaiting).unwrap_or("0"),"0");
        let encoded=serde_json::to_string(&observations).unwrap();
        for private in ["private-provider-tool-id","private reviewer fault","private reviewer reason","delegation-1","candidate-1"] { assert!(!encoded.contains(private)); }
        assert_resource_accounting(&other).await;
    }
}

#[tokio::test]
async fn goal_and_directory_receipts_resolve_production_sized_aliases_after_late_binding_and_writer_handoff() {
    use desk_diagnose_core::{goal::{GoalLimits,GoalModelBinding,GoalOpenRequest,GoalOpenRequestState},
        model_observability::permission,file_scope::{DirectoryConsentSource,DirectoryProposal,transaction::{self,FileScopeMutation,FileScopeUpdate}}};
    let run=desk_diagnose_core::conversation_key::derive_conversation_key("owner","device",Some("browser-run"),"fallback");
    for case in ["goal_approved","goal_denied","goal_superseded","directory_approved","directory_denied","directory_revoked"] {
        let store=fixture_store(NOW-DAY).await;
        let is_goal=case.starts_with("goal_");
        let request_id=format!("{}-{}",if is_goal { "goal-open-request" } else { "directory-proposal" },"a".repeat(64));
        let alias=if is_goal { ObservationAlias::goal_open_request(&run,&request_id) }
            else { ObservationAlias::directory_request(&run,&request_id) }.unwrap();
        let mut received=Vec::new();
        let (expected,key)=if is_goal {
            let binding=GoalModelBinding { connection_id:"connection".into(),connection_revision:1,profile_revision:1,model_id:"model-1".into() };
            let mut request=GoalOpenRequest::new(request_id.clone(),run.clone(),"owner".into(),"device".into(),
                "private-source-message".into(),1,"private-goal-text".into(),GoalLimits::default(),binding.clone(),NOW as u64).unwrap();
            let (outcome,key)=match case {
                "goal_approved"=>{
                    request.approve(1,&binding,"created-goal".into(),"server-decision".into(),(NOW+100) as u64,None).unwrap();
                    (PermissionOutcome::Approved,Count::PermissionApproved)
                },
                "goal_denied"=>{
                    request.close(GoalOpenRequestState::Denied,"server-decision".into(),(NOW+100) as u64).unwrap();
                    (PermissionOutcome::Denied,Count::PermissionDenied)
                },
                _=>{
                    request.close(GoalOpenRequestState::Withdrawn,"server-decision".into(),(NOW+100) as u64).unwrap();
                    (PermissionOutcome::Revoked,Count::PermissionRevoked)
                },
            };
            permission::goal_open_closed(&request,NOW+200,|event|received.push(event));
            (outcome,key)
        } else {
            let mut session=desk_diagnose_core::session::PersistedAgentSession::new(&run,"owner","device",1,
                desk_agent_protocol::AgentScope { granted:vec![],mode:desk_agent_protocol::ExecutionMode::ReadOnly,expires_at:None,policy_name:None },
                timestamp(NOW));
            session.adopt_client_metadata(Some("browser-run"),desk_diagnose_core::session::AgentSessionSurface::AiAssistant);
            let subject=session.file_scope_subject("owner","device",&run).unwrap();
            let proposal=DirectoryProposal { request_id:request_id.clone(),requested_path:"/private/path".into(),
                canonical_path:"/private/canonical".into(),purpose:"private directory purpose".into(),source:DirectoryConsentSource::ModelProposal,
                directory:desk_agent_protocol::computer_use::ObjectRef { token:"private-device-reference".into(),snapshot_id:"private-generation".into(),
                    object_kind:desk_agent_protocol::computer_use::ObjectKind::Directory,expires_at:timestamp(NOW+HOUR) },
            };
            session.file_scope.propose(&subject,0,proposal,NOW as u64).unwrap();
            let (mutation,outcome,key)=match case {
                "directory_approved"=>(FileScopeMutation::Decide { directory_request_id:request_id.clone(),approve:true },PermissionOutcome::Approved,Count::PermissionApproved),
                "directory_denied"=>(FileScopeMutation::Decide { directory_request_id:request_id.clone(),approve:false },PermissionOutcome::Denied,Count::PermissionDenied),
                _=>(FileScopeMutation::Revoke { directory_request_id:request_id.clone() },PermissionOutcome::Revoked,Count::PermissionRevoked),
            };
            let update=FileScopeUpdate { subject,client_conversation_id:"browser-run".into(),client_request_id:"server-decision".into(),
                expected_revision:session.file_scope.revision(),mutation };
            let pending=permission::PendingDirectoryUpdate::capture(&session,&update);
            let (_,receipt)=transaction::prepare(&session,&update,(NOW+100) as u64).unwrap();
            pending.submit(&receipt,NOW+200,|event|received.push(event));
            (outcome,key)
        };
        assert_eq!(received.len(),1);
        store.persist(&received,NOW+200).await.unwrap();
        assert_eq!(store.aggregate(NOW+200).await.unwrap().applied,0);
        assert_eq!(detail::Entity::find().filter(detail::Column::Kind.eq("tool")).count(&store.db).await.unwrap(),0);
        assert_eq!(detail::Entity::find().filter(detail::Column::Kind.eq("unassociated")).count(&store.db).await.unwrap(),1);
        let mut original=bound_tool(alias);
        let ObservationPayload::Tool(tool)=&mut original.payload else { panic!("original input required") };
        tool.tool_key=if is_goal { "request_goal" } else { "request_directory" }.into();
        tool.conclusion=InputConclusion::Accepted; tool.permission=PermissionOutcome::Waiting;
        let other=Store::new(store.db.clone(),store.manager,"approval-writer-2".into());
        other.persist(std::slice::from_ref(&original),NOW+300).await.unwrap();
        assert_eq!(other.aggregate(NOW+10_401).await.unwrap().discarded,0);
        let record=detail::Entity::find_by_id(&original.object_id).one(&other.db).await.unwrap().unwrap();
        let restored: ObservationEvent=decode(&record.snapshot_json).unwrap();
        assert_eq!((&restored.attribution,restored.started_at_ms,&restored.call_id),(&original.attribution,original.started_at_ms,&original.call_id));
        let ObservationPayload::Tool(tool)=&restored.payload else { panic!("tool required") };
        assert_eq!(tool.permission,expected); assert_eq!(tool.conclusion,InputConclusion::Accepted);
        let mut late_waiting=original.clone(); late_waiting.relation=None;
        late_waiting.event_id="original-tool.late-waiting".into(); late_waiting.sequence=100;
        other.persist(&[late_waiting],NOW+10_500).await.unwrap(); other.aggregate(NOW+10_500).await.unwrap();
        let summary=other.overview(&query(NOW-HOUR,NOW+HOUR),NOW+HOUR).await.unwrap().summary;
        assert_eq!(count(&summary,Count::Tools),Some("1"));
        assert_eq!(count(&summary,Count::InputAccepted),Some("1")); assert_eq!(count(&summary,key),Some("1"));
        for excluded in [Count::PermissionWaiting,Count::InputRejected,Count::OperationsDispatched,Count::RequestErrors] {
            assert_eq!(count(&summary,excluded).unwrap_or("0"),"0");
        }
        other.persist(&received,NOW+10_600).await.unwrap();
        assert_eq!(other.aggregate(NOW+10_600).await.unwrap().applied,0);
        let encoded=serde_json::to_string(&received).unwrap();
        for secret in [&*run,&*request_id,"private-goal-text","private-source-message","/private/path","private directory purpose","private-device-reference","private-generation"] {
            assert!(!encoded.contains(secret));
        }
        assert_resource_accounting(&other).await;
    }
}

#[tokio::test]
async fn mismatched_operation_ordinal_is_isolated_without_relabeling_the_original_input() {
    let store=fixture_store(NOW-DAY).await;
    let alias=ObservationAlias::provider_work("malformed-operation").unwrap();
    store.persist(&[bound_tool(alias.clone())],NOW+100).await.unwrap(); store.aggregate(NOW+100).await.unwrap();
    let mut operation=deferred_operation(alias,OperationOutcome::Verified);
    let ObservationPayload::Operation(snapshot)=&mut operation.payload else { panic!("operation required") };
    snapshot.ordinal=1;
    store.persist(&[operation.clone(),call_event("independent-call",NOW,RequestOutcome::Returned)],NOW+2_000).await.unwrap();
    let result=store.aggregate(NOW+2_001).await.unwrap();
    assert_eq!((result.applied,result.discarded),(1,1));
    let row=event::Entity::find().filter(event::Column::EventId.eq(&operation.event_id)).one(&store.db).await.unwrap().unwrap();
    assert_eq!(row.discarded_reason.as_deref(),Some("association_conflict"));
    assert!(detail::Entity::find_by_id(&operation.object_id).one(&store.db).await.unwrap().is_none());
    assert!(detail::Entity::find_by_id("independent-call").one(&store.db).await.unwrap().is_some());
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn resumed_work_links_through_collected_source_and_keeps_original_model_and_tool() {
    let first=fixture_store(NOW-DAY).await;
    let source=ObservationAlias::source_message("server-message",0).unwrap();
    let work=ObservationAlias::provider_work("work-1").unwrap();
    first.persist(&[bound_tool(source.clone())],NOW+100).await.unwrap();
    first.aggregate(NOW+100).await.unwrap();
    let second=Store::new(first.db.clone(),first.manager,"other-node".into());
    let completion=deferred_operation(work.clone(),OperationOutcome::Verified);
    // Completion can precede the alias link; the writer never asks the resumed
    // business task for current model credentials or statistics storage.
    second.persist(&[completion.clone(),ObservationEvent::link_alias(source,work,NOW+2_000)],NOW+2_001).await.unwrap();
    assert_eq!(second.aggregate(NOW+10_101).await.unwrap().discarded,0);
    assert_resource_accounting(&second).await;
    let row=detail::Entity::find_by_id(&completion.object_id).one(&second.db).await.unwrap().unwrap();
    let observed: ObservationEvent=decode(&row.snapshot_json).unwrap();
    assert_eq!(observed.attribution.model_id,"model-1");
    assert_eq!(observed.call_id.as_deref(),Some("original-call"));
    let projected=record(&observed,false);
    assert_eq!(projected.tool_observation_id.as_deref(),Some("original-call.tool.0"));
    assert_eq!(projected.tool.as_deref(),Some("execute_confirmed_ui_action"));
    assert_eq!(projected.ordinal,Some(0));
    let mut selected=query(NOW-HOUR,NOW+HOUR); selected.tool=Some("execute_confirmed_ui_action".into());
    let summary=second.overview(&selected,NOW+HOUR).await.unwrap().summary;
    assert_eq!(count(&summary,Count::Tools),Some("1"));
    assert_eq!(count(&summary,Count::OperationsDispatched),Some("1"));
    assert_eq!(count(&summary,Count::OperationsVerified),Some("1"));
    let rate=summary.rates.iter().find(|rate|rate.key=="execution_verification").unwrap();
    assert_eq!((rate.numerator.as_deref(),rate.denominator.as_deref()),(Some("1"),Some("1")));
    second.persist(&[completion],NOW+10_200).await.unwrap();
    assert_eq!(second.aggregate(NOW+10_200).await.unwrap().applied,0);
    assert_resource_accounting(&second).await;
}

#[tokio::test]
async fn missing_alias_stays_pending_then_reports_gap_without_fabricated_call_or_model() {
    let store=fixture_store(NOW-DAY).await;
    let operation=deferred_operation(ObservationAlias::provider_work("missing-work").unwrap(),OperationOutcome::Verified);
    store.persist(&[operation],NOW+2_001).await.unwrap();
    assert_eq!(store.aggregate(NOW+2_002).await.unwrap().discarded,0);
    assert_eq!(detail::Entity::find().filter(detail::Column::Kind.ne("unassociated")).count(&store.db).await.unwrap(),0);
    assert_eq!(store.aggregate(NOW+32_002).await.unwrap().discarded,0);
    let row=event::Entity::find().one(&store.db).await.unwrap().unwrap();
    assert!(row.applied && row.association_pending && row.discarded_reason.is_none());
    let gaps=store.unassociated(&unassociated_query(NOW,NOW+HOUR,50),NOW+32_002).await.unwrap();
    assert_eq!(gaps.records.len(),1); assert_eq!(gaps.records[0].state,AssociationGapState::Unavailable);
    assert!(gaps.records[0].original_model.is_none() && gaps.records[0].started_at.is_none());
    assert_eq!(compact::Entity::find().count(&store.db).await.unwrap(),0);
    let overview=store.overview(&query(NOW-HOUR,NOW+HOUR),NOW+HOUR).await.unwrap();
    assert_eq!(overview.coverage.sample_status,SampleStatus::Partial);
    assert_eq!(count(&overview.summary,Count::OperationsVerified),None);
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn another_model_cannot_rebind_an_existing_work_to_a_new_input() {
    let store=fixture_store(NOW-DAY).await;
    let alias=ObservationAlias::provider_work("work-1").unwrap();
    store.persist(&[bound_tool(alias.clone())],NOW+100).await.unwrap();
    store.aggregate(NOW+100).await.unwrap();
    let mut attempted=bound_tool(alias.clone());
    attempted.object_id="new-call.tool.0".into(); attempted.call_id=Some("new-call".into());
    attempted.event_id="new-tool.binding".into(); attempted.attribution.model_id="model-2".into();
    store.persist(&[attempted,deferred_operation(alias,OperationOutcome::Verified)],NOW+2_001).await.unwrap();
    store.aggregate(NOW+2_001).await.unwrap();
    let rows=detail::Entity::find().filter(detail::Column::Kind.eq("operation")).all(&store.db).await.unwrap();
    assert_eq!(rows.len(),1); assert_eq!(rows[0].model_id,"model-1");
    assert_eq!(rows[0].call_id.as_deref(),Some("original-call"));
    assert!(settings::Entity::find_by_id(1).one(&store.db).await.unwrap().unwrap().coverage_partial);
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn pending_alias_links_cannot_starve_the_later_source_binding() {
    let store=fixture_store(NOW-DAY).await;
    let source=ObservationAlias::source_message("original-message",0).unwrap();
    let links: Vec<_>=(0..100).map(|index|ObservationEvent::link_alias(source.clone(),
        ObservationAlias::provider_work(&format!("work-{index}")).unwrap(),NOW+100)).collect();
    store.persist(&links,NOW+100).await.unwrap();
    assert_eq!(store.aggregate(NOW+100).await.unwrap().applied,0);
    store.persist(&[bound_tool(source)],NOW+200).await.unwrap();
    let first=store.aggregate(NOW+200).await.unwrap();
    assert_eq!((first.applied,first.discarded),(21,0));
    for time in [NOW+300,NOW+400,NOW+500,NOW+600] {
        let recovered=store.aggregate(time).await.unwrap();
        assert_eq!((recovered.applied,recovered.discarded),(20,0));
    }
    assert_eq!(store.aggregate(NOW+700).await.unwrap().applied,0);
    assert_eq!(detail::Entity::find().filter(detail::Column::Kind.eq("unassociated")).count(&store.db).await.unwrap(),0);
    assert_eq!(compact::Entity::find().filter(compact::Column::Kind.eq("binding")).count(&store.db).await.unwrap(),101);
    let overview=store.overview(&query(NOW-HOUR,NOW+HOUR),NOW+HOUR).await.unwrap();
    assert_eq!(count(&overview.summary,Count::Tools),Some("1"));
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn damaged_source_binding_discards_only_its_deferred_fact_and_keeps_other_calls() {
    let store=fixture_store(NOW-DAY).await;
    let alias=ObservationAlias::provider_work("bad-work").unwrap();
    store.persist(&[bound_tool(alias.clone())],NOW+100).await.unwrap();
    store.aggregate(NOW+100).await.unwrap();
    compact::Entity::update_many().col_expr(compact::Column::SnapshotJson,Expr::value("{}"))
        .filter(compact::Column::ObjectId.eq(alias.object_id())).exec(&store.db).await.unwrap();
    store.persist(&[deferred_operation(alias,OperationOutcome::Verified),call_event("unrelated",NOW,RequestOutcome::Returned)],NOW+2_001).await.unwrap();
    let result=store.aggregate(NOW+2_001).await.unwrap();
    assert_eq!((result.applied,result.discarded),(1,1));
    let overview=store.overview(&query(NOW-HOUR,NOW+HOUR),NOW+HOUR).await.unwrap();
    assert_eq!(count(&overview.summary,Count::Returned),Some("1"));
    assert_eq!(count(&overview.summary,Count::OperationsVerified),None);
    assert!(overview.coverage.sample_status==SampleStatus::Partial);
    assert!(decode::<ObservationEvent>(&"x".repeat(MAX_PROJECTION_JSON_BYTES+1)).is_err());
}

fn count(summary: &MetricsSummary, key: Count) -> Option<&str> {
    let key=tag(&key);
    summary.counts.iter().find(|value| value.key==key).map(|value| value.count.as_str())
}

#[tokio::test]
async fn phase_slot_deduplicates_different_event_ids_without_replacing_the_fact() {
    let store=fixture_store(NOW-DAY).await;
    let first=call_event("call-1",NOW,RequestOutcome::Returned);
    let mut replay=first.clone();
    replay.event_id="another-server-event".into();
    assert_eq!(store.persist(&[first.clone(),replay],NOW+500).await.unwrap(),0);
    assert_eq!(event::Entity::find().count(&store.db).await.unwrap(),1);
    assert_eq!(store.aggregate(NOW+500).await.unwrap().applied,1);
    let result=store.overview(&query(NOW-HOUR,NOW+HOUR),NOW+HOUR).await.unwrap();
    assert_eq!(count(&result.summary,Count::Calls),Some("1"));
    let mut conflict=first;
    conflict.event_id="conflicting-server-event".into();
    conflict.payload=ObservationPayload::Call(CallSnapshot { outcome:RequestOutcome::Timeout,..Default::default() });
    assert_eq!(store.persist(&[conflict],NOW+700).await.unwrap(),1);
    let result=store.overview(&query(NOW-HOUR,NOW+HOUR),NOW+HOUR).await.unwrap();
    assert_eq!(count(&result.summary,Count::Returned),Some("1"));
    assert_eq!(result.coverage.sample_status,SampleStatus::Partial);
}

#[tokio::test]
async fn late_start_cannot_count_a_returned_call_again_or_undo_its_terminal_state() {
    let store=fixture_store(NOW-DAY).await;
    let terminal=call_event("call-1",NOW,RequestOutcome::Returned);
    let mut start=terminal.clone();
    start.event_id="call-1.start".into(); start.phase=ObservationPhase::Started;
    start.occurred_at_ms=NOW; start.payload=ObservationPayload::Call(CallSnapshot { outcome:RequestOutcome::Pending,..Default::default() });
    store.persist(&[terminal,start],NOW+500).await.unwrap();
    assert_eq!(store.aggregate(NOW+500).await.unwrap().applied,2);
    let result=store.overview(&query(NOW-HOUR,NOW+HOUR),NOW+HOUR).await.unwrap();
    assert_eq!(count(&result.summary,Count::Calls),Some("1"));
    assert_eq!(count(&result.summary,Count::Returned),Some("1"));
    assert_eq!(count(&result.summary,Count::Pending),None);
}

#[tokio::test]
async fn before_capture_events_do_not_create_samples_or_zero_error_rates() {
    let store=fixture_store(NOW).await;
    store.persist(&[call_event("before-capture",NOW-100,RequestOutcome::HttpError)],NOW+500).await.unwrap();
    assert_eq!(store.aggregate(NOW+500).await.unwrap().discarded,1);
    assert_eq!(compact::Entity::find().count(&store.db).await.unwrap(),0);
    let result=store.overview(&query(NOW-HOUR,NOW-1),NOW+HOUR).await.unwrap();
    assert_eq!(result.coverage.sample_status,SampleStatus::NotCollected);
    for rate in result.summary.rates {
        assert_eq!(rate.sample_status,SampleStatus::NotCollected);
        assert_eq!((rate.numerator,rate.denominator,rate.value),(None,None,None));
    }
}

#[tokio::test]
async fn output_and_input_detail_filters_use_different_object_domains() {
    let store=fixture_store(NOW-DAY).await;
    let mut output=call_event("call-1",NOW,RequestOutcome::Returned);
    if let ObservationPayload::Call(call)=&mut output.payload { call.output=OutputOutcome::InvalidProtocol; }
    let mut input=output.clone();
    input.event_id="tool-1.emitted".into(); input.object_id="tool-1".into(); input.call_id=Some(output.object_id.clone());
    input.phase=ObservationPhase::Emitted;
    input.payload=ObservationPayload::Tool(ToolSnapshot {
        ordinal:0,tool_key:"read_file".into(),stages:BTreeMap::from([(Stage::Protocol,StageOutcome::Failed)]),
        conclusion:InputConclusion::Rejected,issue:InputIssue::InvalidProtocol,schema_path:None,
        permission:PermissionOutcome::NotReached,correction_of:None,correction_status:CorrectionStatus::Uncorrelated,correction_input:None,
        argument_bytes:0,stage_duration_ms:None,
    });
    store.persist(&[output,input],NOW+500).await.unwrap();
    store.aggregate(NOW+500).await.unwrap();
    for (error,kind,id) in [(ErrorSelector::Output(OutputOutcome::InvalidProtocol),"call","call-1"),
        (ErrorSelector::Input(InputIssue::InvalidProtocol),"tool","tool-1")] {
        let mut query=query(NOW-HOUR,NOW+HOUR); query.error=Some(error);
        let result=store.calls(&query,NOW+HOUR).await.unwrap();
        assert_eq!(result.records.len(),1);
        assert_eq!((result.records[0].kind.as_str(),result.records[0].id.as_str()),(kind,id));
    }
}

#[tokio::test]
async fn shared_store_handles_continue_the_same_object_after_another_writer_lease_expires() {
    let first=fixture_store(NOW-DAY).await;
    let mut pending=call_event("call-1",NOW,RequestOutcome::Pending);
    pending.event_id="call-1.started".into(); pending.phase=ObservationPhase::Started;
    first.persist(&[pending],NOW+100).await.unwrap();
    first.aggregate(NOW+100).await.unwrap();
    let second=Store::new(first.db.clone(),first.manager,"another-node".into());
    second.persist(&[call_event("call-1",NOW,RequestOutcome::Returned)],NOW+500).await.unwrap();
    assert_eq!(second.aggregate(NOW+500).await.unwrap().applied,0);
    assert_eq!(second.aggregate(NOW+10_101).await.unwrap().applied,1);
    let result=second.overview(&query(NOW-HOUR,NOW+HOUR),NOW+HOUR).await.unwrap();
    assert_eq!(count(&result.summary,Count::Calls),Some("1"));
    assert_eq!(count(&result.summary,Count::Pending),Some("0"));
    assert_eq!(count(&result.summary,Count::Returned),Some("1"));
}

#[tokio::test]
async fn metered_axis_is_independent_and_cannot_duplicate_request_error_numerators() {
    let store=fixture_store(NOW-DAY).await;
    let mut event=call_event("call-1",NOW-2*HOUR,RequestOutcome::Timeout);
    if let ObservationPayload::Call(call)=&mut event.payload {
        call.metered_at_ms=Some(NOW);
        call.usage.normalized.input_tokens=Some(9_007_199_254_740_993);
        call.usage.normalized.output_tokens=Some(2);
    }
    store.persist(&[event],NOW+500).await.unwrap();
    store.aggregate(NOW+500).await.unwrap();
    let result=store.overview(&query(NOW-3*HOUR,NOW+HOUR),NOW+HOUR).await.unwrap();
    assert_eq!(count(&result.summary,Count::Calls),Some("1"));
    let failure=result.summary.rates.iter().find(|rate|rate.key=="request_failure").unwrap();
    assert_eq!((failure.numerator.as_deref(),failure.denominator.as_deref()),(Some("1"),Some("1")));
    let metered=store.overview(&query(NOW,NOW+HOUR),NOW+HOUR).await.unwrap();
    if store.manager {
        assert_eq!(count(&metered.summary,Count::MeteredCalls),None);
        assert_eq!(count(&metered.summary,Count::InputTokens),None);
    } else {
        assert_eq!(count(&metered.summary,Count::MeteredCalls),Some("1"));
        assert_eq!(count(&metered.summary,Count::InputTokens),Some("9007199254740993"));
    }
    assert_eq!(count(&metered.summary,Count::Calls),None);
}

#[tokio::test]
async fn corrupt_health_counters_are_unknown_instead_of_zero_dropped_events() {
    let store=fixture_store(NOW-DAY).await;
    health::ActiveModel {
        node_id:Set("first-node".into()),reported_at_ms:Set(NOW),state:Set("ready".into()),
        persisted_at_ms:Set(Some(NOW)),aggregated_at_ms:Set(Some(NOW)),
        dropped_events:Set("invalid".into()),discarded_events:Set("0".into()),
        config_revision:Set(Some("1".into())),reason:Set(None),
    }.insert(&store.db).await.unwrap();
    let status=store.status(NOW).await.unwrap();
    assert_eq!(status.state,ComponentState::CacheExpired);
    assert_eq!(status.dropped_events,None);
    assert_eq!(status.discarded_events,None);
    assert!(!status.settings_effective);
}

#[tokio::test]
async fn duplicate_and_expired_facts_release_exact_resource_reservations() {
    let store=fixture_store(NOW-DAY).await;
    let event=call_event("call-1",NOW,RequestOutcome::Returned);
    store.persist(std::slice::from_ref(&event),NOW+500).await.unwrap();
    store.aggregate(NOW+500).await.unwrap();
    assert_resource_accounting(&store).await;
    let before=settings::Entity::find_by_id(1).one(&store.db).await.unwrap().unwrap();
    store.persist(&[event],NOW+1_000).await.unwrap();
    let after=settings::Entity::find_by_id(1).one(&store.db).await.unwrap().unwrap();
    assert_eq!((before.event_rows,before.storage_used_bytes),(after.event_rows,after.storage_used_bytes));
    store.cleanup(NOW+100*DAY).await.unwrap();
    assert_resource_accounting(&store).await;
    let resources=settings::Entity::find_by_id(1).one(&store.db).await.unwrap().unwrap();
    assert_eq!((resources.event_rows,resources.compact_rows,resources.detail_rows,resources.rollup_rows,resources.storage_used_bytes),(0,0,0,0,0));
}

#[tokio::test]
async fn full_event_capacity_rejects_growth_but_accepts_replays_and_cleans_to_low_water() {
    let store=fixture_store(NOW-DAY).await;
    minimum_row_budgets(&store).await;
    for batch in 0..10 {
        let events: Vec<_>=(0..100).map(|index| call_event(&format!("queued-{}",batch*100+index),NOW,RequestOutcome::Returned)).collect();
        assert_eq!(store.persist(&events,NOW+500).await.unwrap(),0);
    }
    assert_eq!(store.persist(&[call_event("queued-0",NOW,RequestOutcome::Returned)],NOW+500).await.unwrap(),0);
    assert_eq!(store.persist(&[call_event("no-capacity",NOW,RequestOutcome::Returned)],NOW+500).await.unwrap(),1);
    assert_resource_accounting(&store).await;
    store.cleanup(NOW+31_001).await.unwrap();
    let row=settings::Entity::find_by_id(1).one(&store.db).await.unwrap().unwrap();
    assert_eq!((row.event_rows,row.dropped_pending_events),(800,200));
    assert!(!row.event_cleanup_active && row.coverage_partial);
    assert_resource_accounting(&store).await;
    assert_eq!(store.persist(&[call_event("after-release",NOW+31_002,RequestOutcome::Returned)],NOW+31_003).await.unwrap(),0);
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn compact_capacity_freezes_both_precisions_before_removing_deduplication() {
    let store=fixture_store(NOW-DAY).await;
    minimum_row_budgets(&store).await;
    let mut config=store.load_settings().await.unwrap(); config.event_row_budget=2_000;
    store.save_settings(config,NOW).await.unwrap().unwrap();
    thousand_calls(&store).await;
    assert_eq!(store.persist(&[call_event("blocked",NOW,RequestOutcome::Returned)],NOW+600).await.unwrap(),0);
    assert_eq!(store.aggregate(NOW+600).await.unwrap().discarded,1);
    assert_resource_accounting(&store).await;
    store.cleanup(NOW+1_000).await.unwrap();
    let config=settings::Entity::find_by_id(1).one(&store.db).await.unwrap().unwrap();
    assert_eq!(config.compact_rows,0);
    assert!(config.frozen_before_ms>=NOW+HOUR);
    let rows=rollup::Entity::find().all(&store.db).await.unwrap();
    assert!(rows.iter().all(|row| row.frozen));
    assert!(rows.iter().any(|row| row.granularity_ms==300_000));
    assert!(rows.iter().any(|row| row.granularity_ms==HOUR));
    let mut replay=call_event("call-0",NOW,RequestOutcome::Returned);
    replay.event_id="call-0.late".into(); replay.sequence=1;
    store.persist(&[replay],NOW+2_000).await.unwrap();
    assert_eq!(store.aggregate(NOW+2_000).await.unwrap().outside_window,1);
    assert_eq!(compact::Entity::find().count(&store.db).await.unwrap(),0);
    let result=store.overview(&query(NOW,NOW+HOUR),NOW+HOUR).await.unwrap();
    assert_eq!(count(&result.summary,Count::Calls),Some("1000"));
    assert_resource_accounting(&store).await;
    let next=call_event("next-cohort",NOW+HOUR,RequestOutcome::Returned);
    store.persist(&[next],NOW+HOUR+500).await.unwrap();
    assert_eq!(store.aggregate(NOW+HOUR+500).await.unwrap().applied,1);
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn failed_projection_rolls_back_resource_and_rollup_changes_together() {
    let store=fixture_store(NOW-DAY).await;
    let mut pending=call_event("call-1",NOW,RequestOutcome::Pending);
    pending.phase=ObservationPhase::Started; pending.event_id="call-1.start".into();
    store.persist(&[pending],NOW+100).await.unwrap();
    store.aggregate(NOW+100).await.unwrap();
    compact::Entity::update_many().col_expr(compact::Column::StateRevision,Expr::value(i64::MAX))
        .filter(compact::Column::ObjectId.eq("call-1")).exec(&store.db).await.unwrap();
    store.persist(&[call_event("call-1",NOW,RequestOutcome::Returned)],NOW+500).await.unwrap();
    assert_eq!(store.aggregate(NOW+500).await.unwrap().discarded,1);
    let result=store.overview(&query(NOW,NOW+HOUR),NOW+HOUR).await.unwrap();
    assert_eq!(count(&result.summary,Count::Calls),Some("1"));
    assert_eq!(count(&result.summary,Count::Pending),Some("1"));
    assert_eq!(count(&result.summary,Count::Returned),None);
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn trimmed_aggregate_ranges_are_unavailable_instead_of_zero_failure_rates() {
    let store=fixture_store(NOW-DAY).await;
    store.persist(&[call_event("call-1",NOW,RequestOutcome::HttpError)],NOW+500).await.unwrap();
    store.aggregate(NOW+500).await.unwrap();
    // Emulate the authoritative boundary used during bounded bucket deletion.
    // Existing rows still below the boundary must not leak a partial cohort.
    settings::Entity::update_many().col_expr(settings::Column::RollupTrimBeforeMs,Expr::value(NOW+HOUR))
        .filter(settings::Column::Id.eq(1)).exec(&store.db).await.unwrap();
    let result=store.overview(&query(NOW,NOW+HOUR),NOW+HOUR).await.unwrap();
    assert_eq!(result.coverage.sample_status,SampleStatus::Unavailable);
    assert_eq!(result.coverage.trimmed_before,Some(timestamp(NOW+HOUR)));
    let rate=result.summary.rates.iter().find(|rate| rate.key=="request_failure").unwrap();
    assert_eq!((rate.numerator.clone(),rate.denominator.clone(),rate.value),(None,None,None));
    assert_eq!(rate.reason,Some(MetricRateReason::RetentionTrim));
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn previous_period_before_activation_has_no_synthetic_zero_rates() {
    let store=fixture_store(NOW).await;
    store.persist(&[call_event("call-1",NOW,RequestOutcome::Returned)],NOW+500).await.unwrap();
    store.aggregate(NOW+500).await.unwrap();
    let result=store.overview(&query(NOW,NOW+HOUR),NOW+HOUR).await.unwrap();
    let previous=result.previous.unwrap();
    assert!(previous.rates.iter().all(|rate| rate.sample_status==SampleStatus::NotCollected));
    assert!(previous.rates.iter().all(|rate| rate.value.is_none() && rate.denominator.is_none()));
}
