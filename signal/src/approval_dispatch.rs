//! Durable OSS permission-review dispatch. Each item is independently claimed
//! and charged; only a fully reviewed batch enters the existing permission
//! decision transaction. A failed reviewer produces a system-sourced denial
//! after the bounded review attempt; no device action is dispatched.

use crate::config::connection::DatabaseConnection;
use actix_web::web;
use desk_agent_protocol::ai_assistant::AiAssistantAsk;
use desk_agent_protocol::capability_provider::ProductSurface;
use desk_agent_protocol::computer_use::ObjectKind;
use desk_diagnose_core::ai_assistant::{
    ai_assistant_provider_registry, provider_readiness_reports,
};
use desk_diagnose_core::approval_review::PendingRequestKind;
use desk_diagnose_core::approval_review::reviewer_billed_tokens;
use desk_diagnose_core::capability_availability::{
    CapabilityAvailability, project_capability_availability,
};
use desk_diagnose_core::provider_registry::ProviderRegistry;
use desk_signal_facade::model::{
    auth_context::AuthKind, connection::SharedConnectionMap, signal::RemoteDeskTypeEnum,
};

use crate::agent_approval_store::PendingPermissionReview;
use crate::agent_session_store::{
    PermissionDecisionSubject, PermissionGrantIssuanceContext, SignalAgentSessionStore,
};
use crate::control_authorizer::SINGLE_ACCOUNT_USER_ID;

struct ReviewDeviceContext {
    connection_id: String,
    registry: ProviderRegistry,
    inventory: Vec<CapabilityAvailability>,
    readiness_revision: u64,
    device_allows_unattended: bool,
    implicit_fresh_object_refs: Vec<desk_agent_protocol::computer_use::ObjectRef>,
}

async fn current_device_context(
    db: &DatabaseConnection,
    connections: &SharedConnectionMap,
    subject: &PendingPermissionReview,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<ReviewDeviceContext> {
    if matches!(
        subject.kind,
        PendingRequestKind::Goal | PendingRequestKind::Schedule
    ) {
        return Some(ReviewDeviceContext {
            connection_id: String::new(),
            registry: ai_assistant_provider_registry(),
            inventory: vec![],
            readiness_revision: 0,
            device_allows_unattended: false,
            implicit_fresh_object_refs: vec![],
        });
    }
    if subject.kind == PendingRequestKind::Permission && subject.target_device_id.is_empty() {
        return targetless_context(db, now).await;
    }
    let connection_id = {
        let map = connections.read().await;
        let mut targets = map.values().filter(|target| {
            target.auth_context.auth_kind == AuthKind::TokenAuth
                && target.auth_context.remote_desk_type == RemoteDeskTypeEnum::Server
                && target.client_id.as_deref() == Some(subject.target_device_id.as_str())
        });
        let first = targets
            .next()
            .map(|target| target.model.connection_id.clone());
        if targets.next().is_some() {
            None
        } else {
            first
        }
    }?;
    let readiness = crate::computer_use_readiness::global_computer_use_readiness_cache()
        .get_fresh(&connection_id, now)?;
    let reports = provider_readiness_reports(&readiness.readiness).ok()?;
    let search = crate::web_search_config::read(db).await.ok()?;
    let registry = ai_assistant_provider_registry().with_web_search_binding(search.binding());
    let registry =
        match crate::command_policy::current(db, connections, &connection_id, &subject.owner_id)
            .await
        {
            Ok(policy) => registry.with_command_policy(policy),
            Err(_) => registry,
        };
    let now_unix_ms = u64::try_from(now.timestamp_millis()).ok()?;
    let inventory = project_capability_availability(
        &registry,
        ProductSurface::OssPersonalOwner,
        now_unix_ms,
        desk_diagnose_core::capability_availability::assistant_central_capability_readiness(
            search.configured(),
            true,
        ),
        reports,
    )
    .ok()?;
    let implicit_fresh_object_refs = readiness
        .readiness
        .context_references
        .iter()
        .filter(|reference| {
            matches!(
                reference.object_ref.object_kind,
                ObjectKind::BrowserSurface
                    | ObjectKind::Application
                    | ObjectKind::Range
                    | ObjectKind::Document
                    | ObjectKind::Slide
            )
        })
        .map(|reference| reference.object_ref.clone())
        .collect();
    Some(ReviewDeviceContext {
        connection_id,
        registry,
        inventory,
        readiness_revision: readiness.readiness.revision,
        device_allows_unattended: readiness.readiness.unattended_command_execution,
        implicit_fresh_object_refs,
    })
}

/// A central web request in a personal conversation is reviewed with the same
/// central capability context as the owner's decision for it.
async fn targetless_context(
    db: &DatabaseConnection,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<ReviewDeviceContext> {
    let search = crate::web_search_config::read(db).await.ok()?;
    let registry = ai_assistant_provider_registry().with_web_search_binding(search.binding());
    let inventory = project_capability_availability(
        &registry,
        ProductSurface::OssPersonalOwner,
        u64::try_from(now.timestamp_millis()).ok()?,
        desk_diagnose_core::capability_availability::assistant_central_capability_readiness(
            search.configured(),
            false,
        ),
        Vec::new(),
    )
    .ok()?;
    Some(ReviewDeviceContext {
        connection_id: String::new(),
        registry,
        inventory,
        readiness_revision: desk_diagnose_core::web_research::CENTRAL_WEB_READINESS_REVISION,
        device_allows_unattended: false,
        implicit_fresh_object_refs: vec![],
    })
}

pub(crate) async fn process_pending_unattended_command(
    db: DatabaseConnection,
    connections: web::Data<SharedConnectionMap>,
    subject: PendingPermissionReview,
) -> bool {
    if subject.owner_id.parse::<i32>().ok() != Some(SINGLE_ACCOUNT_USER_ID) {
        return false;
    }
    let now = chrono::Utc::now();
    let Some(device) = current_device_context(&db, connections.as_ref(), &subject, now).await
    else {
        return false;
    };
    if !device.device_allows_unattended {
        return false;
    }
    let store = SignalAgentSessionStore::new(db.clone());
    let Ok(outcome) = store
        .decide_permission_request_without_review(
            PermissionDecisionSubject {
                conversation_id: &subject.conversation_id,
                actor_id: &subject.owner_id,
                device_id: &subject.device_id,
            },
            &subject.request_id,
            PermissionGrantIssuanceContext {
                surface: ProductSurface::OssPersonalOwner,
                registry: &device.registry,
                inventory: &device.inventory,
                readiness_revision: device.readiness_revision,
                now_unix_ms: u64::try_from(now.timestamp_millis()).unwrap_or(0),
                implicit_fresh_object_refs: &device.implicit_fresh_object_refs,
            },
            device.device_allows_unattended,
            &now.to_rfc3339(),
        )
        .await
    else {
        return false;
    };
    if !outcome.newly_recorded {
        return true;
    }
    resume_permission_decision(db, store, connections, subject, device.connection_id).await
}

pub async fn process_pending_permission_review(
    db: DatabaseConnection,
    connections: web::Data<SharedConnectionMap>,
    subject: PendingPermissionReview,
) -> bool {
    if subject.owner_id.parse::<i32>().ok() != Some(SINGLE_ACCOUNT_USER_ID) {
        return false;
    }
    let now = chrono::Utc::now();
    let Some(device) = current_device_context(&db, connections.as_ref(), &subject, now).await
    else {
        if subject.kind == PendingRequestKind::Goal {
            return crate::agent_approval_store::deny_goal_unavailable(
                &db,
                &subject.conversation_id,
                &subject.owner_id,
                &subject.device_id,
                &subject.request_id,
                u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0),
                desk_diagnose_core::approval_observation::ReviewReason::Policy,
            )
            .await
            .unwrap_or(false);
        }
        if subject.kind == PendingRequestKind::Directory {
            return crate::agent_approval_store::deny_directory_unavailable(
                &db,
                &subject.conversation_id,
                &subject.owner_id,
                &subject.device_id,
                &subject.request_id,
                u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0),
                true,
            )
            .await
            .is_ok();
        }
        return false;
    };
    let now_unix_ms = u64::try_from(now.timestamp_millis()).unwrap_or(0);
    let candidates = match crate::agent_approval_store::prepare_permission_review_batch(
        &db,
        &subject.conversation_id,
        &subject.owner_id,
        &subject.device_id,
        &subject.request_id,
        subject.kind,
        &device.registry,
        ProductSurface::OssPersonalOwner,
        device.readiness_revision,
        now_unix_ms,
    )
    .await
    {
        Ok(candidates) => candidates,
        Err(_) if subject.kind == PendingRequestKind::Goal => {
            return crate::agent_approval_store::deny_goal_unavailable(
                &db,
                &subject.conversation_id,
                &subject.owner_id,
                &subject.device_id,
                &subject.request_id,
                now_unix_ms,
                desk_diagnose_core::approval_observation::ReviewReason::Processing,
            )
            .await
            .unwrap_or(false);
        }
        Err(error) => {
            log::warn!(
                "[approval-executor] permission preparation deferred: request={}, error={error}",
                subject.request_id
            );
            return false;
        }
    };
    if candidates.is_empty() {
        return true;
    }
    let mut unstarted_failures = Vec::new();
    let mut retry_required = false;
    for candidate in &candidates {
        let lease_owner = format!("oss-review-{}", uuid::Uuid::new_v4());
        let claim = match crate::agent_approval_store::claim_permission_review(
            &db,
            candidate,
            &device.registry,
            ProductSurface::OssPersonalOwner,
            &lease_owner,
            device.readiness_revision,
            u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0),
        )
        .await
        {
            Ok(claim) => claim,
            Err(crate::agent_approval_store::ReviewStartError::Unavailable(reason)) => {
                if subject.kind == PendingRequestKind::Permission
                    && reason
                        == desk_diagnose_core::approval_observation::ReviewReason::Configuration
                {
                    return false;
                }
                // An unavailable reviewer leaves the proposal to the owner.
                if subject.kind == PendingRequestKind::Schedule {
                    return true;
                }
                if subject.kind == PendingRequestKind::Goal {
                    return crate::agent_approval_store::deny_goal_unavailable(
                        &db,
                        &subject.conversation_id,
                        &subject.owner_id,
                        &subject.device_id,
                        &subject.request_id,
                        u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0),
                        reason,
                    )
                    .await
                    .unwrap_or(false);
                }
                let (reason_code, reason_text) =
                    desk_diagnose_core::approval_review::system_review_reason(reason);
                if let desk_diagnose_core::approval_review::ApprovalSource::PermissionItem {
                    item_id,
                    ..
                } = &candidate.source
                {
                    unstarted_failures.push(
                        desk_diagnose_core::dynamic_run::SystemPermissionDecisionEvidence {
                            item_id: item_id.clone(),
                            candidate_id: Some(candidate.candidate_id.clone()),
                            reason_code: reason_code.into(),
                            reason: reason_text.into(),
                        },
                    );
                }
                continue;
            }
            Err(crate::agent_approval_store::ReviewStartError::Backend(_error)) => {
                retry_required = true;
                continue;
            }
        };
        let Some(claim) = claim else {
            continue;
        };
        let response =
            crate::approval_reviewer::call_claimed_permission_review(&db, candidate, &claim).await;
        let current_revision =
            current_device_context(&db, connections.as_ref(), &subject, chrono::Utc::now())
                .await
                .map_or(0, |device| device.readiness_revision);
        let (decision, tokens) = match &response {
            Ok(response) => (
                response.decision.as_ref(),
                reviewer_billed_tokens(response.usage),
            ),
            Err(_) => (None, None),
        };
        if crate::agent_approval_store::settle_permission_review(
            &db,
            candidate,
            &claim.lease_owner,
            claim.lease_epoch,
            decision,
            tokens,
            current_revision,
            u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0),
            claim.statistics.try_lock().ok().map(|value| value.clone()),
        )
        .await
        .is_err()
        {
            return false;
        }
    }
    if retry_required {
        return false;
    }
    if matches!(
        subject.kind,
        PendingRequestKind::Goal | PendingRequestKind::Schedule
    ) {
        return true;
    }
    if subject.kind == PendingRequestKind::Directory {
        return crate::agent_approval_store::deny_directory_unavailable(
            &db,
            &subject.conversation_id,
            &subject.owner_id,
            &subject.device_id,
            &subject.request_id,
            u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0),
            false,
        )
        .await
        .is_ok();
    }
    let now = chrono::Utc::now();
    let Some(device) = current_device_context(&db, connections.as_ref(), &subject, now).await
    else {
        return false;
    };
    let now_unix_ms = u64::try_from(now.timestamp_millis()).unwrap_or(0);
    let store = SignalAgentSessionStore::new(db.clone());
    let outcome = store
        .decide_permission_request_by_ai(
            PermissionDecisionSubject {
                conversation_id: &subject.conversation_id,
                actor_id: &subject.owner_id,
                device_id: &subject.device_id,
            },
            &subject.request_id,
            &candidates,
            &unstarted_failures,
            PermissionGrantIssuanceContext {
                surface: ProductSurface::OssPersonalOwner,
                registry: &device.registry,
                inventory: &device.inventory,
                readiness_revision: device.readiness_revision,
                now_unix_ms,
                implicit_fresh_object_refs: &device.implicit_fresh_object_refs,
            },
            &now.to_rfc3339(),
        )
        .await;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            log::warn!(
                "[approval-executor] permission decision deferred: request={}, kind={:?}, retryable={}, message={}",
                subject.request_id,
                error.kind,
                error.retryable,
                error.message
            );
            return false;
        }
    };
    if !outcome.newly_recorded {
        return true;
    }
    resume_permission_decision(db, store, connections, subject, device.connection_id).await
}

async fn resume_permission_decision(
    db: DatabaseConnection,
    store: SignalAgentSessionStore,
    connections: web::Data<SharedConnectionMap>,
    subject: PendingPermissionReview,
    connection_id: String,
) -> bool {
    if !subject.device_id.is_empty() {
        match crate::agent_goal_store::wake_for_permission_decision(
            &db,
            &subject.conversation_id,
            &subject.owner_id,
            &subject.device_id,
            &subject.request_id,
            chrono::Utc::now(),
        )
        .await
        {
            Ok(desk_diagnose_core::goal::GoalPermissionWake::NoGoal) => {}
            Ok(
                desk_diagnose_core::goal::GoalPermissionWake::Queued
                | desk_diagnose_core::goal::GoalPermissionWake::Held,
            ) => return true,
            Err(error) => {
                log::warn!("[ai-assistant-goal] AI permission decision routing deferred: {error}");
                return true;
            }
        }
    }
    let snapshot = store
        .read_snapshot_for_subject(
            &subject.conversation_id,
            &subject.owner_id,
            &subject.device_id,
        )
        .await
        .ok()
        .flatten();
    let Some(snapshot) = snapshot else {
        return true;
    };
    let child_context = match crate::agent_subagent_store::SubAgentStore::new(db.clone())
        .child_context_for_subject(
            &subject.conversation_id,
            &subject.owner_id,
            &subject.device_id,
            snapshot.seq,
        )
        .await
    {
        Ok(context) => context,
        Err(_) => return true,
    };
    let Some(question) = child_context
        .as_ref()
        .map(|(creation, _)| creation.source.owner_requirement.text.clone())
        .or_else(|| {
            desk_diagnose_core::permission_resume::latest_user_requirement(&snapshot.messages)
                .map(|message| message.text.clone())
        })
    else {
        return true;
    };
    let locale = child_context
        .as_ref()
        .and_then(|(creation, _)| creation.response_locale.clone());
    let resume_request_id = format!("permission-resume-{}", subject.request_id);
    let ask = AiAssistantAsk {
        temporary_conversation: None,
        question,
        locale,
        client_message_id: resume_request_id.clone(),
        conversation_id: snapshot.client_conversation_id,
        ..Default::default()
    };
    // A selected action target does not replace the targetless session subject.
    let connection_id = if subject.device_id.is_empty() {
        String::new()
    } else {
        connection_id
    };
    actix_web::rt::spawn(async move {
        crate::ai_assistant_orchestrator::resume_after_permission_decision(
            connections,
            db,
            resume_request_id,
            connection_id,
            SINGLE_ACCOUNT_USER_ID,
            subject.device_id,
            subject.conversation_id,
            subject.request_id,
            ask,
        )
        .await;
    });
    true
}
