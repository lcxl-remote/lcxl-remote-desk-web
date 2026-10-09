use desk_diagnose_core::model_observability::correction::{CorrectionCandidate,CorrectionFact,CorrectionFence,CorrectionResponse};

fn correction_fence() -> CorrectionFence {
    CorrectionFence { input_revision:1,focus_revision:1,source_input_id:"owner-input".into(),goal:None,
        provider_id:"provider-1".into(),model_id:"model-1".into(),configuration_revision:"1".into(),contract_revision:"1".into() }
}

fn correction_input(id: &str,started: i64,conclusion: InputConclusion) -> ObservationEvent {
    let mut event=bound_tool(ObservationAlias::source_message("server-message",0).unwrap());
    event.object_id=format!("{id}.tool.0"); event.call_id=Some(id.into()); event.event_id=format!("{id}.input");
    event.started_at_ms=started; event.occurred_at_ms=started+10;
    let ObservationPayload::Tool(tool)=&mut event.payload else { unreachable!(); };
    tool.stages.insert(Stage::Dispatch,StageOutcome::NotReached);
    tool.conclusion=conclusion;
    if conclusion==InputConclusion::Rejected { tool.stages.insert(Stage::Schema,StageOutcome::Failed); tool.issue=InputIssue::Type; }
    if conclusion==InputConclusion::Accepted { tool.stages.insert(Stage::Preflight,StageOutcome::Passed); }
    event
}

fn correction_feedback(source: &ObservationEvent) -> ObservationEvent {
    let mut event=source.clone(); event.event_id=format!("{}.feedback",source.object_id); event.sequence=2;
    if let ObservationPayload::Tool(tool)=&mut event.payload { tool.correction_status=CorrectionStatus::AwaitingResponse; }
    let alias=match &source.relation { Some(ObservationRelation::Bind(alias))=>alias.clone(),_=>ObservationAlias::source_message("server-message",0).unwrap() };
    event.relation=Some(ObservationRelation::Correction { source:alias,
        fact:CorrectionFact::Feedback { fence:correction_fence() } });
    event
}

fn correction_marker(next: &str,fence: CorrectionFence,response: Option<CorrectionResponse>) -> ObservationEvent {
    let alias=ObservationAlias::source_message("server-message",0).unwrap();
    let mut event=ObservationEvent::deferred_tool(alias.clone(),ObservationPhase::Stage,0,NOW+300,
        BTreeMap::new(),PermissionOutcome::NotReached,None,InputIssue::None);
    let suffix=if response.is_some() { "response" } else { "projected" };
    event.object_id=format!("{next}.correction.{suffix}"); event.event_id=event.object_id.clone();
    event.relation=Some(ObservationRelation::Correction { source:alias,fact:match response {
        Some(response)=>CorrectionFact::Response { fence,next_call_id:next.into(),response },
        None=>CorrectionFact::Projected { fence,next_call_id:next.into() },
    } });
    event
}

fn correction_candidate(next: &str) -> CorrectionResponse {
    CorrectionResponse::Returned(Some(CorrectionCandidate { object_id:format!("{next}.tool.0"),tool_key:"execute_confirmed_ui_action".into() }))
}

async fn collected_correction_source(store: &Store) -> ObservationEvent {
    let source=correction_input("original-call",NOW,InputConclusion::Rejected);
    let mut binding=source.clone();
    if let ObservationPayload::Tool(tool)=&mut binding.payload {
        tool.conclusion=InputConclusion::Unknown; tool.issue=InputIssue::None;
        tool.stages.insert(Stage::Schema,StageOutcome::NotReached);
    }
    assert_eq!(store.persist(&[binding,correction_feedback(&source)],NOW+100).await.unwrap(),0);
    assert_eq!(store.aggregate(NOW+100).await.unwrap().discarded,0);
    source
}

#[tokio::test]
async fn correction_survives_writer_handoff_and_keeps_rates_in_the_original_feedback_cohort() {
    let store=fixture_store(NOW-DAY).await;
    let source=collected_correction_source(&store).await;
    let next_store=Store::new(store.db.clone(),store.manager,"resumed-node".into());
    let next_start=NOW+2*HOUR;
    let mut next=correction_input("next-call",next_start,InputConclusion::Unknown); next.relation=None;
    next.attribution.origin=Origin::PermissionResume;
    let projected=correction_marker("next-call",correction_fence(),None);
    let response=correction_marker("next-call",correction_fence(),Some(correction_candidate("next-call")));
    assert_eq!(next_store.persist(&[response.clone(),projected.clone(),next.clone()],next_start).await.unwrap(),0);
    assert_eq!(next_store.aggregate(next_start).await.unwrap().discarded,0);
    assert_eq!(next_store.aggregate(next_start+1).await.unwrap().discarded,0);
    let parent=detail::Entity::find_by_id(&source.object_id).one(&store.db).await.unwrap().unwrap();
    let parent: ObservationEvent=decode(&parent.snapshot_json).unwrap();
    let ObservationPayload::Tool(parent)=parent.payload else { unreachable!(); };
    assert_eq!(parent.correction_status,CorrectionStatus::Linked); assert_eq!(parent.correction_input,None);

    let mut verdict=next.clone(); verdict.event_id="next-call.validation".into(); verdict.sequence=2;
    if let ObservationPayload::Tool(tool)=&mut verdict.payload { tool.conclusion=InputConclusion::Accepted; tool.stages.insert(Stage::Preflight,StageOutcome::Passed); }
    next_store.persist(&[verdict],next_start+200).await.unwrap(); next_store.aggregate(next_start+200).await.unwrap();
    let original_period=next_store.overview(&query(NOW-HOUR,NOW+HOUR),next_start+HOUR).await.unwrap();
    assert_eq!(count(&original_period.summary,Count::CorrectionLinked),Some("1"));
    assert_eq!(count(&original_period.summary,Count::CorrectionAccepted),Some("1"));
    assert_eq!(count(&original_period.summary,Count::InputRejected),Some("1"));
    let later_period=next_store.overview(&query(next_start,next_start+HOUR),next_start+HOUR).await.unwrap();
    assert_eq!(count(&later_period.summary,Count::CorrectionAccepted),None);
    assert_eq!(count(&later_period.summary,Count::InputAccepted),Some("1"));
    let row=detail::Entity::find_by_id(&next.object_id).one(&store.db).await.unwrap().unwrap();
    let next: ObservationEvent=decode(&row.snapshot_json).unwrap();
    assert_eq!(next.started_at_ms,next_start); assert_eq!(next.attribution.origin,Origin::PermissionResume);
    let ObservationPayload::Tool(next)=next.payload else { unreachable!(); };
    assert_eq!(next.correction_of.as_deref(),Some(source.object_id.as_str()));
    assert_eq!(next_store.persist(&[projected,response,correction_feedback(&source)],next_start+300).await.unwrap(),0);
    next_store.aggregate(next_start+300).await.unwrap();
    let repeated=next_store.overview(&query(NOW-HOUR,NOW+HOUR),next_start+HOUR).await.unwrap();
    assert_eq!(count(&repeated.summary,Count::CorrectionAccepted),Some("1"));
    assert_resource_accounting(&next_store).await;
}

#[tokio::test]
async fn correction_consumes_only_the_first_projected_request_and_never_a_later_replayed_success() {
    let store=fixture_store(NOW-DAY).await; let source=collected_correction_source(&store).await;
    let first=correction_marker("first-next",correction_fence(),None);
    let missing=correction_marker("first-next",correction_fence(),Some(CorrectionResponse::NoResponse));
    store.persist(&[first,missing],NOW+500).await.unwrap(); store.aggregate(NOW+500).await.unwrap();
    let mut later=correction_input("later-success",NOW+HOUR,InputConclusion::Accepted); later.relation=None;
    store.persist(&[later.clone(),correction_marker("later-success",correction_fence(),None),
        correction_marker("later-success",correction_fence(),Some(correction_candidate("later-success")))],NOW+HOUR).await.unwrap();
    store.aggregate(NOW+HOUR).await.unwrap(); store.aggregate(NOW+HOUR+1).await.unwrap();
    let original=detail::Entity::find_by_id(&source.object_id).one(&store.db).await.unwrap().unwrap();
    let original: ObservationEvent=decode(&original.snapshot_json).unwrap();
    let ObservationPayload::Tool(original)=original.payload else { unreachable!(); };
    assert_eq!(original.correction_status,CorrectionStatus::NoResponse);
    let next=detail::Entity::find_by_id(&later.object_id).one(&store.db).await.unwrap().unwrap();
    let next: ObservationEvent=decode(&next.snapshot_json).unwrap();
    let ObservationPayload::Tool(next)=next.payload else { unreachable!(); }; assert_eq!(next.correction_of,None);
    let overview=store.overview(&query(NOW-HOUR,NOW+2*HOUR),NOW+2*HOUR).await.unwrap();
    assert_eq!(count(&overview.summary,Count::CorrectionNoResponse),Some("1"));
    assert_eq!(count(&overview.summary,Count::CorrectionLinked),None);
    assert_eq!(count(&overview.summary,Count::CorrectionGroupNoResponse),Some("1"));
    let group=store.call_detail(&source.object_id).await.unwrap().unwrap().call.correction_group.unwrap();
    assert_eq!(group.outcome,"no_response"); assert_eq!(group.linked_attempts,"0");
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn correction_model_input_and_contract_changes_are_excluded_from_comparable_denominators() {
    for change in ["model","input","contract","ambiguous"] {
        let store=fixture_store(NOW-DAY).await; let source=collected_correction_source(&store).await;
        let mut fence=correction_fence();
        match change { "model"=>fence.model_id="model-2".into(),"input"=>fence.input_revision+=1,
            "contract"=>fence.contract_revision="2".into(),_=>{}, }
        store.persist(&[correction_marker("next",fence.clone(),None),
            correction_marker("next",fence,Some(CorrectionResponse::Returned(None)))],NOW+500).await.unwrap();
        assert_eq!(store.aggregate(NOW+500).await.unwrap().discarded,0);
        let row=detail::Entity::find_by_id(&source.object_id).one(&store.db).await.unwrap().unwrap();
        let original: ObservationEvent=decode(&row.snapshot_json).unwrap();
        let ObservationPayload::Tool(tool)=original.payload else { unreachable!(); };
        assert_eq!(tool.correction_status,match change { "model"=>CorrectionStatus::Switched,"ambiguous"=>CorrectionStatus::Ambiguous,_=>CorrectionStatus::NotComparable });
        let overview=store.overview(&query(NOW-HOUR,NOW+HOUR),NOW+HOUR).await.unwrap();
        assert_eq!(count(&overview.summary,Count::CorrectionResponded),if change=="ambiguous" { Some("1") } else { None });
        assert_eq!(count(&overview.summary,Count::CorrectionLinked),None);
        let group=store.call_detail(&source.object_id).await.unwrap().unwrap().call.correction_group.unwrap();
        assert_eq!(group.outcome,match change { "model"=>"switched","ambiguous"=>"ambiguous",_=>"not_comparable" });
        assert_eq!(group.linked_attempts,"0"); assert_resource_accounting(&store).await;
    }
}

#[tokio::test]
async fn missing_feedback_reports_a_gap_without_recreating_an_opportunity_from_business_history() {
    let store=fixture_store(NOW-DAY).await;
    let source=correction_input("original-call",NOW,InputConclusion::Rejected);
    store.persist(&[source],NOW+100).await.unwrap(); store.aggregate(NOW+100).await.unwrap();
    store.persist(&[correction_marker("next",correction_fence(),None)],NOW+500).await.unwrap();
    assert_eq!(store.aggregate(NOW+500).await.unwrap().applied,0);
    assert_eq!(store.aggregate(NOW+30_501).await.unwrap().discarded,0);
    let gaps=store.unassociated(&unassociated_query(NOW,NOW+HOUR,50),NOW+30_501).await.unwrap();
    assert_eq!(gaps.records.len(),1);
    assert_eq!(gaps.records[0].missing,AssociationGapKind::CorrectionPrerequisite);
    assert_eq!(gaps.records[0].state,AssociationGapState::Unavailable);
    assert_eq!(gaps.records[0].original_model.as_ref().unwrap().model_id,"model-1");
    let row=compact::Entity::find_by_id("original-call.tool.0").one(&store.db).await.unwrap().unwrap();
    let state: CompactState=decode(&row.snapshot_json).unwrap(); assert!(state.correction.is_none());
    assert_eq!(store.overview(&query(NOW,NOW+HOUR),NOW+HOUR).await.unwrap().coverage.sample_status,SampleStatus::Partial);
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn conflicting_candidate_does_not_partly_link_the_original_input() {
    let store=fixture_store(NOW-DAY).await; let source=collected_correction_source(&store).await;
    let mut candidate=correction_input("next",NOW+HOUR,InputConclusion::Accepted); candidate.relation=None;
    candidate.attribution.model_id="unexpected-model".into();
    store.persist(&[candidate,correction_marker("next",correction_fence(),None),
        correction_marker("next",correction_fence(),Some(correction_candidate("next")))],NOW+HOUR).await.unwrap();
    let first=store.aggregate(NOW+HOUR).await.unwrap();
    let second=store.aggregate(NOW+HOUR+1).await.unwrap();
    assert_eq!(first.discarded+second.discarded,1);
    assert_eq!(store.unassociated(&unassociated_query(NOW,NOW+2*HOUR,50),NOW+HOUR+1).await.unwrap().records[0].state,AssociationGapState::Conflict);
    let row=detail::Entity::find_by_id(&source.object_id).one(&store.db).await.unwrap().unwrap();
    let original: ObservationEvent=decode(&row.snapshot_json).unwrap();
    let ObservationPayload::Tool(tool)=original.payload else { unreachable!(); };
    assert_eq!(tool.correction_status,CorrectionStatus::AwaitingResponse); assert_eq!(tool.correction_input,None);
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn consecutive_invalid_inputs_keep_distinct_opportunities_and_known_validation_conclusions() {
    let store=fixture_store(NOW-DAY).await; collected_correction_source(&store).await;
    let mut second=correction_input("second-call",NOW+HOUR,InputConclusion::Rejected);
    let second_alias=ObservationAlias::source_message("second-message",0).unwrap();
    second.relation=Some(ObservationRelation::Bind(second_alias.clone()));
    store.persist(&[second.clone(),correction_marker("second-call",correction_fence(),None),
        correction_marker("second-call",correction_fence(),Some(correction_candidate("second-call")))],NOW+HOUR).await.unwrap();
    store.aggregate(NOW+HOUR).await.unwrap(); store.aggregate(NOW+HOUR+1).await.unwrap();
    store.persist(&[correction_feedback(&second)],NOW+HOUR+100).await.unwrap(); store.aggregate(NOW+HOUR+100).await.unwrap();
    let mut third=correction_input("third-call",NOW+2*HOUR,InputConclusion::Accepted); third.relation=None;
    let mut projection=correction_marker("third-call",correction_fence(),None);
    let mut response=correction_marker("third-call",correction_fence(),Some(correction_candidate("third-call")));
    for event in [&mut projection,&mut response] {
        let Some(ObservationRelation::Correction { source,.. })=&mut event.relation else { unreachable!(); };
        *source=second_alias.clone();
    }
    store.persist(&[third,projection,response],NOW+2*HOUR).await.unwrap();
    store.aggregate(NOW+2*HOUR).await.unwrap(); store.aggregate(NOW+2*HOUR+1).await.unwrap();
    let overview=store.overview(&query(NOW-HOUR,NOW+3*HOUR),NOW+3*HOUR).await.unwrap();
    assert_eq!(count(&overview.summary,Count::CorrectionLinked),Some("2"));
    assert_eq!(count(&overview.summary,Count::CorrectionAccepted),Some("1"));
    assert_eq!(count(&overview.summary,Count::CorrectionRejected),Some("1"));
    let row=detail::Entity::find_by_id(&second.object_id).one(&store.db).await.unwrap().unwrap();
    let second: ObservationEvent=decode(&row.snapshot_json).unwrap();
    let ObservationPayload::Tool(tool)=second.payload else { unreachable!(); };
    assert_eq!(tool.correction_of.as_deref(),Some("original-call.tool.0"));
    assert_eq!(tool.correction_input,Some(InputConclusion::Accepted));
    assert_eq!(count(&overview.summary,Count::CorrectionGroups),Some("1"));
    assert_eq!(count(&overview.summary,Count::CorrectionGroupInputAccepted),Some("1"));
    let original=store.call_detail("original-call.tool.0").await.unwrap().unwrap();
    let group=original.call.correction_group.unwrap();
    assert_eq!(group.root_id,"original-call.tool.0"); assert_eq!(group.last_input_id,"third-call.tool.0");
    assert_eq!(group.linked_attempts,"2"); assert_eq!(group.outcome,"input_accepted");
    let member=store.call_detail("second-call.tool.0").await.unwrap().unwrap();
    assert_eq!(member.call.correction_group_root.as_deref(),Some("original-call.tool.0"));
    assert!(member.call.correction_group.is_none()); assert!(!member.call.correction_group_unavailable);
    let root_period=store.overview(&query(NOW-HOUR,NOW+HOUR),NOW+3*HOUR).await.unwrap();
    let later_period=store.overview(&query(NOW+HOUR,NOW+3*HOUR),NOW+3*HOUR).await.unwrap();
    assert_eq!(count(&root_period.summary,Count::CorrectionGroupInputAccepted),Some("1"));
    assert_eq!(count(&later_period.summary,Count::CorrectionGroups),None);
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn frozen_original_feedback_does_not_discard_the_later_inputs_ordinary_metrics() {
    let store=fixture_store(NOW-DAY).await; collected_correction_source(&store).await;
    let mut candidate=correction_input("next",NOW+2*HOUR,InputConclusion::Unknown); candidate.relation=None;
    store.persist(&[candidate.clone(),correction_marker("next",correction_fence(),None),
        correction_marker("next",correction_fence(),Some(correction_candidate("next")))],NOW+2*HOUR).await.unwrap();
    store.aggregate(NOW+2*HOUR).await.unwrap(); store.aggregate(NOW+2*HOUR+1).await.unwrap();
    rollup::Entity::update_many().col_expr(rollup::Column::Frozen,Expr::value(true))
        .filter(rollup::Column::BucketMs.eq(NOW)).exec(&store.db).await.unwrap();
    candidate.event_id="next.validation".into(); candidate.sequence=2;
    if let ObservationPayload::Tool(tool)=&mut candidate.payload { tool.conclusion=InputConclusion::Accepted; tool.stages.insert(Stage::Preflight,StageOutcome::Passed); }
    store.persist(&[candidate],NOW+2*HOUR+200).await.unwrap();
    assert_eq!(store.aggregate(NOW+2*HOUR+200).await.unwrap().discarded,0);
    let later=store.overview(&query(NOW+2*HOUR,NOW+3*HOUR),NOW+3*HOUR).await.unwrap();
    assert_eq!(count(&later.summary,Count::InputAccepted),Some("1"));
    assert!(settings::Entity::find_by_id(1).one(&store.db).await.unwrap().unwrap().coverage_partial);
    let original=store.call_detail("original-call.tool.0").await.unwrap().unwrap();
    assert_eq!(original.call.correction_group.unwrap().outcome,"awaiting_validation");
    assert_resource_accounting(&store).await;
}

fn correction_for_alias(mut event: ObservationEvent,alias: &ObservationAlias) -> ObservationEvent {
    let Some(ObservationRelation::Correction { source,.. })=&mut event.relation else { unreachable!(); };
    *source=alias.clone(); event
}

#[tokio::test]
async fn late_parent_link_demotes_a_provisional_group_atomically_and_replay_keeps_one_root() {
    let store=fixture_store(NOW-DAY).await;
    collected_correction_source(&store).await;
    let alias=ObservationAlias::source_message("provisional-message",0).unwrap();
    let mut second=correction_input("provisional",NOW+HOUR,InputConclusion::Rejected);
    second.relation=Some(ObservationRelation::Bind(alias.clone()));
    store.persist(&[second.clone(),correction_feedback(&second)],NOW+HOUR+100).await.unwrap();
    store.aggregate(NOW+HOUR+100).await.unwrap();
    let before=store.overview(&query(NOW-HOUR,NOW+2*HOUR),NOW+2*HOUR).await.unwrap();
    assert_eq!(count(&before.summary,Count::CorrectionGroups),Some("2"));
    let writer=Store::new(store.db.clone(),store.manager,"group-writer".into());
    let mut last=correction_input("group-last",NOW+2*HOUR,InputConclusion::Accepted); last.relation=None;
    if let ObservationPayload::Tool(tool)=&mut last.payload { tool.permission=PermissionOutcome::Denied; }
    let projected=correction_for_alias(correction_marker("group-last",correction_fence(),None),&alias);
    let response=correction_for_alias(correction_marker("group-last",correction_fence(),Some(correction_candidate("group-last"))),&alias);
    writer.persist(&[last.clone(),projected.clone(),response.clone()],NOW+2*HOUR+100).await.unwrap();
    writer.aggregate(NOW+2*HOUR+100).await.unwrap(); writer.aggregate(NOW+2*HOUR+101).await.unwrap();
    let parent_projection=correction_marker("provisional",correction_fence(),None);
    let parent_response=correction_marker("provisional",correction_fence(),Some(correction_candidate("provisional")));
    writer.persist(&[parent_response.clone(),parent_projection.clone()],NOW+2*HOUR+200).await.unwrap();
    writer.aggregate(NOW+2*HOUR+200).await.unwrap(); writer.aggregate(NOW+2*HOUR+201).await.unwrap();
    let after=writer.overview(&query(NOW-HOUR,NOW+3*HOUR),NOW+3*HOUR).await.unwrap();
    assert_eq!(count(&after.summary,Count::CorrectionGroups),Some("1"));
    assert_eq!(count(&after.summary,Count::CorrectionGroupInputAccepted),Some("1"));
    assert_eq!(count(&after.summary,Count::PermissionDenied),Some("1"));
    assert_eq!(count(&after.summary,Count::OperationsDispatched),None);
    let original=writer.call_detail("original-call.tool.0").await.unwrap().unwrap();
    let group=original.call.correction_group.unwrap();
    assert_eq!((group.linked_attempts.as_str(),group.last_input_id.as_str()),("2","group-last.tool.0"));
    let provisional=writer.call_detail("provisional.tool.0").await.unwrap().unwrap();
    assert!(provisional.call.correction_group.is_none());
    assert_eq!(provisional.call.correction_group_root.as_deref(),Some("original-call.tool.0"));
    writer.persist(&[parent_projection,parent_response,projected,response,correction_feedback(&second),last],NOW+2*HOUR+300).await.unwrap();
    writer.aggregate(NOW+2*HOUR+300).await.unwrap();
    let repeated=writer.overview(&query(NOW-HOUR,NOW+3*HOUR),NOW+3*HOUR).await.unwrap();
    assert_eq!(count(&repeated.summary,Count::CorrectionGroups),Some("1"));
    assert_eq!(count(&repeated.summary,Count::CorrectionGroupInputAccepted),Some("1"));
    assert_resource_accounting(&writer).await;
}

#[tokio::test]
async fn input_group_categories_use_registered_contracts_and_never_owner_decisions() {
    use desk_diagnose_core::{schedule::proposal::REQUEST_SCHEDULE,permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME};
    for (key,category,counter) in [(REQUEST_SCHEDULE,"schedule",Count::CorrectionGroupSchedule),
        (REQUEST_CAPABILITY_GRANTS_TOOL_NAME,"approval",Count::CorrectionGroupApproval)] {
        let store=fixture_store(NOW-DAY).await;
        let mut source=correction_input("original-call",NOW,InputConclusion::Rejected);
        if let ObservationPayload::Tool(tool)=&mut source.payload { tool.tool_key=key.into(); }
        store.persist(&[source.clone(),correction_feedback(&source)],NOW+100).await.unwrap();
        store.aggregate(NOW+100).await.unwrap();
        let group=store.call_detail(&source.object_id).await.unwrap().unwrap().call.correction_group.unwrap();
        assert_eq!(group.category,category); assert_eq!(group.outcome,"awaiting_response");
        let overview=store.overview(&query(NOW-HOUR,NOW+HOUR),NOW+HOUR).await.unwrap();
        assert_eq!(count(&overview.summary,counter),Some("1"));
        assert_eq!(count(&overview.summary,Count::PermissionDenied),None);
        assert_eq!(count(&overview.summary,Count::CorrectionGroupProtocol),None);
        assert_resource_accounting(&store).await;
    }
}

#[tokio::test]
async fn damaged_group_metadata_cannot_discard_a_later_inputs_validation() {
    let store=fixture_store(NOW-DAY).await; collected_correction_source(&store).await;
    let mut candidate=correction_input("damage-next",NOW+HOUR,InputConclusion::Unknown); candidate.relation=None;
    store.persist(&[candidate.clone(),correction_marker("damage-next",correction_fence(),None),
        correction_marker("damage-next",correction_fence(),Some(correction_candidate("damage-next")))],NOW+HOUR+100).await.unwrap();
    store.aggregate(NOW+HOUR+100).await.unwrap(); store.aggregate(NOW+HOUR+101).await.unwrap();
    let row=compact::Entity::find_by_id("original-call.tool.0").one(&store.db).await.unwrap().unwrap();
    let mut state:CompactState=decode(&row.snapshot_json).unwrap();
    state.group.as_mut().unwrap().summary.as_mut().unwrap().root_id="corrupt!-call.tool.0".into();
    let damaged=encode(&state).unwrap(); assert_eq!(damaged.len(),row.snapshot_json.len());
    compact::Entity::update_many().col_expr(compact::Column::SnapshotJson,Expr::value(damaged))
        .filter(compact::Column::ObjectId.eq("original-call.tool.0")).exec(&store.db).await.unwrap();
    candidate.event_id="damage-next.validated".into(); candidate.sequence=2;
    if let ObservationPayload::Tool(tool)=&mut candidate.payload { tool.conclusion=InputConclusion::Accepted; tool.stages.insert(Stage::Preflight,StageOutcome::Passed); }
    store.persist(&[candidate],NOW+HOUR+200).await.unwrap();
    assert_eq!(store.aggregate(NOW+HOUR+200).await.unwrap().discarded,0);
    let later=store.overview(&query(NOW+HOUR,NOW+2*HOUR),NOW+2*HOUR).await.unwrap();
    assert_eq!(count(&later.summary,Count::InputAccepted),Some("1"));
    let original=store.call_detail("original-call.tool.0").await.unwrap().unwrap();
    assert!(original.call.correction_group_unavailable); assert!(original.call.correction_group.is_none());
    assert!(settings::Entity::find_by_id(1).one(&store.db).await.unwrap().unwrap().coverage_partial);
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn bounded_group_walk_keeps_overflow_explicit_and_does_not_create_more_roots() {
    use desk_diagnose_core::model_observability::correction_group::MAX_GROUP_LINKS;
    let store=fixture_store(NOW-DAY).await;
    let mut parent=collected_correction_source(&store).await;
    let mut alias=ObservationAlias::source_message("server-message",0).unwrap();
    for index in 0..MAX_GROUP_LINKS+2 {
        let call_id=format!("bounded-{index}");
        let next_alias=ObservationAlias::source_message(&format!("bounded-message-{index}"),0).unwrap();
        let mut next=correction_input(&call_id,NOW+1_000+i64::from(index)*1_000,InputConclusion::Rejected);
        next.relation=Some(ObservationRelation::Bind(next_alias.clone()));
        let projected=correction_for_alias(correction_marker(&call_id,correction_fence(),None),&alias);
        let response=correction_for_alias(correction_marker(&call_id,correction_fence(),Some(correction_candidate(&call_id))),&alias);
        let at=next.started_at_ms+100;
        store.persist(&[next.clone(),correction_feedback(&next),projected,response],at).await.unwrap();
        store.aggregate(at).await.unwrap(); store.aggregate(at+1).await.unwrap();
        alias=next_alias; parent=next;
    }
    let root=store.call_detail("original-call.tool.0").await.unwrap().unwrap();
    let group=root.call.correction_group.unwrap();
    assert_eq!(group.outcome,"unavailable"); assert_eq!(group.linked_attempts,MAX_GROUP_LINKS.to_string());
    let last=store.call_detail(&parent.object_id).await.unwrap().unwrap();
    assert!(last.call.correction_group.is_none());
    assert_eq!(last.call.correction_group_root.as_deref(),Some("original-call.tool.0"));
    assert!(last.call.correction_group_unavailable);
    let overview=store.overview(&query(NOW-HOUR,NOW+HOUR),NOW+HOUR).await.unwrap();
    assert_eq!(count(&overview.summary,Count::CorrectionGroups),Some("1"));
    assert_eq!(count(&overview.summary,Count::CorrectionGroupUnavailable),Some("1"));
    assert!(settings::Entity::find_by_id(1).one(&store.db).await.unwrap().unwrap().coverage_partial);
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn a_retained_member_marks_its_trimmed_group_root_unavailable() {
    use desk_diagnose_core::model_observability::capacity::StorageKind;
    let store=fixture_store(NOW-DAY).await; collected_correction_source(&store).await;
    let mut next=correction_input("retained-member",NOW+HOUR,InputConclusion::Accepted); next.relation=None;
    store.persist(&[next,correction_marker("retained-member",correction_fence(),None),
        correction_marker("retained-member",correction_fence(),Some(correction_candidate("retained-member")))],NOW+HOUR+100).await.unwrap();
    store.aggregate(NOW+HOUR+100).await.unwrap(); store.aggregate(NOW+HOUR+101).await.unwrap();
    let row=compact::Entity::find_by_id("original-call.tool.0").one(&store.db).await.unwrap().unwrap();
    let txn=store.db.begin().await.unwrap();
    let (_,config)=resources::configuration(&txn).await.unwrap();
    // Emulate the shared boundary and both precision marks before compact trim.
    settings::Entity::update_many().col_expr(settings::Column::FrozenBeforeMs,Expr::value(NOW+HOUR))
        .col_expr(settings::Column::CoveragePartial,Expr::value(true)).filter(settings::Column::Id.eq(1)).exec(&txn).await.unwrap();
    rollup::Entity::update_many().col_expr(rollup::Column::Frozen,Expr::value(true))
        .filter(rollup::Column::BucketMs.lt(NOW+HOUR)).exec(&txn).await.unwrap();
    compact::Entity::delete_by_id(&row.object_id).exec(&txn).await.unwrap();
    resources::release(&txn,&config,StorageKind::Compact,1,row.storage_bytes).await.unwrap();
    txn.commit().await.unwrap();
    let member=store.call_detail("retained-member.tool.0").await.unwrap().unwrap();
    assert_eq!(member.call.correction_group_root.as_deref(),Some("original-call.tool.0"));
    assert!(member.call.correction_group_unavailable); assert!(member.call.correction_group.is_none());
    let root=store.call_detail("original-call.tool.0").await.unwrap().unwrap();
    assert!(root.call.correction_group_unavailable); assert!(root.call.correction_group.is_none());
    assert_resource_accounting(&store).await;
}
