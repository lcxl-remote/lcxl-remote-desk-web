// Included by the two disposable capacity runners; uses each real Store.

use std::{collections::BTreeMap,time::Instant};
use desk_diagnose_core::model_observability::*;
use desk_signal_facade::{model::model_metrics::MetricsQuery,service::model_metrics::{ResolvedQuery,timestamp}};
use sea_orm::{ConnectionTrait,DbBackend,Statement};
use super::Store;

type Failure=Box<dyn std::error::Error>;

pub fn sample_count() -> Result<usize,Failure> {
    let count=std::env::var("LRDM_METRICS_CAPACITY_SAMPLES").unwrap_or_else(|_|"1000000".into()).parse::<usize>()?;
    if !(1_000..=5_000_000).contains(&count) || !count.is_multiple_of(100) {
        return Err("sample count must be a multiple of 100 between 1000 and 5000000".into());
    }
    Ok(count)
}

fn sample(index:usize,base:i64,manager:bool) -> ObservationEvent {
    let call_index=index/2;
    let call_id=format!("capacity-call-{call_index}");
    let tool=index%2==1;
    let id=if tool { format!("{call_id}.tool.0") } else { call_id.clone() };
    let started=base+call_index as i64*2_000;
    let rejected=call_index.is_multiple_of(10);
    let schema_rejected = rejected && call_index.is_multiple_of(20);
    ObservationEvent {
        schema_version:EVENT_SCHEMA_VERSION,event_id:format!("{id}.terminal"),object_id:id,
        call_id:tool.then_some(call_id),phase:if tool { ObservationPhase::Stage } else { ObservationPhase::Terminal },
        sequence:0,started_at_ms:started,occurred_at_ms:started+100,relation:None,
        attribution:Attribution {
            provider_id:format!("capacity-provider-{}",call_index%4),model_id:format!("capacity-model-{}",call_index%32),
            model_name:format!("Capacity model {}",call_index%32),configuration_revision:format!("{}",call_index%8+1),
            contract_revision:"1".into(),purpose:Purpose::Agent,surface:Surface::Assistant,
            origin:if call_index.is_multiple_of(3) { Origin::ScheduledTask } else { Origin::User },
            configuration_scope:if manager { ConfigurationScope::Platform } else { ConfigurationScope::Local },
            protocol:Protocol::OpenAiChatCompletions,
        },
        payload:if tool { ObservationPayload::Tool(ToolSnapshot {
            ordinal:0,tool_key:format!("capacity_tool_{}",call_index%8),
            stages:BTreeMap::from([(Stage::Json,if rejected && !schema_rejected { StageOutcome::Failed } else { StageOutcome::Passed }),
                (Stage::Schema,if schema_rejected { StageOutcome::Failed } else if rejected { StageOutcome::NotReached } else { StageOutcome::Passed }),
                (Stage::Reference,StageOutcome::NotApplicable),(Stage::Preflight,if rejected { StageOutcome::NotReached } else { StageOutcome::Passed })]),
            conclusion:if rejected { InputConclusion::Rejected } else { InputConclusion::Accepted },
            issue:if schema_rejected { InputIssue::Type } else if rejected { InputIssue::InvalidJson } else { InputIssue::None },schema_path:schema_rejected.then(|| "$.items[].count".into()),
            permission:PermissionOutcome::NotReached,correction_of:None,correction_status:CorrectionStatus::Uncorrelated,
            correction_input:None,argument_bytes:32,stage_duration_ms:Some(1),
        }) } else { ObservationPayload::Call(CallSnapshot {
            outcome:if call_index.is_multiple_of(20) { RequestOutcome::HttpError } else { RequestOutcome::Returned },
            output:if call_index.is_multiple_of(20) { OutputOutcome::NotEvaluated } else { OutputOutcome::Accepted },
            timing:Timing { duration_ms:Some(if call_index.is_multiple_of(100) { 6_000 } else { 100+(call_index%100) as u64 }),first_content_ms:Some(30),..Default::default() },
            generated_tool_count:Some(1),..Default::default()
        }) },
    }
}

fn distribution(values:&mut [u64]) -> serde_json::Value {
    values.sort_unstable();
    let percentile=|percent:usize|values.get(values.len().saturating_sub(1)*percent/100).copied();
    serde_json::json!({"samples":values.len(),"p50_ms":percentile(50),"p95_ms":percentile(95),"p99_ms":percentile(99),"max_ms":values.last()})
}

fn elapsed(start:Instant) -> u64 { start.elapsed().as_millis().min(u64::MAX as u128) as u64 }

pub async fn run(store:&Store,count:usize) -> Result<serde_json::Value,Failure> {
    // Advance a reproducible logical clock across twelve days at the million
    // sample default. This exercises retention and budget pressure together.
    let base=chrono::Utc::now().timestamp_millis()-count as i64*1_000-86_400_000;
    store.initialize_settings(base-3_600_000).await?;
    let defaults=store.load_settings().await?;
    let started=Instant::now();
    let mut persist_ms=Vec::with_capacity(count/100);
    let mut aggregate_ms=Vec::with_capacity(count/100);
    let mut lag_ms=Vec::with_capacity(count/100);
    let mut persist_discarded=0u64; let mut projection_discarded=0u64; let mut applied=0u64; let mut outside=0u64;
    let mut now=base;
    for offset in (0..count).step_by(100) {
        let events:Vec<_>=(offset..offset+100).map(|index|sample(index,base,store.manager)).collect();
        now=events.last().ok_or("empty capacity batch")?.occurred_at_ms+100;
        // Use the production cleanup policy, without truncating or importing.
        store.cleanup(now).await?;
        let batch_started=Instant::now();
        let before=Instant::now(); persist_discarded+=u64::from(store.persist(&events,now).await?); persist_ms.push(elapsed(before));
        let before=Instant::now(); let result=store.aggregate(now).await?; aggregate_ms.push(elapsed(before));
        applied+=u64::from(result.applied); projection_discarded+=u64::from(result.discarded); outside+=u64::from(result.outside_window);
        lag_ms.push(elapsed(batch_started));
        if (offset+100).is_multiple_of(10_000) {
            eprintln!("model_metrics_capacity samples={} applied={} discarded={} elapsed_s={}",offset+100,applied,persist_discarded+projection_discarded,started.elapsed().as_secs());
        }
    }
    let elapsed_ms=elapsed(started);
    store.cleanup(now).await?;
    let query=ResolvedQuery::resolve(&MetricsQuery { from:Some(timestamp(base-3_600_000)),to:Some(timestamp(now+1_000)),
        limit:Some(200),..Default::default() },now+1_000).map_err(|reason|reason.to_string())?;
    let before=Instant::now(); let overview=store.overview(&query,now+1_000).await?; let overview_ms=elapsed(before);
    let before=Instant::now(); let page=store.calls(&query,now+1_000).await?; let page_ms=elapsed(before);
    let before=Instant::now(); let groups=store.groups(&query,false,now+1_000).await?; let groups_ms=elapsed(before);
    let status=store.status(now+1_000).await?;
    let mut scan_queries = vec![("page", query.clone())];
    let mut errors = query.clone(); errors.calls.outcome=Some("request_error".into()); scan_queries.push(("request_errors",errors));
    let mut slow = query.clone(); slow.calls.min_duration_ms=Some(5_000); scan_queries.push(("slow_calls",slow));
    let mut rejected = query.clone(); rejected.calls.kind=Some(desk_signal_facade::model::model_metrics::MetricRecordKind::Tool); rejected.calls.outcome=Some("rejected".into()); scan_queries.push(("rejected_inputs",rejected));
    let mut plans=Vec::new(); let mut record_query_ms=BTreeMap::new();
    for (name, selection) in scan_queries {
        let before=Instant::now(); let result=store.calls(&selection,now+1_000).await?;
        record_query_ms.insert(name,serde_json::json!({"elapsed_ms":elapsed(before),"rows":result.records.len()}));
        let page_statement = store.call_page_statement(&selection, now+1_000)?;
        let prefix = match store.db.get_database_backend() { DbBackend::Postgres => "EXPLAIN (FORMAT JSON) ", _ => "EXPLAIN QUERY PLAN " };
        let statement = Statement { sql: format!("{prefix}{}", page_statement.sql), ..page_statement };
        let rows=store.db.query_all_raw(statement).await?;
        let mut selection_plans=Vec::new();
        for row in rows {
            selection_plans.push(if store.manager { row.try_get::<serde_json::Value>("","QUERY PLAN")? }
                else { serde_json::Value::String(row.try_get::<String>("","detail")?) });
        }
        plans.push(serde_json::json!({"selection":name,"plans":selection_plans}));
    }
    Ok(serde_json::json!({
        "role":if store.manager {"manager"} else {"oss"},"schema_version":EVENT_SCHEMA_VERSION,
        "samples_generated":count,"samples_written":count as u64-persist_discarded,
        "million_sample_scope":count as u64-persist_discarded>=1_000_000,"samples_applied":applied,
        "persist_discarded":persist_discarded,"projection_discarded":projection_discarded,"outside_window":outside,"elapsed_ms":elapsed_ms,
        "samples_per_second":if elapsed_ms>0 {Some(count as f64*1_000.0/elapsed_ms as f64)} else {None},
        "persist":distribution(&mut persist_ms),"aggregate":distribution(&mut aggregate_ms),
        "storage_batch_end_to_end":distribution(&mut lag_ms),"storage_batch_within_five_seconds":lag_ms.iter().all(|value|*value<=5_000),
        "queue_to_watermark_evaluated":false,
        "query_ms":{"overview":overview_ms,"page":page_ms,"models":groups_ms},"record_queries":record_query_ms,
        "scan_plan_scope":"production_call_page_all_columns_and_filters","page_records":page.records.len(),"model_groups":groups.groups.len(),"scan_plans":plans,
        "settings":defaults,"storage":status.storage,"coverage":overview.coverage,"summary":overview.summary,
        "logical_from":timestamp(base),"logical_to":timestamp(now),
    }))
}
