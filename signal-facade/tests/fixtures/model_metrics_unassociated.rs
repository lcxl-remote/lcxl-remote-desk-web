fn unassociated_query(from:i64,to:i64,limit:u32) -> desk_signal_facade::service::model_metrics::unassociated::ResolvedUnassociatedQuery {
    desk_signal_facade::service::model_metrics::unassociated::ResolvedUnassociatedQuery::resolve(&UnassociatedQuery {
        from:Some(timestamp(from)),to:Some(timestamp(to)),limit:Some(limit),cursor:None,
    },NOW+10*DAY).unwrap()
}

#[tokio::test]
async fn missing_alias_is_visible_and_late_original_source_resolves_once_on_another_writer() {
    let store=fixture_store(NOW-DAY).await;
    let alias=ObservationAlias::provider_work("late-collected-work").unwrap();
    let fact=deferred_operation(alias.clone(),OperationOutcome::Verified);
    store.persist(std::slice::from_ref(&fact),NOW+2_000).await.unwrap();
    assert_eq!(store.aggregate(NOW+2_001).await.unwrap().discarded,0);
    let page=store.unassociated(&unassociated_query(NOW,NOW+HOUR,50),NOW+40_000).await.unwrap();
    assert_eq!(page.records.len(),1);
    let gap=&page.records[0];
    assert_eq!(gap.missing,AssociationGapKind::Attribution);
    assert_eq!(gap.state,AssociationGapState::Unavailable);
    assert_eq!(gap.fact_outcome.as_deref(),Some("verified"));
    assert!(gap.original_model.is_none() && gap.started_at.is_none() && gap.call_id.is_none() && gap.tool.is_none());
    assert_eq!(gap.received_at,timestamp(NOW+2_000));
    assert!(store.call_detail(&gap.id).await.unwrap().is_none());
    assert!(store.calls(&query(NOW,NOW+HOUR),NOW+HOUR).await.unwrap().records.is_empty());
    let summary=store.overview(&query(NOW,NOW+HOUR),NOW+HOUR).await.unwrap();
    assert_eq!(summary.coverage.sample_status,SampleStatus::Partial);
    assert_eq!(count(&summary.summary,Count::Calls),None);
    assert_eq!(count(&summary.summary,Count::OperationsVerified),None);
    assert_eq!(count(&summary.summary,Count::InputTokens),None);
    assert_eq!(store.status(NOW+40_000).await.unwrap().unassociated_records.as_deref(),Some("1"));

    let other=Store::new(store.db.clone(),store.manager,"replacement-writer".into());
    other.persist(&[bound_tool(alias)],NOW+HOUR).await.unwrap();
    assert_eq!(other.aggregate(NOW+HOUR).await.unwrap().applied,2);
    assert!(other.unassociated(&unassociated_query(NOW,NOW+2*HOUR,50),NOW+2*HOUR).await.unwrap().records.is_empty());
    let original=other.overview(&query(NOW,NOW+HOUR),NOW+2*HOUR).await.unwrap();
    assert_eq!(count(&original.summary,Count::OperationsVerified),Some("1"));
    assert_eq!(original.coverage.sample_status,SampleStatus::Complete);
    let received=other.overview(&query(NOW+HOUR,NOW+2*HOUR),NOW+2*HOUR).await.unwrap();
    assert_eq!(count(&received.summary,Count::OperationsVerified),None);
    other.persist(std::slice::from_ref(&fact),NOW+2*HOUR).await.unwrap();
    assert_eq!(other.aggregate(NOW+2*HOUR).await.unwrap().applied,0);
    let raw=event::Entity::find().filter(event::Column::EventId.eq(&fact.event_id)).one(&other.db).await.unwrap().unwrap();
    assert!(raw.applied && !raw.association_pending && raw.discarded_reason.is_none());
    assert_resource_accounting(&other).await;
}

#[tokio::test]
async fn pending_association_survives_normal_raw_retention_without_model_or_business_history_reads() {
    let store=fixture_store(NOW-DAY).await;
    let alias=ObservationAlias::permission_request("original-run","original-permission").unwrap();
    let fact=ObservationEvent::deferred_tool(alias.clone(),ObservationPhase::Completed,1,NOW+100,
        BTreeMap::new(),PermissionOutcome::Denied,None,InputIssue::None);
    store.persist(std::slice::from_ref(&fact),NOW+100).await.unwrap();
    store.aggregate(NOW+101).await.unwrap();
    store.cleanup(NOW+DAY+1_000).await.unwrap();
    let retained=event::Entity::find().filter(event::Column::EventId.eq(&fact.event_id)).one(&store.db).await.unwrap().unwrap();
    assert!(retained.applied && retained.association_pending && retained.discarded_reason.is_none());
    store.persist(&[bound_tool(alias)],NOW+DAY+2_000).await.unwrap();
    assert_eq!(store.aggregate(NOW+DAY+2_000).await.unwrap().discarded,0);
    let known=detail::Entity::find_by_id("original-call.tool.0").one(&store.db).await.unwrap().unwrap();
    let known:ObservationEvent=decode(&known.snapshot_json).unwrap();
    assert_eq!(record(&known,false).permission.as_deref(),Some("denied"));
    assert_eq!(known.attribution.model_id,"model-1");
    assert_eq!(count(&store.overview(&query(NOW,NOW+HOUR),NOW+DAY+3_000).await.unwrap().summary,Count::PermissionDenied),Some("1"));
    assert_eq!(detail::Entity::find().filter(detail::Column::Kind.eq("unassociated")).count(&store.db).await.unwrap(),0);
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn expiration_keeps_a_terminal_gap_and_never_recreates_its_operation() {
    let store=fixture_store(NOW-DAY).await;
    let mut settings=store.load_settings().await.unwrap(); settings.mutable_days=1;
    store.save_settings(settings,NOW).await.unwrap().unwrap();
    let alias=ObservationAlias::provider_work("expired-association").unwrap();
    let fact=deferred_operation(alias.clone(),OperationOutcome::Verified);
    store.persist(std::slice::from_ref(&fact),NOW+2_000).await.unwrap();
    store.aggregate(NOW+2_001).await.unwrap();
    store.cleanup(NOW+DAY+3_000).await.unwrap();
    assert!(event::Entity::find().filter(event::Column::EventId.eq(&fact.event_id)).one(&store.db).await.unwrap().is_none());
    let gap=store.unassociated(&unassociated_query(NOW,NOW+HOUR,50),NOW+DAY+4_000).await.unwrap();
    assert_eq!(gap.records.len(),1); assert_eq!(gap.records[0].state,AssociationGapState::OutsideWindow);
    assert!(gap.records[0].original_model.is_none() && gap.records[0].started_at.is_none());
    let mut later=bound_tool(alias); later.started_at_ms=NOW+DAY+4_000; later.occurred_at_ms=NOW+DAY+4_100;
    store.persist(&[later],NOW+DAY+4_100).await.unwrap(); store.aggregate(NOW+DAY+4_100).await.unwrap();
    assert_eq!(detail::Entity::find().filter(detail::Column::Kind.eq("operation")).count(&store.db).await.unwrap(),0);
    assert_eq!(store.unassociated(&unassociated_query(NOW,NOW+HOUR,50),NOW+DAY+4_200).await.unwrap().records[0].state,AssociationGapState::OutsideWindow);
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn unassociated_pagination_is_received_time_based_and_isolated_from_normal_call_details() {
    let store=fixture_store(NOW-DAY).await;
    for index in 0..3 {
        let fact=deferred_operation(ObservationAlias::provider_work(&format!("missing-page-{index}")).unwrap(),OperationOutcome::Unknown);
        store.persist(&[fact],NOW+2_000+index).await.unwrap();
    }
    store.aggregate(NOW+2_010).await.unwrap();
    let filters=unassociated_query(NOW,NOW+HOUR,1);
    let first=store.unassociated(&filters,NOW+3_000).await.unwrap();
    assert_eq!(first.records.len(),1); assert_eq!(first.records[0].received_at,timestamp(NOW+2_002));
    assert_eq!(first.coverage.cohort_basis,"fact_received");
    let mut second_filters=filters.clone(); second_filters.cursor=first.next_cursor;
    let second=store.unassociated(&second_filters,NOW+3_001).await.unwrap();
    assert_eq!(second.records[0].received_at,timestamp(NOW+2_001));
    assert_ne!(first.records[0].id,second.records[0].id);
    second_filters.cursor=second.next_cursor;
    let third=store.unassociated(&second_filters,NOW+3_002).await.unwrap();
    assert_eq!(third.records[0].received_at,timestamp(NOW+2_000)); assert!(third.next_cursor.is_none());
    let mut wrong=unassociated_query(NOW+1,NOW+HOUR,1); wrong.cursor=second_filters.cursor;
    assert!(store.unassociated(&wrong,NOW+3_003).await.is_err());
    assert_eq!(detail::Entity::find().filter(detail::Column::Kind.eq("unassociated")).filter(detail::Column::StartedAtMs.is_null()).count(&store.db).await.unwrap(),3);
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn late_dispatch_restores_the_observed_cohort_after_the_gap_timeout() {
    let store=fixture_store(NOW-DAY).await;
    let alias=ObservationAlias::provider_work("dispatch-observed-later").unwrap();
    store.persist(&[bound_tool(alias.clone())],NOW+100).await.unwrap(); store.aggregate(NOW+100).await.unwrap();
    let completion=operation_result_without_start(alias.clone(),ObservationPhase::Completed,OperationOutcome::Verified,NOW+2*HOUR);
    store.persist(std::slice::from_ref(&completion),NOW+2*HOUR).await.unwrap(); store.aggregate(NOW+2*HOUR).await.unwrap();
    let gaps=store.unassociated(&unassociated_query(NOW+HOUR,NOW+3*HOUR,50),NOW+2*HOUR+40_000).await.unwrap();
    assert_eq!(gaps.records[0].missing,AssociationGapKind::OperationStart);
    assert_eq!(gaps.records[0].state,AssociationGapState::Unavailable);
    assert_eq!(gaps.records[0].original_model.as_ref().unwrap().model_id,"model-1");
    let replacement=Store::new(store.db.clone(),store.manager,"late-dispatch-writer".into());
    replacement.persist(&[operation_dispatch(alias)],NOW+3*HOUR).await.unwrap();
    assert_eq!(replacement.aggregate(NOW+3*HOUR).await.unwrap().applied,2);
    let operation=detail::Entity::find_by_id(&completion.object_id).one(&store.db).await.unwrap().unwrap();
    assert_eq!(operation.started_at_ms,Some(NOW+1_000));
    let original=store.overview(&query(NOW,NOW+HOUR),NOW+4*HOUR).await.unwrap().summary;
    assert_eq!(count(&original,Count::OperationsDispatched),Some("1"));
    assert_eq!(count(&original,Count::OperationsVerified),Some("1"));
    assert_eq!(count(&store.overview(&query(NOW+HOUR,NOW+4*HOUR),NOW+4*HOUR).await.unwrap().summary,Count::OperationsVerified),None);
    assert_eq!(detail::Entity::find().filter(detail::Column::Kind.eq("unassociated")).count(&store.db).await.unwrap(),0);
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn a_trimmed_unassociated_detail_is_not_rebuilt_by_pending_correction_checks() {
    let store=fixture_store(NOW-DAY).await;
    let source=correction_input("original-call",NOW,InputConclusion::Rejected);
    store.persist(&[source],NOW+100).await.unwrap(); store.aggregate(NOW+100).await.unwrap();
    store.persist(&[correction_marker("next",correction_fence(),None)],NOW+500).await.unwrap();
    store.aggregate(NOW+500).await.unwrap();
    let gap=detail::Entity::find().filter(detail::Column::Kind.eq("unassociated")).one(&store.db).await.unwrap().unwrap();
    let txn=store.db.begin().await.unwrap();
    detail::Entity::delete_by_id(&gap.object_id).exec(&txn).await.unwrap();
    let (_,config)=resources::configuration(&txn).await.unwrap();
    resources::release(&txn,&config,StorageKind::Detail,1,gap.storage_bytes).await.unwrap();
    resources::partial(&txn).await.unwrap(); txn.commit().await.unwrap();
    store.aggregate(NOW+2_000).await.unwrap(); store.aggregate(NOW+40_000).await.unwrap();
    assert_eq!(detail::Entity::find().filter(detail::Column::Kind.eq("unassociated")).count(&store.db).await.unwrap(),0);
    assert_eq!(event::Entity::find().filter(event::Column::AssociationPending.eq(true)).count(&store.db).await.unwrap(),1);
    assert_eq!(count(&store.overview(&query(NOW,NOW+HOUR),NOW+HOUR).await.unwrap().summary,Count::CorrectionAwaiting),None);
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn corrupt_gap_projection_cannot_poison_unrelated_current_calls() {
    let store=fixture_store(NOW-DAY).await;
    let alias=ObservationAlias::provider_work("corrupt-gap").unwrap();
    let mut fact=deferred_operation(alias.clone(),OperationOutcome::Verified);
    store.persist(std::slice::from_ref(&fact),NOW+2_000).await.unwrap(); store.aggregate(NOW+2_001).await.unwrap();
    let gap=detail::Entity::find().filter(detail::Column::Kind.eq("unassociated")).one(&store.db).await.unwrap().unwrap();
    let corrupt=format!("x{}",&gap.snapshot_json[1..]);
    assert_eq!(corrupt.len(),gap.snapshot_json.len());
    detail::Entity::update_many().set(detail::ActiveModel { snapshot_json:Set(corrupt),..Default::default() })
        .filter(detail::Column::ObjectId.eq(&gap.object_id)).exec(&store.db).await.unwrap();
    let raw=event::Entity::find().filter(event::Column::EventId.eq(&fact.event_id)).one(&store.db).await.unwrap().unwrap();
    fact.occurred_at_ms=NOW+HOUR;
    let corrupt_clock=encode(&fact).unwrap(); assert_eq!(corrupt_clock.len(),raw.payload_json.len());
    event::Entity::update_many().set(event::ActiveModel { payload_json:Set(corrupt_clock),..Default::default() })
        .filter(event::Column::Id.eq(raw.id)).exec(&store.db).await.unwrap();
    store.persist(&[bound_tool(alias),call_event("unrelated-current-call",NOW,RequestOutcome::Returned)],NOW+3_000).await.unwrap();
    let result=store.aggregate(NOW+3_000).await.unwrap();
    assert_eq!((result.applied,result.discarded),(2,1));
    assert!(detail::Entity::find_by_id("unrelated-current-call").one(&store.db).await.unwrap().is_some());
    let raw=event::Entity::find_by_id(raw.id).one(&store.db).await.unwrap().unwrap();
    assert!(raw.applied && !raw.association_pending);
    assert_eq!(raw.discarded_reason.as_deref(),Some("clock_skew"));
    let overview=store.overview(&query(NOW,NOW+HOUR),NOW+HOUR).await.unwrap();
    assert_eq!(count(&overview.summary,Count::Returned),Some("1"));
    assert_eq!(count(&overview.summary,Count::OperationsVerified),None);
    assert_eq!(store.aggregate(NOW+3_001).await.unwrap().discarded,0);
    assert_resource_accounting(&store).await;
}
