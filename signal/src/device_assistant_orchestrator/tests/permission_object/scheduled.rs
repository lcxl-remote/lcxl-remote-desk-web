//! Original model-proposed permission resumed through a durable timer.
use super::*;
use crate::schedule_store::{ContinuationClaim, ContinuationLease, ScheduleStore};
use desk_agent_protocol::schedule::*;
use desk_diagnose_core::{
    seam::SessionSeam,
    session::{PersistedAgentSession, TurnState},
};

#[actix_web::test]
async fn scheduled_approval_reads_original_object_over_websocket_and_settles_once() {
    Box::pin(run_case(None, ResumeMode::Scheduled)).await;
}

#[actix_web::test]
async fn scheduled_approval_reads_original_live_document_over_websocket() {
    Box::pin(run_case_with_live(None, ResumeMode::Scheduled, true)).await;
}

pub(super) async fn wait_for_permission(
    db: &DatabaseConnection,
    conversation: &str,
    request: &str,
) -> String {
    let row = crate::entity::agent_session::Entity::find()
        .filter(crate::entity::agent_session::Column::ConversationId.eq(conversation))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    let store = ScheduleStore::new(db.clone());
    let task = store
        .create_draft(
            1,
            &ScheduleDraft {
                time_confirmation: None,
                client_create_key: "scheduled-read".into(),
                kind: ScheduledTaskKind::ConversationResume,
                target_device_id: "device".into(),
                title: "Continue original read".into(),
                prompt: "Continue".into(),
                locale: None,
                model_id: None,
                source_conversation_id: Some(conversation.into()),
                requirement_revision: Some(session.input_revision),
                creation_source: ScheduleCreationSource::Manual,
                spec: ScheduleSpec {
                    schema_version: 1,
                    rule: ScheduleRule::Once {
                        at: (Utc::now() + chrono::Duration::seconds(3))
                            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    },
                },
            },
            store.database_time().await.unwrap(),
        )
        .await
        .unwrap();
    let task = store
        .activate_conversation_resume(1, &task.schedule_id, task.revision)
        .await
        .unwrap();
    let wait = task.next_run_at.unwrap() - store.database_time().await.unwrap() + 20;
    tokio::time::sleep(Duration::from_millis(wait.max(0) as u64)).await;
    let run = store
        .materialize_due(&task.schedule_id, task.revision)
        .await
        .unwrap()
        .unwrap();
    let mut claimed = store
        .claim_conversation_resume(ContinuationClaim {
            owner: 1,
            run_id: &run.run_id,
            node_id: "oss-scheduler",
            lease_seconds: 90,
            policy_revision: session.policy_revision,
            scope: session.scope_snapshot,
        })
        .await
        .unwrap();
    claimed
        .session
        .finish_turn(TurnState::Idle, Utc::now().to_rfc3339());
    claimed.session.handled_input_seq = claimed.session.latest_input_seq;
    crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .save(&mut claimed.session)
        .await
        .unwrap();
    store
        .await_continuation_permission(
            ContinuationLease {
                owner: 1,
                run_id: &run.run_id,
                node_id: "oss-scheduler",
                run_epoch: claimed.run.lease_epoch,
                session_token: claimed.session.lease_token,
            },
            request,
        )
        .await
        .unwrap();
    run.run_id
}
#[actix_web::test]
async fn scheduled_approval_revoked_after_claim_never_reads_from_the_device() {
    Box::pin(run_case(Some("scheduled_revoke"), ResumeMode::Scheduled)).await;
}

#[actix_web::test]
async fn scheduled_executor_scans_approval_and_reads_original_object_once() {
    Box::pin(run_case(None, ResumeMode::ScheduledScan)).await;
}

#[actix_web::test]
async fn scheduled_executor_periodic_loop_resumes_without_controller_wakeup() {
    Box::pin(run_case(None, ResumeMode::ScheduledLoop)).await;
}
