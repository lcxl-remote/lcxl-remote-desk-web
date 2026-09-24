//! The separate OSS reviewer dial. Only a claimed, exactly authorized prompt
//! reaches this model; configuration is pinned and checked again at send time.

use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::approval_review::{
    ApprovalReviewCandidate, ApprovalReviewDecision, reviewer_model_decision,
    reviewer_model_request,
};
use desk_diagnose_core::chat::TokenUsage;
use desk_diagnose_core::seam::{ModelSeam, NullTurnSink};
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
    let request = reviewer_model_request(candidate, claim.authorized.prompt.clone())
        .map_err(|_| reviewer_unavailable())?;
    let seam = SignalModelSeam::from_approval_config(&config)?.with_context_db(db.clone());
    let turn = seam.call(request, &mut NullTurnSink).await?;
    let decision = reviewer_model_decision(candidate, &turn).ok();
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
