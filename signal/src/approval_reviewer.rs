//! The separate OSS reviewer dial. Only a claimed, exactly authorized prompt
//! reaches this model; configuration is pinned and checked again at send time.

use crate::config::connection::DatabaseConnection;
use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::approval_review::{
    ApprovalReviewCandidate, ApprovalReviewDecision, reviewer_model_decision_observed,
};
use desk_diagnose_core::chat::TokenUsage;
use desk_diagnose_core::seam::ModelSeam;

use crate::agent_approval_store::ClaimedPermissionReview;
use crate::model_dial::SignalModelSeam;

pub struct ReviewerResponse {
    pub format_issue: Option<desk_diagnose_core::approval_review::ReviewFormatIssue>,
    pub response_complete: bool,
    pub decision: Option<ApprovalReviewDecision>,
    pub usage: TokenUsage,
    pub stop_reason: String,
    pub failure_reason: Option<String>,
}

/// A malformed or incomplete model answer cannot become a grant. Its reported
/// usage still reaches settlement; transport errors leave usage unknown and
/// therefore consume the full reservation.
pub async fn call_claimed_permission_review(
    db: &DatabaseConnection,
    candidate: &ApprovalReviewCandidate,
    claim: &ClaimedPermissionReview,
) -> Result<ReviewerResponse, AgentError> {
    let mut sink = crate::approval_trace::Store::new(db.clone())
        .capture(candidate, claim)
        .await;
    sink.tracking = Some(claim.statistics.clone());
    sink.publish = Some(std::sync::Arc::new(crate::model_metrics::runtime::submit));
    sink.event("preparation");
    let mut current = claim.clone();
    let mut combined_usage = None;
    let result = loop {
        sink.begin_attempt(current.call_authority.attempt.index);
        let mut response = Box::pin(call_claimed_permission_review_inner(
            db, candidate, &current, &mut sink,
        ))
        .await;
        if let Ok(value) = &mut response {
            sink.model_finished();
            sink.text(
                "usage",
                &serde_json::to_string(&value.usage).unwrap_or_default(),
            );
            combined_usage = Some(match combined_usage {
                None => value.usage,
                Some(previous) => {
                    desk_diagnose_core::approval_usage::sum_review_usage(previous, value.usage)
                }
            });
            if crate::approval_attempt::record_response(
                db,
                &current.call_authority,
                value.response_complete,
                value.format_issue,
            )
            .await
            .is_err()
            {
                break Err(AgentError {
                    kind: AgentErrorKind::Internal,
                    message: "Approval response facts could not be recorded".into(),
                    ..reviewer_unavailable()
                });
            }
            if value.format_issue.is_some() {
                value.failure_reason = Some(format!(
                    "format_retries_exhausted:{}",
                    current.call_authority.attempt.index
                ));
            }
            if let Some(issue) = value.format_issue
                && current.call_authority.attempt.next(issue).is_some()
            {
                sink.event("format_retry_requested");
                match Box::pin(crate::approval_retry::advance(
                    db, candidate, &current, issue,
                ))
                .await
                {
                    Ok(Some(next)) => {
                        current = next;
                        continue;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        sink.event("format_retry_unavailable");
                        value.failure_reason = Some(
                            if error
                                == sea_orm::DbErr::Custom("approval review budget exhausted".into())
                            {
                                "retry_budget"
                            } else {
                                "retry_unavailable"
                            }
                            .into(),
                        );
                    }
                }
            }
        }
        break response;
    };
    match &result {
        Ok(response) => {
            sink.model_finished();
            if let Some(decision) = &response.decision
                && let Ok(value) = serde_json::to_string(decision)
            {
                sink.text("decision", &value);
            }
            sink.event(if response.decision.is_some() {
                "decision_parsed"
            } else {
                "decision_invalid"
            });
            sink.complete(
                Some(response.stop_reason.clone()),
                serde_json::to_string(&combined_usage.unwrap_or(response.usage)).ok(),
                response.failure_reason.clone(),
            );
        }
        Err(error) => {
            let reason = if sink.stage() == "non_stream_response" {
                "stream"
            } else {
                match error.kind {
                    AgentErrorKind::Timeout => "timeout",
                    AgentErrorKind::Cancelled => "cancelled",
                    AgentErrorKind::TransportError => "transport",
                    AgentErrorKind::ModelUnavailable => "provider",
                    AgentErrorKind::ModelRejected => {
                        if sink.model_started() {
                            "provider"
                        } else {
                            "configuration"
                        }
                    }
                    AgentErrorKind::ContentBlocked | AgentErrorKind::RiskBlocked => "policy",
                    AgentErrorKind::PermissionDenied => {
                        if sink.stage() == "configuration" {
                            "configuration"
                        } else {
                            "revalidation"
                        }
                    }
                    _ => "processing",
                }
            };
            sink.model_finished();
            sink.event("review_failed");
            sink.complete(None, None, Some(reason.into()));
        }
    }
    result
}

async fn call_claimed_permission_review_inner(
    db: &DatabaseConnection,
    candidate: &ApprovalReviewCandidate,
    claim: &ClaimedPermissionReview,
    sink: &mut desk_signal_facade::service::approval_trace::ApprovalTraceSink,
) -> Result<ReviewerResponse, AgentError> {
    let now_unix_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
    if claim.candidate_id != candidate.candidate_id
        || claim.call_authority.lease_deadline_ms != claim.lease_deadline_unix_ms
        || claim.call_authority.model_config_revision != claim.model_config_revision
        || now_unix_ms >= claim.lease_deadline_unix_ms
        || now_unix_ms >= candidate.expires_at_unix_ms
    {
        return Err(reviewer_unavailable());
    }
    // Retry only a pre-dial configuration read. Once provider I/O starts its
    // outcome may be unknown. Each correction owns a new, independently fenced attempt.
    sink.event("configuration");
    let config = match crate::approval_model_provider::load(db).await {
        Ok(config) => config,
        Err(_) => {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            crate::approval_model_provider::load(db)
                .await
                .map_err(|_| reviewer_unavailable())?
        }
    };
    if config.unavailable_reason().is_some()
        || u64::try_from(config.configuration_revision).ok() != Some(claim.model_config_revision)
        || config.destination_identity().ok().as_ref() != Some(&claim.model_destination)
    {
        return Err(reviewer_unavailable());
    }
    let send_at = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(u64::MAX);
    if send_at >= claim.lease_deadline_unix_ms || send_at >= candidate.expires_at_unix_ms {
        return Err(reviewer_unavailable());
    }
    let mut request = claim
        .authorized
        .model_request_for_attempt(candidate, &claim.call_authority.attempt)
        .map_err(|_| reviewer_unavailable())?;
    request.delegation_call = claim.delegation_call.clone();
    request.validate_delegation_call().inspect_err(|error| {
        log::warn!(
            "[approval] reviewer admission rejected: candidate={}, stage=delegation_request, error_kind={:?}",
            candidate.candidate_id,
            error.kind,
        );
    })?;
    if desk_diagnose_core::approval_review::reviewer_request_digest(&request)
        .ok()
        .as_ref()
        != Some(&claim.call_authority.request_sha256)
        || claim.call_authority.candidate_id != candidate.candidate_id
        || claim.call_authority.lease_owner != claim.lease_owner
        || claim.call_authority.lease_epoch != claim.lease_epoch
        || claim.call_authority.model_destination != claim.model_destination
    {
        return Err(reviewer_unavailable());
    }
    let seam = SignalModelSeam::from_approval_config(&config)?.with_context_db(db.clone());
    sink.protocol(
        serde_json::to_value(config.gateway.wire_protocol)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default(),
    );
    sink.event("admission");
    let permit = seam.acquire_admission().await?;
    let admitted_at = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(u64::MAX);
    if admitted_at >= claim.lease_deadline_unix_ms || admitted_at >= candidate.expires_at_unix_ms {
        return Err(reviewer_unavailable());
    }
    let gate = crate::ai_assistant_gate::global_ai_assistant_gate();
    let settings = gate.snapshot();
    if !settings.enabled {
        return Err(reviewer_unavailable());
    }
    let call_id = claim
        .call_authority
        .attempt
        .call_id(&candidate.candidate_id);
    let receipt_id = format!("approval-call:{call_id}");
    let export_id = format!("approval-export:{call_id}");
    let txn = crate::db::begin_write(db, crate::entity::agent_session::Entity)
        .await
        .map_err(|_| reviewer_unavailable())?;
    seam.validate_current_on(&txn)
        .await
        .map_err(|_| reviewer_unavailable())?;
    if let Some(reservation) = &claim.delegation_call {
        crate::agent_subagent_store::SubAgentStore::link_model_receipt_on(
            &txn,
            reservation,
            &receipt_id,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .map_err(|_| reviewer_unavailable())?;
    } else {
        if claim.call_authority.group_id.is_some() {
            return Err(reviewer_unavailable());
        }
        crate::agent_subagent_store::mark_review_dispatch_on(
            &txn,
            &claim.call_authority,
            "oss_model_egress",
            &receipt_id,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .map_err(|_| reviewer_unavailable())?;
    }
    crate::model_egress_store::SignalModelEgressStore::record_dispatch_intent_on(
        &txn,
        receipt_id.clone(),
        export_id.clone(),
        1,
        &claim.authorized.prompt_audit,
        std::slice::from_ref(&claim.authorized.prompt_envelope),
    )
    .await
    .map_err(|_| reviewer_unavailable())?;
    if gate.snapshot() != settings || seam.dispatch_cancelled() {
        return Err(reviewer_unavailable());
    }
    txn.commit().await.map_err(|_| reviewer_unavailable())?;
    let store = crate::model_egress_store::SignalModelEgressStore::new(db.clone());
    request.observation = seam.observation_context(
        request.use_case,
        desk_diagnose_core::model_observability::Origin::System,
    );
    if let Some(observation) = &request.observation {
        sink.protocol(
            serde_json::to_value(observation.attribution.protocol)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
        );
    }
    sink.event("provider_admission");
    if let Some(context) = &request.observation
        && let Ok(mut tracking) = claim.statistics.try_lock()
    {
        tracking.attribution = context.attribution.clone();
    }
    let observation = request.observation.clone();
    let turn = match seam.call_admitted(request, sink, permit).await {
        Ok(turn) => turn,
        Err(error) => {
            if let Err(audit_error) = store.mark_failed(&receipt_id).await {
                log::warn!(
                    "[approval] failed to close reviewer receipt {receipt_id}: {audit_error}"
                );
            }
            return Err(error);
        }
    };
    store
        .record_terminal_usage(
            &receipt_id,
            &turn.usage,
            turn.provider_meta.context_observation.as_ref(),
        )
        .await
        .map_err(|_| reviewer_unavailable())?;
    if let Some(observation) = &observation {
        observation.metered(chrono::Utc::now().timestamp_millis());
    }
    let decision = reviewer_model_decision_observed(
        candidate,
        &turn,
        observation.as_ref(),
        chrono::Utc::now().timestamp_millis(),
    )
    .map_err(|error| {
        log::warn!(
            "[approval] reviewer answer rejected: candidate={}, error={error:?}, stop_reason={:?}, text_bytes={}, tool_calls={}",
            candidate.candidate_id,
            turn.stop_reason,
            turn.text.len(),
            turn.tool_calls.len(),
        );
    })
    .ok();
    let mut format_issue =
        match desk_diagnose_core::approval_review::reviewer_output_diagnosed(candidate, &turn) {
            Err(desk_diagnose_core::approval_review::ReviewOutputIssue::Format(issue)) => {
                Some(issue)
            }
            _ => None,
        };
    let output_policy = desk_diagnose_core::model_egress::ModelEgressPolicy {
        destination: claim.model_destination.clone(),
        selected_source_tools: Default::default(),
        export_authorization_id: export_id,
        now_unix_ms: u64::try_from(chrono::Utc::now().timestamp_millis())
            .map_err(|_| reviewer_unavailable())?,
        byte_cap: desk_diagnose_core::approval_review::MAX_APPROVAL_CONTEXT_BYTES,
        permission_resume: false,
    };
    let decision = match output_policy.derive_model_output_envelope(
        &turn,
        std::slice::from_ref(&claim.authorized.prompt_envelope),
    ) {
        Ok(output) => {
            store
                .mark_succeeded(&receipt_id, &output)
                .await
                .map_err(|_| reviewer_unavailable())?;
            decision
        }
        Err(_) => {
            log::warn!(
                "[approval] reviewer output audit rejected: candidate={}, stop_reason={:?}, text_bytes={}, tool_calls={}",
                candidate.candidate_id,
                turn.stop_reason,
                turn.text.len(),
                turn.tool_calls.len(),
            );
            format_issue = None;
            // Preserve an incomplete, empty or malformed answer's classification.
            if let Some(observation) = &observation
                && decision.is_some()
            {
                observation.output(
                    desk_diagnose_core::model_observability::OutputOutcome::PolicyRejected,
                    chrono::Utc::now().timestamp_millis(),
                );
            }
            store
                .mark_failed(&receipt_id)
                .await
                .map_err(|_| reviewer_unavailable())?;
            None
        }
    };
    Ok(ReviewerResponse {
        format_issue,
        response_complete: turn.stop_reason == desk_diagnose_core::chat::StopReason::EndTurn
            && turn.tool_calls.is_empty(),
        decision: decision.clone(),
        usage: turn.usage,
        stop_reason: format!("{:?}", turn.stop_reason),
        failure_reason: decision.is_none().then(|| {
            if turn.stop_reason == desk_diagnose_core::chat::StopReason::MaxTokens {
                "output_truncated"
            } else {
                "invalid_decision"
            }
            .to_string()
        }),
    })
}

fn reviewer_unavailable() -> AgentError {
    AgentError {
        kind: AgentErrorKind::PermissionDenied,
        message: "The independent approval model or its review lease is no longer current".into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}
