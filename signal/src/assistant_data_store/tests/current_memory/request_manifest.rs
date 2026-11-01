use super::*;
use crate::agent_session_store::SignalAgentSessionStore;
use crate::entity::{agent_assistant_quota as quota, agent_assistant_runtime as runtime};
use desk_diagnose_core::seam::SessionSeam;

#[tokio::test]
async fn actual_session_port_selects_current_read_without_consuming_it_or_minting_a_send() {
    let (fixture, mut request, proof) = ready_request("manifest-native-current").await;
    let expected = request.assistant_data.take().unwrap();
    let quota_before = quota::Entity::find().one(&fixture.store.db).await.unwrap();
    let native_before = native::Entity::find().all(&fixture.store.db).await.unwrap();
    let auth_before = authorization::Entity::find()
        .all(&fixture.store.db)
        .await
        .unwrap();
    let projection_before = projection::load_on(
        &fixture.store.db,
        &fixture.session.actor_id,
        &proof.usage.projection_id,
    )
    .await
    .unwrap()
    .unwrap()
    .row;
    let host = SignalAgentSessionStore::new(fixture.store.db.clone());
    // The caller sampled this clock before a backwards wall-clock adjustment.
    // Keep the earlier sample ahead of later reads without racing the live clock.
    let caller_sample = chrono::Utc::now().timestamp_millis() as u64 + 30_000;
    for observed_at in [chrono::Utc::now().timestamp_millis() as u64, caller_sample] {
        let manifest = host
            .assistant_data_request_manifest(&fixture.session, &request, observed_at)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&manifest, expected.as_ref());
        assert_eq!(
            manifest.read_projection_ids,
            vec![proof.usage.projection_id.clone()]
        );
    }
    let expires_at = agent_session::Entity::find()
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap()
        .lease_deadline
        .unwrap()
        .timestamp_millis() as u64;
    assert!(
        host.assistant_data_request_manifest(&fixture.session, &request, expires_at)
            .await
            .is_err()
    );
    request.messages.clear();
    let empty = host
        .assistant_data_request_manifest(
            &fixture.session,
            &request,
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(empty.read_projection_ids.is_empty() && empty.current_memories.is_empty());
    assert_eq!(
        quota::Entity::find().one(&fixture.store.db).await.unwrap(),
        quota_before
    );
    assert_eq!(
        native::Entity::find().all(&fixture.store.db).await.unwrap(),
        native_before
    );
    assert_eq!(
        authorization::Entity::find()
            .all(&fixture.store.db)
            .await
            .unwrap(),
        auth_before
    );
    assert_eq!(
        projection::load_on(
            &fixture.store.db,
            &fixture.session.actor_id,
            &proof.usage.projection_id
        )
        .await
        .unwrap()
        .unwrap()
        .row,
        projection_before
    );
}

#[tokio::test]
async fn actual_manifest_port_rejects_stale_native_identity_revoked_reads_and_changed_selected_bytes()
 {
    for case in 0..5 {
        let (fixture, mut request, proof) =
            ready_request(&format!("manifest-native-stale-{case}")).await;
        request.assistant_data = None;
        match case {
            0 => {
                agent_session::Entity::update_many()
                    .col_expr(
                        agent_session::Column::Version,
                        Expr::value(fixture.session.version + 1),
                    )
                    .exec(&fixture.store.db)
                    .await
                    .unwrap();
            }
            1 => {
                projection_row::Entity::update_many()
                    .col_expr(
                        projection_row::Column::RevokedAtMs,
                        Expr::value(Some(chrono::Utc::now().timestamp_millis())),
                    )
                    .filter(projection_row::Column::ProjectionId.eq(&proof.usage.projection_id))
                    .exec(&fixture.store.db)
                    .await
                    .unwrap();
            }
            2 => {
                request.messages[0]
                    .text
                    .push_str(" changed selected content");
            }
            3 => {
                runtime::Entity::update_many()
                    .col_expr(
                        runtime::Column::ControlGeneration,
                        Expr::value(
                            fixture
                                .session
                                .assistant_data
                                .as_ref()
                                .unwrap()
                                .control_generation as i64
                                + 1,
                        ),
                    )
                    .exec(&fixture.store.db)
                    .await
                    .unwrap();
            }
            _ => {
                agent_session::Entity::update_many()
                    .col_expr(
                        agent_session::Column::LeaseDeadline,
                        Expr::value(Some(chrono::Utc::now() - chrono::Duration::seconds(1))),
                    )
                    .exec(&fixture.store.db)
                    .await
                    .unwrap();
            }
        }
        let quota_before = quota::Entity::find().one(&fixture.store.db).await.unwrap();
        let host = SignalAgentSessionStore::new(fixture.store.db.clone());
        assert!(
            host.assistant_data_request_manifest(
                &fixture.session,
                &request,
                chrono::Utc::now().timestamp_millis() as u64
            )
            .await
            .is_err(),
            "case {case}"
        );
        assert_eq!(
            quota::Entity::find().one(&fixture.store.db).await.unwrap(),
            quota_before
        );
    }
}
