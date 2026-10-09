use desk_diagnose_core::model_observability::protocol_correction::{ProtocolCorrectionFact,ProtocolCorrectionReason,ProtocolCorrectionCheck};

fn protocol_source(reason:ProtocolCorrectionReason) -> ObservationEvent {
    let mut source=call_event("protocol-source",NOW,RequestOutcome::Returned);
    let ObservationPayload::Call(call)=&mut source.payload else { unreachable!(); };
    call.output=match reason {
        ProtocolCorrectionReason::EmptyResponse=>OutputOutcome::EmptyResponse,
        ProtocolCorrectionReason::TruncatedOutput=>OutputOutcome::OutputTruncated,
        ProtocolCorrectionReason::CompletionInterpretation | ProtocolCorrectionReason::PermissionPlanProtocol
            | ProtocolCorrectionReason::PermissionActionMissing=>OutputOutcome::InvalidStructuredOutput,
        ProtocolCorrectionReason::PermissionProtocol=>OutputOutcome::InvalidProtocol,
        ProtocolCorrectionReason::GoalControlMissing=>OutputOutcome::Accepted,
        ProtocolCorrectionReason::ToolChoiceFallback=>{ call.outcome=RequestOutcome::HttpError; OutputOutcome::NotEvaluated },
    };
    source
}

fn protocol_fact(source:&ObservationEvent,id:&str,fact:ProtocolCorrectionFact) -> ObservationEvent {
    let mut event=source.clone(); event.event_id=id.into(); event.object_id=id.into();
    event.phase=ObservationPhase::Stage; event.sequence=0; event.occurred_at_ms=source.occurred_at_ms+500;
    event.payload=ObservationPayload::Call(CallSnapshot::default());
    event.relation=Some(ObservationRelation::ProtocolCorrection { source:ObservationAlias::model_call(&source.object_id).unwrap(),fact });
    event
}

fn protocol_feedback(source:&ObservationEvent,reason:ProtocolCorrectionReason) -> ObservationEvent {
    protocol_fact(source,"protocol-feedback",ProtocolCorrectionFact::Feedback { source_call_id:source.object_id.clone(),fence:Some(correction_fence()),reason })
}

fn protocol_projection(source:&ObservationEvent,next:&str,fence:Option<CorrectionFence>) -> ObservationEvent {
    protocol_fact(source,&format!("protocol-projected-{next}"),ProtocolCorrectionFact::Projected { source_call_id:source.object_id.clone(),next_call_id:next.into(),fence })
}

fn protocol_returned(source:&ObservationEvent,next:&str,fence:Option<CorrectionFence>) -> ObservationEvent {
    protocol_fact(source,&format!("protocol-returned-{next}"),ProtocolCorrectionFact::Returned { source_call_id:source.object_id.clone(),next_call_id:next.into(),fence })
}

fn protocol_checked(source:&ObservationEvent,next:&str,fence:Option<CorrectionFence>,check:ProtocolCorrectionCheck) -> ObservationEvent {
    protocol_fact(source,&format!("protocol-checked-{next}"),ProtocolCorrectionFact::Response { source_call_id:source.object_id.clone(),next_call_id:next.into(),fence,check })
}

#[tokio::test]
async fn protocol_recovery_keeps_calls_and_input_permission_operation_axes_independent() {
    for reason in [ProtocolCorrectionReason::EmptyResponse,ProtocolCorrectionReason::TruncatedOutput,
        ProtocolCorrectionReason::CompletionInterpretation,ProtocolCorrectionReason::PermissionProtocol,
        ProtocolCorrectionReason::PermissionPlanProtocol,ProtocolCorrectionReason::PermissionActionMissing,
        ProtocolCorrectionReason::GoalControlMissing,ProtocolCorrectionReason::ToolChoiceFallback] {
        let store=fixture_store(NOW-DAY).await;
        let source=protocol_source(reason);
        let next=call_event("protocol-next",NOW+2*HOUR,RequestOutcome::Returned);
        let feedback=protocol_feedback(&source,reason);
        store.persist(&[source.clone(),feedback.clone()],NOW+100).await.unwrap(); store.aggregate(NOW+100).await.unwrap();
        let writer=Store::new(store.db.clone(),store.manager,"protocol-writer".into());
        let projection=protocol_projection(&source,&next.object_id,Some(correction_fence()));
        let returned=protocol_returned(&source,&next.object_id,Some(correction_fence()));
        let checked=protocol_checked(&source,&next.object_id,Some(correction_fence()),ProtocolCorrectionCheck::Passed);
        writer.persist(&[checked.clone(),returned.clone(),projection.clone(),next.clone()],NOW+2*HOUR+100).await.unwrap();
        writer.aggregate(NOW+2*HOUR+100).await.unwrap(); writer.aggregate(NOW+2*HOUR+101).await.unwrap();
        let original=writer.call_detail(&source.object_id).await.unwrap().unwrap();
        let group=original.call.correction_group.unwrap();
        assert_eq!(group.reason,Some(tag(&reason)));
        assert_eq!(group.category,tag(&reason.category())); assert_eq!(group.outcome,"output_accepted");
        assert_eq!(group.last_input_id,next.object_id); assert_eq!(group.linked_attempts,"1");
        assert!(!original.call.correction_group_unavailable);
        let member=writer.call_detail(&next.object_id).await.unwrap().unwrap();
        assert_eq!(member.call.correction_group_root.as_deref(),Some(source.object_id.as_str()));
        assert!(member.call.correction_group.is_none());
        let all=writer.overview(&query(NOW-HOUR,NOW+3*HOUR),NOW+3*HOUR).await.unwrap();
        assert_eq!(count(&all.summary,Count::Calls),Some("2"));
        assert_eq!(count(&all.summary,Count::CorrectionGroups),Some("1"));
        assert_eq!(count(&all.summary,Count::CorrectionGroupOutputAccepted),Some("1"));
        for counter in [Count::Tools,Count::InputAccepted,Count::InputRejected,Count::PermissionApproved,
            Count::PermissionDenied,Count::OperationsDispatched,Count::CorrectionAccepted] {
            assert_eq!(count(&all.summary,counter),None);
        }
        let root_period=writer.overview(&query(NOW-HOUR,NOW+HOUR),NOW+3*HOUR).await.unwrap();
        let next_period=writer.overview(&query(NOW+2*HOUR,NOW+3*HOUR),NOW+3*HOUR).await.unwrap();
        assert_eq!(count(&root_period.summary,Count::CorrectionGroupOutputAccepted),Some("1"));
        assert_eq!(count(&next_period.summary,Count::CorrectionGroups),None);
        writer.persist(&[feedback,projection,returned,checked,next],NOW+2*HOUR+200).await.unwrap();
        writer.aggregate(NOW+2*HOUR+200).await.unwrap();
        let repeated=writer.overview(&query(NOW-HOUR,NOW+3*HOUR),NOW+3*HOUR).await.unwrap();
        assert_eq!(count(&repeated.summary,Count::Calls),Some("2"));
        assert_eq!(count(&repeated.summary,Count::CorrectionGroupOutputAccepted),Some("1"));
        assert_resource_accounting(&writer).await;
    }
}

#[tokio::test]
async fn protocol_returned_without_validation_is_pending_and_a_late_return_cannot_reopen_a_verdict() {
    let store=fixture_store(NOW-DAY).await; let source=protocol_source(ProtocolCorrectionReason::EmptyResponse);
    store.persist(&[source.clone(),protocol_feedback(&source,ProtocolCorrectionReason::EmptyResponse),
        call_event("protocol-next",NOW+HOUR,RequestOutcome::Returned),protocol_projection(&source,"protocol-next",Some(correction_fence())),
        protocol_returned(&source,"protocol-next",Some(correction_fence()))],NOW+HOUR+100).await.unwrap();
    store.aggregate(NOW+HOUR+100).await.unwrap(); store.aggregate(NOW+HOUR+101).await.unwrap();
    let first=store.call_detail(&source.object_id).await.unwrap().unwrap().call.correction_group.unwrap();
    assert_eq!(first.outcome,"awaiting_output_check");
    store.persist(&[protocol_checked(&source,"protocol-next",Some(correction_fence()),ProtocolCorrectionCheck::Rejected)],NOW+HOUR+200).await.unwrap();
    store.aggregate(NOW+HOUR+200).await.unwrap();
    let mut late=protocol_returned(&source,"protocol-next",Some(correction_fence())); late.object_id="late-protocol-returned".into(); late.event_id=late.object_id.clone();
    store.persist(&[late],NOW+HOUR+300).await.unwrap(); store.aggregate(NOW+HOUR+300).await.unwrap();
    let final_group=store.call_detail(&source.object_id).await.unwrap().unwrap().call.correction_group.unwrap();
    assert_eq!(final_group.outcome,"output_rejected");
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn protocol_missing_calls_are_unassociated_and_resume_without_synthetic_denominators() {
    let store=fixture_store(NOW-DAY).await; let source=protocol_source(ProtocolCorrectionReason::EmptyResponse);
    store.persist(&[protocol_feedback(&source,ProtocolCorrectionReason::EmptyResponse)],NOW+100).await.unwrap();
    store.aggregate(NOW+100).await.unwrap();
    let gaps=store.unassociated(&unassociated_query(NOW,NOW+HOUR,50),NOW+HOUR).await.unwrap();
    assert_eq!(gaps.records.len(),1); assert!(gaps.records[0].original_model.is_none()); assert!(gaps.records[0].started_at.is_none());
    let early=store.overview(&query(NOW-HOUR,NOW+HOUR),NOW+HOUR).await.unwrap(); assert_eq!(count(&early.summary,Count::Calls),None);
    store.persist(&[source.clone(),protocol_projection(&source,"missing-next",Some(correction_fence())),
        protocol_checked(&source,"missing-next",Some(correction_fence()),ProtocolCorrectionCheck::Passed)],NOW+200).await.unwrap();
    store.aggregate(NOW+200).await.unwrap(); store.aggregate(NOW+1_201).await.unwrap();
    let gaps=store.unassociated(&unassociated_query(NOW,NOW+HOUR,50),NOW+HOUR).await.unwrap();
    assert_eq!(gaps.records.len(),1); assert_eq!(gaps.records[0].missing,AssociationGapKind::CorrectionPrerequisite);
    assert_eq!(gaps.records[0].original_model.as_ref().unwrap().model_id,"model-1");
    assert_eq!(gaps.records[0].call_id.as_deref(),Some(source.object_id.as_str())); assert!(gaps.records[0].tool_observation_id.is_none());
    let writer=Store::new(store.db.clone(),store.manager,"late-protocol-writer".into());
    writer.persist(&[call_event("missing-next",NOW+HOUR,RequestOutcome::Returned)],NOW+HOUR+100).await.unwrap();
    writer.aggregate(NOW+HOUR+100).await.unwrap();
    assert!(writer.unassociated(&unassociated_query(NOW,NOW+2*HOUR,50),NOW+2*HOUR).await.unwrap().records.is_empty());
    let all=writer.overview(&query(NOW-HOUR,NOW+2*HOUR),NOW+2*HOUR).await.unwrap();
    assert_eq!(count(&all.summary,Count::Calls),Some("2")); assert_eq!(count(&all.summary,Count::CorrectionGroupOutputAccepted),Some("1"));
    assert_resource_accounting(&writer).await;
}

#[tokio::test]
async fn protocol_changed_model_missing_fence_and_first_projection_have_explicit_exclusions() {
    for mode in ["model","contract","missing","no_response","unavailable"] {
        let store=fixture_store(NOW-DAY).await; let source=protocol_source(ProtocolCorrectionReason::EmptyResponse);
        let mut next=call_event("protocol-first",NOW+HOUR,RequestOutcome::Returned);
        let mut fence=correction_fence();
        if mode=="model" { fence.model_id="model-2".into(); next.attribution.model_id="model-2".into(); }
        if mode=="contract" { fence.contract_revision="2".into(); next.attribution.contract_revision="2".into(); }
        let fence=(mode!="missing").then_some(fence);
        let check=match mode { "no_response"=>ProtocolCorrectionCheck::NoResponse,"unavailable"=>ProtocolCorrectionCheck::Unavailable,_=>ProtocolCorrectionCheck::Passed };
        store.persist(&[source.clone(),protocol_feedback(&source,ProtocolCorrectionReason::EmptyResponse),next,
            protocol_projection(&source,"protocol-first",fence.clone()),protocol_checked(&source,"protocol-first",fence,check)],NOW+HOUR+100).await.unwrap();
        store.aggregate(NOW+HOUR+100).await.unwrap(); store.aggregate(NOW+HOUR+101).await.unwrap();
        store.persist(&[call_event("protocol-later",NOW+2*HOUR,RequestOutcome::Returned),
            protocol_projection(&source,"protocol-later",Some(correction_fence())),
            protocol_checked(&source,"protocol-later",Some(correction_fence()),ProtocolCorrectionCheck::Passed)],NOW+2*HOUR+100).await.unwrap();
        store.aggregate(NOW+2*HOUR+100).await.unwrap();
        let root=store.call_detail(&source.object_id).await.unwrap().unwrap(); let group=root.call.correction_group.unwrap();
        assert_eq!(group.last_input_id,"protocol-first");
        assert_eq!(group.outcome,match mode { "model"=>"switched","contract"|"missing"=>"not_comparable","no_response"=>"no_response",_=>"unavailable" });
        let overview=store.overview(&query(NOW-HOUR,NOW+3*HOUR),NOW+3*HOUR).await.unwrap();
        assert_eq!(count(&overview.summary,Count::CorrectionGroups),Some("1"));
        assert_eq!(count(&overview.summary,Count::CorrectionGroupOutputAccepted),None);
        assert_resource_accounting(&store).await;
    }
}

#[tokio::test]
async fn later_recovery_source_keeps_its_own_category_when_the_previous_response_arrives_late() {
    let store=fixture_store(NOW-DAY).await; let source=protocol_source(ProtocolCorrectionReason::EmptyResponse);
    let mut next=protocol_source(ProtocolCorrectionReason::PermissionPlanProtocol);
    next.object_id="approval-protocol-source".into(); next.event_id="approval-protocol-source.terminal".into();
    next.started_at_ms=NOW+HOUR; next.occurred_at_ms=NOW+HOUR+100;
    let mut next_feedback=protocol_feedback(&next,ProtocolCorrectionReason::PermissionPlanProtocol);
    next_feedback.event_id="approval-protocol-feedback".into(); next_feedback.object_id=next_feedback.event_id.clone();
    store.persist(&[source.clone(),protocol_feedback(&source,ProtocolCorrectionReason::EmptyResponse),next.clone(),next_feedback,
        protocol_projection(&source,&next.object_id,Some(correction_fence()))],NOW+HOUR+200).await.unwrap();
    store.aggregate(NOW+HOUR+200).await.unwrap();
    store.persist(&[protocol_checked(&source,&next.object_id,Some(correction_fence()),ProtocolCorrectionCheck::Rejected)],NOW+HOUR+300).await.unwrap();
    store.aggregate(NOW+HOUR+300).await.unwrap();
    let older=store.call_detail(&source.object_id).await.unwrap().unwrap().call.correction_group.unwrap();
    let newer=store.call_detail(&next.object_id).await.unwrap().unwrap().call.correction_group.unwrap();
    assert_eq!(older.category,"protocol"); assert_eq!(older.outcome,"output_rejected");
    assert_eq!(newer.category,"approval"); assert_eq!(newer.outcome,"awaiting_response");
    let overview=store.overview(&query(NOW-HOUR,NOW+2*HOUR),NOW+2*HOUR).await.unwrap();
    assert_eq!(count(&overview.summary,Count::CorrectionGroups),Some("2"));
    assert_resource_accounting(&store).await;
}
