//! The separate OSS reviewer dial. Only a claimed, exactly authorized prompt
//! reaches this model; configuration is pinned and checked again at send time.

use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::approval_review::{
    ApprovalReviewCandidate, ApprovalReviewDecision, reviewer_model_decision,
};
use desk_diagnose_core::chat::TokenUsage;
use desk_diagnose_core::seam::NullTurnSink;
use sea_orm::DatabaseConnection;

use crate::agent_approval_store::ClaimedPermissionReview;
use crate::model_dial::SignalModelSeam;

pub struct ReviewerResponse {
    pub decision: Option<ApprovalReviewDecision>,
    pub usage: TokenUsage,
}

/// A malformed or incomplete model answer cannot become a grant. Its reported
/// usage still reaches settlement; transport errors leave usage unknown and
/// therefore consume the full reservation.
pub async fn call_claimed_permission_review(
    db: &DatabaseConnection,
    candidate: &ApprovalReviewCandidate,
    claim: &ClaimedPermissionReview,
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
    // outcome may be unknown, so the claimed candidate must never be redialed.
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
        .model_request(candidate)
        .map_err(|_| reviewer_unavailable())?;
    request.delegation_call = claim.delegation_call.clone();
    request.validate_delegation_call()?;
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
    let receipt_id = format!("approval-call:{}", candidate.candidate_id);
    let export_id = format!("approval-export:{}", candidate.candidate_id);
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
    let turn = match seam.call_admitted(request, &mut NullTurnSink, permit).await {
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
        .record_terminal_usage(&receipt_id, &turn.usage)
        .await
        .map_err(|_| reviewer_unavailable())?;
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
            reviewer_model_decision(candidate, &turn).ok()
        }
        Err(_) => {
            store
                .mark_failed(&receipt_id)
                .await
                .map_err(|_| reviewer_unavailable())?;
            None
        }
    };
    Ok(ReviewerResponse {
        decision,
        usage: turn.usage,
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
