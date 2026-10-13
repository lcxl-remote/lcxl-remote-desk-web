// The same fixture runs against OSS and Manager storage implementations.
#[tokio::test]
async fn approval_rates_cohort_dedup_and_private_facts() {
    use desk_diagnose_core::approval_observation::{ReviewReason, ReviewTracking};
    use desk_signal_facade::model::approval_trace::ApprovalMetricsQuery;
    let store = fixture_store(NOW - DAY).await;
    let queued = |index: usize| {
        let conversation = format!("conversation-{index}");
        ReviewTracking::queued(
            if index.is_multiple_of(2) {
                "owner-a"
            } else {
                "owner-b"
            },
            "device",
            &conversation,
            if index < 40 { &conversation } else { "root" },
            "delegation",
            1,
            "permission_item",
            &index.to_string(),
            NOW,
        )
    };
    let mut events = Vec::new();
    for i in 0..100 {
        let mut tracking = queued(i);
        tracking.snapshot.provider_started_at_ms = Some(NOW + 3_000);
        let (status, reason) = match i {
            0..60 => ("approved", None),
            60..80 => ("denied", None),
            80..90 => ("unavailable", Some(ReviewReason::InvalidDecision)),
            90..95 => ("unavailable", Some(ReviewReason::Revoked)),
            95..98 => ("unavailable", Some(ReviewReason::Policy)),
            _ => ("reviewing", None),
        };
        tracking = tracking.settled(status, reason, NOW + 8_000);
        events.push(tracking.event(
            if i < 98 {
                ObservationPhase::Completed
            } else {
                ObservationPhase::Started
            },
            if i < 98 { 2 } else { 0 },
            NOW + 8_000,
        ));
    }
    assert_eq!(store.persist(&events, NOW + 8_000).await.unwrap(), 0);
    for _ in 0..4 {
        store.aggregate(NOW + 8_000).await.unwrap();
    }
    assert_eq!(store.persist(&events, NOW + 9_000).await.unwrap(), 0);
    let filters = ApprovalMetricsQuery {
        from: Some(timestamp(NOW - 1)),
        to: Some(timestamp(NOW + 1)),
        granularity: Some("hour".into()),
        model_id: None,
        provider_id: None,
        source_kind: None,
        session_kind: None,
        reason: None,
        outcome: None,
    };
    let peer = super::Store::new(store.db.clone(), store.manager, "second-node".into());
    assert_eq!(peer.persist(&events, NOW + 20_000).await.unwrap(), 0);
    peer.aggregate(NOW + 20_000).await.unwrap();
    let stats = peer
        .approval_overview(&filters, NOW + 21_000)
        .await
        .unwrap();
    assert_eq!(stats.rates.denominator, "90");
    assert_eq!(stats.rates.approved, "60");
    assert_eq!(stats.rates.denied, "20");
    assert_eq!(stats.rates.fault, "10");
    assert!((stats.rates.approval_rate.unwrap() - 2.0 / 3.0).abs() < 1e-10);
    assert_eq!(stats.waiting.count, "98");
    assert_eq!(stats.waiting.average_ms, Some(8_000.0));
    assert_eq!(stats.before_model.average_ms, Some(3_000.0));
    assert_eq!(stats.pending_age.count, "2");
    assert_eq!(stats.session_kinds[0].count, "40");
    assert_eq!(stats.session_kinds[1].count, "60");
    assert!(
        store
            .calls(&query(NOW - HOUR, NOW + HOUR), NOW + 11_000)
            .await
            .unwrap()
            .records
            .is_empty()
    );
    assert!(
        store
            .call_detail(&events[0].object_id)
            .await
            .unwrap()
            .is_none()
    );
    let public = serde_json::to_string(&stats).unwrap();
    assert!(!public.contains("owner-a"));
    assert!(!public.contains("conversation-"));
    assert!(!public.contains("approval."));
    let mut empty = filters.clone();
    empty.from = Some(timestamp(NOW + 1));
    empty.to = Some(timestamp(NOW + 2));
    let empty = store.approval_overview(&empty, NOW + 10_000).await.unwrap();
    assert_eq!(empty.rates.denominator, "0");
    assert_eq!(empty.rates.approval_rate, None);
    assert_resource_accounting(&store).await;
}

#[tokio::test]
async fn approval_transition_replaces_pending_and_unbound_contributions() {
    use desk_diagnose_core::approval_observation::ReviewTracking;
    use desk_signal_facade::model::approval_trace::ApprovalMetricsQuery;
    let store = fixture_store(NOW - DAY).await;
    let mut tracking = ReviewTracking::queued(
        "owner",
        "device",
        "child",
        "root",
        "delegation",
        1,
        "concrete_call",
        "call",
        NOW,
    );
    let started = tracking.event(ObservationPhase::Started, 0, NOW);
    store.persist(&[started], NOW).await.unwrap();
    store.aggregate(NOW).await.unwrap();
    // A scanner can submit the same source repeatedly before a later claim.
    tracking.snapshot.eligible_at_ms = NOW + 1_000;
    let scan = tracking.event(ObservationPhase::Started, 0, NOW + 1_000);
    assert_eq!(store.persist(&[scan], NOW + 1_000).await.unwrap(), 0);
    tracking.attribution = call_event("model", NOW, RequestOutcome::Returned).attribution;
    tracking.attribution.purpose = Purpose::Approval;
    tracking.snapshot.provider_started_at_ms = Some(NOW + 3_000);
    let output = tracking.event(ObservationPhase::Output, 1, NOW + 3_000);
    let final_tracking = tracking.settled("denied", None, NOW + 8_000);
    let end = final_tracking.event(ObservationPhase::Completed, 2, NOW + 8_000);
    store
        .persist(&[output, end.clone()], NOW + 8_000)
        .await
        .unwrap();
    let progress = store.aggregate(NOW + 8_000).await.unwrap();
    assert_eq!(
        progress.discarded,
        0,
        "{:?}",
        event::Entity::find()
            .all(&store.db)
            .await
            .unwrap()
            .iter()
            .map(|v| (&v.event_id, &v.discarded_reason))
            .collect::<Vec<_>>()
    );
    store.persist(&[end], NOW + 20_000).await.unwrap();
    store.aggregate(NOW + 20_000).await.unwrap();
    let filters = ApprovalMetricsQuery {
        from: Some(timestamp(NOW - HOUR)),
        to: Some(timestamp(NOW + HOUR)),
        granularity: Some("hour".into()),
        model_id: None,
        provider_id: None,
        source_kind: None,
        session_kind: None,
        reason: None,
        outcome: None,
    };
    let stats = store
        .approval_overview(&filters, NOW + 21_000)
        .await
        .unwrap();
    assert_eq!(stats.rates.denied, "1");
    assert_eq!(stats.rates.denominator, "1");
    assert_eq!(stats.waiting.average_ms, Some(8_000.0));
    assert_eq!(
        stats
            .counts
            .iter()
            .find(|v| v.kind == "pending")
            .unwrap()
            .count,
        "0"
    );
    let mut model = filters.clone();
    model.model_id = Some("model-1".into());
    assert_eq!(
        store
            .approval_overview(&model, NOW + 21_000)
            .await
            .unwrap()
            .rates
            .denied,
        "1"
    );
    assert!(
        store
            .calls(&query(NOW - HOUR, NOW + HOUR), NOW + 21_000)
            .await
            .unwrap()
            .records
            .is_empty()
    );
    assert_resource_accounting(&store).await;
}
