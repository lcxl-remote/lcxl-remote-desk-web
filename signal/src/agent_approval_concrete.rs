//! Independent review of each concrete UI call under an AI-approved scope.
//! The prepared capability work remains the sole dispatch authority.

use crate::capability_grant_store::{
    CAPABILITY_WORK_KIND, CAPABILITY_WORK_PREPARED, GRANT_STATUS_ACTIVE, decode_grant,
    decode_prepared_payload,
};
use crate::entity::{
    agent_action_item, agent_approval_delegation as delegation_row,
    agent_approval_review as review_row, agent_capability_grant, agent_goal_run, agent_session,
};
use desk_agent_protocol::capability_grant::CapabilityGrantIssuer;
use desk_agent_protocol::capability_provider::ProductSurface;
use desk_diagnose_core::approval_egress::{
    AuthorizedApprovalReview, SourceReviewBinding, authorize_source_review_egress,
};
use desk_diagnose_core::approval_review::{
    APPROVAL_REVIEW_LEASE_MS, APPROVAL_REVIEW_SOURCE_CONCRETE_CALL,
    APPROVAL_REVIEW_STATUS_APPROVED, APPROVAL_REVIEW_STATUS_DENIED, APPROVAL_REVIEW_STATUS_EXPIRED,
    APPROVAL_REVIEW_STATUS_REVIEWING, APPROVAL_REVIEW_STATUS_UNAVAILABLE, ApprovalAuthorityFact,
    ApprovalAuthorityStatus, ApprovalInputKind, ApprovalReviewCandidate, ApprovalReviewDecision,
    ApprovalSource, ApprovalVerdict, SourceReviewInput, candidate_context_hmac_sha256,
    concrete_call_review_identity, source_review_candidate,
};
use desk_diagnose_core::chat::ToolCall;
use desk_diagnose_core::provider_registry::ProviderRegistry;
use desk_diagnose_core::session::{AgentSessionSurface, PersistedAgentSession};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DatabaseTransaction, DbErr, EntityTrait,
    QueryFilter, Set,
};
use sha2::{Digest, Sha256};

use crate::agent_approval_store::ClaimedPermissionReview;

#[derive(Debug, Clone)]
pub enum ConcreteReviewResult {
    NotRequired,
    Approved,
    Denied(String),
    Unavailable,
}

struct PreparedReview {
    candidate: ApprovalReviewCandidate,
    authorized: AuthorizedApprovalReview,
    destination: desk_agent_protocol::data_lineage::DestinationIdentity,
    model_config_revision: u64,
    prices: desk_diagnose_core::approval_cost::ApprovalTokenPrices,
    max_context_bytes: u64,
}

enum ClaimState {
    NotRequired,
    Existing(ConcreteReviewResult),
    Claimed(ApprovalReviewCandidate, ClaimedPermissionReview),
}

pub struct ConcreteSubject<'a> {
    pub work_id: i64,
    pub server_call_id: &'a str,
    pub turn_id: &'a str,
    pub conversation_id: &'a str,
    pub actor_id: &'a str,
    pub device_id: &'a str,
    pub canonical_input_json: &'a str,
    pub call: &'a ToolCall,
}

fn invalid() -> DbErr {
    DbErr::Custom("concrete UI review is stale or inconsistent".into())
}

fn as_i64(value: u64) -> Result<i64, DbErr> {
    value.try_into().map_err(|_| invalid())
}

async fn prepare_on(
    txn: &DatabaseTransaction,
    subject: &ConcreteSubject<'_>,
    registry: &ProviderRegistry,
    expires_at: Option<u64>,
    now_unix_ms: u64,
) -> Result<Option<PreparedReview>, DbErr> {
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(subject.conversation_id))
        .filter(agent_session::Column::ActorId.eq(subject.actor_id))
        .filter(agent_session::Column::DeviceId.eq(subject.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let mut session = PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
    session.version = row.version;
    if session.surface != AgentSessionSurface::AiAssistant
        || !session.turn_state.is_active()
        || !session.trigger_origin.allows_delegated_review()
        || session.current_turn_id.as_deref() != Some(subject.turn_id)
        || session.conversation_id != subject.conversation_id
        || session.actor_id != subject.actor_id
        || session.device_id != subject.device_id
    {
        return Err(invalid());
    }
    let work = agent_action_item::Entity::find_by_id(subject.work_id)
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    if work.kind != CAPABILITY_WORK_KIND
        || work.status != CAPABILITY_WORK_PREPARED
        || work.action_request_id != subject.server_call_id
        || work.turn_id != subject.turn_id
        || work.tool_call_id != subject.server_call_id
        || work.conversation_id != subject.conversation_id
        || work.actor_id != subject.actor_id
        || work.target_device_id != subject.device_id
        || work.policy_revision != session.policy_revision
    {
        return Err(invalid());
    }
    let prepared = decode_prepared_payload(&work)?;
    if prepared.call_id != subject.server_call_id
        || prepared.canonical_input_json != subject.canonical_input_json
        || prepared.tool_name != subject.call.name
        || work.draft_hash != prepared.canonical_input_digest_sha256
        || prepared.canonical_input_digest_sha256
            != format!(
                "{:x}",
                Sha256::digest(prepared.canonical_input_json.as_bytes())
            )
        || prepared.input_revision != session.input_revision
        || prepared.input_watermark != session.latest_input_seq
    {
        return Err(invalid());
    }
    let grant_row = agent_capability_grant::Entity::find()
        .filter(agent_capability_grant::Column::GrantId.eq(&prepared.grant_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    if grant_row.status != GRANT_STATUS_ACTIVE {
        return Err(invalid());
    }
    let grant = decode_grant(&grant_row)?;
    if grant.run_id != subject.conversation_id
        || grant.actor_id != subject.actor_id
        || grant.target_device_id != subject.device_id
        || grant.tool_name != subject.call.name
        || grant.provider_id != prepared.provider_id
        || grant.capability_id != prepared.capability_id
    {
        return Err(invalid());
    }
    let CapabilityGrantIssuer::AiApproval(parent) = &grant.issued_by else {
        return Ok(None);
    };
    let expected =
        concrete_call_review_identity(&grant, subject.server_call_id, subject.canonical_input_json)
            .map_err(|_| invalid())?
            .ok_or_else(invalid)?;
    crate::agent_approval_store::current_grant_parent_on(txn, &session, &grant, now_unix_ms)
        .await?;
    let stored = delegation_row::Entity::find()
        .filter(delegation_row::Column::DelegationId.eq(&parent.delegation_id))
        .filter(delegation_row::Column::ConversationId.eq(subject.conversation_id))
        .filter(delegation_row::Column::ActorId.eq(subject.actor_id))
        .filter(delegation_row::Column::DeviceId.eq(subject.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let delegation = crate::agent_approval_store::decode(&stored)?;
    let config = crate::approval_model_provider::load(txn).await?;
    if config.unavailable_reason().is_some() {
        return Err(invalid());
    }
    let model_config_revision =
        u64::try_from(config.configuration_revision).map_err(|_| invalid())?;
    let destination = config.destination_identity().map_err(|_| invalid())?;
    let prices = config.prices.ok_or_else(invalid)?;
    let goal_row = agent_goal_run::Entity::find()
        .filter(agent_goal_run::Column::ConversationId.eq(subject.conversation_id))
        .filter(agent_goal_run::Column::ActorId.eq(subject.actor_id))
        .filter(agent_goal_run::Column::DeviceId.eq(subject.device_id))
        .filter(agent_goal_run::Column::Status.is_not_in(["completed", "failed", "cancelled"]))
        .one(txn)
        .await?;
    let goal = goal_row
        .as_ref()
        .map(crate::agent_goal_store::decode)
        .transpose()?;
    let capability = registry
        .capability_for_tool(&subject.call.name)
        .ok_or_else(invalid)?;
    let provider = registry
        .provider_for_capability(&capability.wire.capability_id)
        .ok_or_else(invalid)?;
    if provider.wire.provider_id != grant.provider_id
        || capability.wire.input_schema_version != grant.tool_schema_version
        || capability.wire.effect != grant.effect
        || !capability
            .wire
            .surfaces
            .contains(&ProductSurface::OssPersonalOwner)
    {
        return Err(invalid());
    }
    let risk =
        desk_diagnose_core::provider_preflight::classify_provider_call(capability, subject.call)
            .map_err(|_| invalid())?;
    if risk > grant.risk_tier {
        return Err(invalid());
    }
    let descriptor_json = serde_json::json!({
        "provider_id": &provider.wire.provider_id,
        "tool_name": &subject.call.name,
        "provider": &provider.wire,
        "capability": &capability.wire,
        "tool_spec": &capability.tool_spec,
        "risk_tier": risk,
    })
    .to_string();
    let expiry = expires_at
        .unwrap_or_else(|| now_unix_ms.saturating_add(5 * 60 * 1_000))
        .min(grant.expires_at_unix_ms);
    if expiry <= now_unix_ms {
        return Err(invalid());
    }
    let source = ApprovalSource::ConcreteCall {
        call_id: subject.server_call_id.to_owned(),
        grant_id: grant.grant_id.clone(),
    };
    let candidate = source_review_candidate(SourceReviewInput {
        session: &session,
        delegation: &delegation,
        goal: goal.as_ref(),
        source: source.clone(),
        tool_name: &subject.call.name,
        descriptor_json: &descriptor_json,
        risk,
        input_kind: ApprovalInputKind::ConcreteCall,
        action_json: &prepared.canonical_input_json,
        expires_at_unix_ms: expiry,
        current_authority: vec![ApprovalAuthorityFact {
            source: source.clone(),
            status: ApprovalAuthorityStatus::ActiveGrant,
            decision_event_id: Some(parent.decision_event_id.clone()),
            active_grant_ids: vec![grant.grant_id.clone()],
            dispatch_ids: vec![],
        }],
        now_unix_ms,
    })
    .map_err(|_| invalid())?;
    if candidate.candidate_id != expected.candidate_id
        || candidate.action_sha256 != expected.action_sha256
    {
        return Err(invalid());
    }
    let authorized = authorize_source_review_egress(
        &candidate,
        &SourceReviewBinding {
            source: &source,
            tool_call_id: &subject.call.id,
            turn_id: subject.turn_id,
            lease_token: session.lease_token,
            tool_name: &subject.call.name,
            action_json: &prepared.canonical_input_json,
            descriptor_json: &descriptor_json,
            risk,
            input_kind: ApprovalInputKind::ConcreteCall,
            input_revision: session.input_revision,
            expires_at_unix_ms: expiry,
        },
        &session,
        goal.as_ref(),
        &delegation,
        &destination,
        now_unix_ms,
    )
    .map_err(|_| invalid())?;
    Ok(Some(PreparedReview {
        candidate,
        authorized,
        destination,
        model_config_revision,
        prices,
        max_context_bytes: u64::try_from(config.gateway.max_context_bytes.ok_or_else(invalid)?)
            .map_err(|_| invalid())?,
    }))
}

async fn claim(
    db: &DatabaseConnection,
    subject: &ConcreteSubject<'_>,
    registry: &ProviderRegistry,
    now_unix_ms: u64,
) -> Result<ClaimState, DbErr> {
    let key = crate::approval_review_secret::load_or_create(db).await?;
    let txn = crate::db::begin_write(db, delegation_row::Entity).await?;
    let Some(initial) = prepare_on(&txn, subject, registry, None, now_unix_ms).await? else {
        return Ok(ClaimState::NotRequired);
    };
    let existing = review_row::Entity::find()
        .filter(review_row::Column::CandidateId.eq(&initial.candidate.candidate_id))
        .one(&txn)
        .await?;
    let prepared = if let Some(row) = &existing {
        prepare_on(
            &txn,
            subject,
            registry,
            Some(u64::try_from(row.expires_at).map_err(|_| invalid())?),
            now_unix_ms,
        )
        .await?
        .ok_or_else(invalid)?
    } else {
        initial
    };
    let candidate = &prepared.candidate;
    let context_hmac = candidate_context_hmac_sha256(&key, candidate).map_err(|_| invalid())?;
    if let Some(row) = existing {
        if row.conversation_id != candidate.conversation_id
            || row.actor_id != candidate.owner_id
            || row.device_id != candidate.device_id
            || row.delegation_id != candidate.delegation_id
            || row.source_kind != APPROVAL_REVIEW_SOURCE_CONCRETE_CALL
            || row.source_id != subject.server_call_id
            || row.action_sha256 != candidate.action_sha256
            || row.context_hmac_sha256 != context_hmac
            || row.expires_at != as_i64(candidate.expires_at_unix_ms)?
        {
            return Err(invalid());
        }
        let result = match row.status.as_str() {
            APPROVAL_REVIEW_STATUS_APPROVED => {
                let decision: ApprovalReviewDecision =
                    serde_json::from_str(row.decision_json.as_deref().ok_or_else(invalid)?)
                        .map_err(|_| invalid())?;
                decision.validate_for(candidate).map_err(|_| invalid())?;
                if decision.verdict != ApprovalVerdict::Approve {
                    return Err(invalid());
                }
                ConcreteReviewResult::Approved
            }
            APPROVAL_REVIEW_STATUS_DENIED => {
                let decision: ApprovalReviewDecision =
                    serde_json::from_str(row.decision_json.as_deref().ok_or_else(invalid)?)
                        .map_err(|_| invalid())?;
                decision.validate_for(candidate).map_err(|_| invalid())?;
                if decision.verdict != ApprovalVerdict::Deny {
                    return Err(invalid());
                }
                ConcreteReviewResult::Denied(decision.reason)
            }
            _ => ConcreteReviewResult::Unavailable,
        };
        txn.commit().await?;
        return Ok(ClaimState::Existing(result));
    }
    let stored = delegation_row::Entity::find()
        .filter(delegation_row::Column::DelegationId.eq(&candidate.delegation_id))
        .filter(delegation_row::Column::ConversationId.eq(&candidate.conversation_id))
        .filter(delegation_row::Column::ActorId.eq(&candidate.owner_id))
        .filter(delegation_row::Column::DeviceId.eq(&candidate.device_id))
        .one(&txn)
        .await?
        .ok_or_else(invalid)?;
    let mut delegation = crate::agent_approval_store::decode(&stored)?;
    let (reserved_tokens, reserved_cost_micros) =
        desk_diagnose_core::approval_cost::reviewer_reservation(
            &prepared.authorized.prompt,
            prepared.prices,
            prepared.max_context_bytes,
        )
        .ok_or_else(invalid)?;
    delegation
        .reserve(reserved_tokens, reserved_cost_micros)
        .map_err(|_| invalid())?;
    let changed = delegation_row::Entity::update_many()
        .col_expr(
            delegation_row::Column::StateJson,
            Expr::value(serde_json::to_string(&delegation).map_err(|_| invalid())?),
        )
        .col_expr(
            delegation_row::Column::Version,
            Expr::value(as_i64(delegation.ledger_version)?),
        )
        .col_expr(
            delegation_row::Column::UpdatedAt,
            Expr::value(as_i64(now_unix_ms)?),
        )
        .filter(delegation_row::Column::Id.eq(stored.id))
        .filter(delegation_row::Column::Version.eq(stored.version))
        .filter(delegation_row::Column::Status.eq("active"))
        .exec(&txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    let lease_owner = format!("oss-ui-review-{}", uuid::Uuid::new_v4());
    let lease_deadline_unix_ms = now_unix_ms
        .saturating_add(APPROVAL_REVIEW_LEASE_MS)
        .min(candidate.expires_at_unix_ms);
    review_row::ActiveModel {
        candidate_id: Set(candidate.candidate_id.clone()),
        conversation_id: Set(candidate.conversation_id.clone()),
        actor_id: Set(candidate.owner_id.clone()),
        device_id: Set(candidate.device_id.clone()),
        delegation_id: Set(candidate.delegation_id.clone()),
        model_config_revision: Set(as_i64(prepared.model_config_revision)?),
        source_kind: Set(APPROVAL_REVIEW_SOURCE_CONCRETE_CALL.into()),
        source_id: Set(subject.server_call_id.to_owned()),
        action_sha256: Set(candidate.action_sha256.clone()),
        context_hmac_sha256: Set(context_hmac),
        decision_json: Set(None),
        status: Set(APPROVAL_REVIEW_STATUS_REVIEWING.into()),
        lease_epoch: Set(1),
        lease_owner: Set(Some(lease_owner.clone())),
        lease_deadline: Set(Some(as_i64(lease_deadline_unix_ms)?)),
        reserved_tokens: Set(as_i64(reserved_tokens)?),
        reserved_cost_micros: Set(as_i64(reserved_cost_micros)?),
        expires_at: Set(as_i64(candidate.expires_at_unix_ms)?),
        created_at: Set(as_i64(now_unix_ms)?),
        updated_at: Set(as_i64(now_unix_ms)?),
        ..Default::default()
    }
    .insert(&txn)
    .await?;
    txn.commit().await?;
    Ok(ClaimState::Claimed(
        prepared.candidate.clone(),
        ClaimedPermissionReview {
            candidate_id: prepared.candidate.candidate_id,
            lease_epoch: 1,
            lease_owner,
            lease_deadline_unix_ms,
            model_destination: prepared.destination,
            model_config_revision: prepared.model_config_revision,
            prices: prepared.prices,
            authorized: prepared.authorized,
        },
    ))
}

#[allow(clippy::too_many_arguments)]
async fn settle(
    db: &DatabaseConnection,
    subject: &ConcreteSubject<'_>,
    registry: &ProviderRegistry,
    candidate: &ApprovalReviewCandidate,
    claim: &ClaimedPermissionReview,
    decision: Option<&ApprovalReviewDecision>,
    actual_tokens: Option<u64>,
    actual_cost_micros: Option<u64>,
    now_unix_ms: u64,
) -> Result<ConcreteReviewResult, DbErr> {
    let key = crate::approval_review_secret::load_or_create(db).await?;
    let context_hmac = candidate_context_hmac_sha256(&key, candidate).map_err(|_| invalid())?;
    let txn = crate::db::begin_write(db, delegation_row::Entity).await?;
    let row = review_row::Entity::find()
        .filter(review_row::Column::CandidateId.eq(&candidate.candidate_id))
        .one(&txn)
        .await?
        .ok_or_else(invalid)?;
    if row.status != APPROVAL_REVIEW_STATUS_REVIEWING
        || row.source_kind != APPROVAL_REVIEW_SOURCE_CONCRETE_CALL
        || row.source_id != subject.server_call_id
        || row.conversation_id != subject.conversation_id
        || row.actor_id != subject.actor_id
        || row.device_id != subject.device_id
        || row.delegation_id != candidate.delegation_id
        || row.action_sha256 != candidate.action_sha256
        || row.context_hmac_sha256 != context_hmac
        || row.lease_owner.as_deref() != Some(claim.lease_owner.as_str())
        || row.lease_epoch != as_i64(claim.lease_epoch)?
        || row.reserved_tokens <= 0
        || row.reserved_cost_micros <= 0
    {
        return Err(invalid());
    }
    let stored = delegation_row::Entity::find()
        .filter(delegation_row::Column::DelegationId.eq(&candidate.delegation_id))
        .filter(delegation_row::Column::ConversationId.eq(subject.conversation_id))
        .filter(delegation_row::Column::ActorId.eq(subject.actor_id))
        .filter(delegation_row::Column::DeviceId.eq(subject.device_id))
        .one(&txn)
        .await?
        .ok_or_else(invalid)?;
    let mut delegation = crate::agent_approval_store::decode(&stored)?;
    let current = prepare_on(
        &txn,
        subject,
        registry,
        Some(candidate.expires_at_unix_ms),
        now_unix_ms,
    )
    .await
    .ok()
    .flatten()
    .is_some_and(|prepared| {
        prepared.candidate == *candidate
            && row.model_config_revision == as_i64(prepared.model_config_revision).unwrap_or(-1)
            && prepared.destination == claim.model_destination
    });
    let expired = now_unix_ms >= candidate.expires_at_unix_ms
        || row
            .lease_deadline
            .is_none_or(|deadline| deadline <= as_i64(now_unix_ms).unwrap_or(i64::MAX));
    let usage_in_bounds = actual_tokens.is_none_or(|used| used <= row.reserved_tokens as u64)
        && actual_cost_micros.is_none_or(|used| used <= row.reserved_cost_micros as u64);
    let valid_decision = decision.is_some_and(|review| review.validate_for(candidate).is_ok());
    let status = if expired {
        APPROVAL_REVIEW_STATUS_EXPIRED
    } else if !current || !usage_in_bounds || !valid_decision {
        APPROVAL_REVIEW_STATUS_UNAVAILABLE
    } else {
        match decision.map(|review| review.verdict) {
            Some(ApprovalVerdict::Approve) => APPROVAL_REVIEW_STATUS_APPROVED,
            Some(ApprovalVerdict::Deny) => APPROVAL_REVIEW_STATUS_DENIED,
            _ => APPROVAL_REVIEW_STATUS_UNAVAILABLE,
        }
    };
    delegation
        .settle(
            row.reserved_tokens as u64,
            row.reserved_cost_micros as u64,
            usage_in_bounds.then_some(actual_tokens).flatten(),
            usage_in_bounds.then_some(actual_cost_micros).flatten(),
        )
        .map_err(|_| invalid())?;
    let changed = delegation_row::Entity::update_many()
        .col_expr(
            delegation_row::Column::StateJson,
            Expr::value(serde_json::to_string(&delegation).map_err(|_| invalid())?),
        )
        .col_expr(
            delegation_row::Column::Version,
            Expr::value(as_i64(delegation.ledger_version)?),
        )
        .col_expr(
            delegation_row::Column::UpdatedAt,
            Expr::value(as_i64(now_unix_ms)?),
        )
        .filter(delegation_row::Column::Id.eq(stored.id))
        .filter(delegation_row::Column::Version.eq(stored.version))
        .exec(&txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    let decision_json = if valid_decision && usage_in_bounds {
        decision
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| invalid())?
    } else {
        None
    };
    let changed = review_row::Entity::update_many()
        .col_expr(review_row::Column::Status, Expr::value(status))
        .col_expr(review_row::Column::DecisionJson, Expr::value(decision_json))
        .col_expr(
            review_row::Column::LeaseOwner,
            Expr::value(Option::<String>::None),
        )
        .col_expr(
            review_row::Column::LeaseDeadline,
            Expr::value(Option::<i64>::None),
        )
        .col_expr(
            review_row::Column::UpdatedAt,
            Expr::value(as_i64(now_unix_ms)?),
        )
        .filter(review_row::Column::Id.eq(row.id))
        .filter(review_row::Column::Status.eq(APPROVAL_REVIEW_STATUS_REVIEWING))
        .filter(review_row::Column::LeaseEpoch.eq(row.lease_epoch))
        .filter(review_row::Column::LeaseOwner.eq(&claim.lease_owner))
        .exec(&txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    txn.commit().await?;
    Ok(match status {
        APPROVAL_REVIEW_STATUS_APPROVED => ConcreteReviewResult::Approved,
        APPROVAL_REVIEW_STATUS_DENIED => ConcreteReviewResult::Denied(
            decision
                .map(|review| review.reason.clone())
                .ok_or_else(invalid)?,
        ),
        _ => ConcreteReviewResult::Unavailable,
    })
}

/// A claimed review is dialed once and settled before the source work can
/// record any dispatch intent. Crashes leave the claim for lease expiry, not
/// another model call with the same action identity.
pub async fn review_prepared_call(
    db: &DatabaseConnection,
    registry: &ProviderRegistry,
    subject: &ConcreteSubject<'_>,
) -> Result<ConcreteReviewResult, DbErr> {
    let now_unix_ms =
        u64::try_from(chrono::Utc::now().timestamp_millis()).map_err(|_| invalid())?;
    let (candidate, claim) = match claim(db, subject, registry, now_unix_ms).await? {
        ClaimState::NotRequired => return Ok(ConcreteReviewResult::NotRequired),
        ClaimState::Existing(result) => return Ok(result),
        ClaimState::Claimed(candidate, claim) => (candidate, claim),
    };
    let remaining_ms = candidate
        .expires_at_unix_ms
        .saturating_sub(u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0));
    let response = tokio::time::timeout(
        std::time::Duration::from_millis(remaining_ms.saturating_sub(1_000)),
        crate::approval_reviewer::call_claimed_permission_review(db, &candidate, &claim),
    )
    .await;
    let (decision, tokens, cost) = match &response {
        Ok(Ok(response)) => (
            response.decision.as_ref(),
            desk_diagnose_core::approval_review::reviewer_billed_tokens(response.usage),
            claim.prices.actual(response.usage),
        ),
        _ => (None, None, None),
    };
    settle(
        db,
        subject,
        registry,
        &candidate,
        &claim,
        decision,
        tokens,
        cost,
        u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0),
    )
    .await
}
