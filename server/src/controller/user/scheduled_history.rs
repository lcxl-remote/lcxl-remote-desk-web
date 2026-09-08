//! Exercise public occurrence reads through the production owner guard.
use super::*;
use actix_session::{SessionMiddleware, storage::CookieSessionStore};
use actix_web::{App, cookie::Key, middleware::from_fn, test, web};
use desk_agent_protocol::{AgentScope, ExecutionMode, schedule::*};
use desk_diagnose_core::{
    chat::{ChatMessage, ChatRole},
    session::{AgentSessionSurface, PersistedAgentSession},
};
use desk_signal::{
    controller::device_assistant_session::get_device_assistant_session,
    entity::{agent_schedule_run as run, agent_session},
};
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

async fn seed_owner(session: Session) -> HttpResponse {
    session
        .set_current_user(&CurrentUser::new_admin("owner"))
        .unwrap();
    HttpResponse::Ok().finish()
}
async fn seed_code(session: Session) -> HttpResponse {
    session
        .insert(
            CODE_SESSION_KEY,
            CodeSessionCookie {
                code_session_id: "temporary-code".into(),
                grant_session_id: "grant".into(),
                target_connection_id: "device".into(),
            },
        )
        .unwrap();
    HttpResponse::Ok().finish()
}

#[actix_web::test]
#[ignore = "run alone: initializes the process-wide OSS database in a temporary directory"]
async fn scheduled_result_rest_owner_guard_and_original_subject() {
    let dir = tempfile::tempdir().unwrap();
    let db = desk_signal::db::init_db(dir.path().to_str().unwrap())
        .await
        .unwrap();
    let now = chrono::Utc::now();
    let ms = now.timestamp_millis();
    let schedules = desk_signal::schedule_store::ScheduleStore::new(db.clone());
    let draft = ScheduleDraft {
        client_create_key: "result-rest".into(),
        kind: ScheduledTaskKind::ConversationResume,
        target_device_id: "device-1".into(),
        title: "Saved result".into(),
        prompt: "Review status".into(),
        locale: None,
        model_id: None,
        spec: ScheduleSpec {
            schema_version: SCHEDULE_SCHEMA_VERSION,
            rule: ScheduleRule::Once {
                at: (now + chrono::Duration::hours(1))
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            },
        },
        time_confirmation: None,
        source_conversation_id: Some("original-session".into()),
        requirement_revision: Some(1),
        creation_source: ScheduleCreationSource::Manual,
    };
    let task = schedules.create_draft(1, &draft, ms).await.unwrap();
    let run_id = "schedule-run-rest-test";
    let turn_id = format!("{run_id}-turn");
    let mut session = PersistedAgentSession::new(
        "original-session",
        "1",
        "device-1",
        1,
        AgentScope {
            granted: vec![],
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        &now.to_rfc3339(),
    );
    session.surface = AgentSessionSurface::DeviceAssistant;
    session.conversation.push(ChatMessage::text(
        "question",
        ChatRole::User,
        "Original question",
    ));
    session.conversation.push(
        ChatMessage::text("answer", ChatRole::Assistant, "Original saved result")
            .with_turn_id(&turn_id),
    );
    agent_session::ActiveModel {
        conversation_id: Set(session.conversation_id.clone()),
        actor_id: Set("1".into()),
        device_id: Set("device-1".into()),
        state_json: Set(session.encode_json_for_storage().unwrap()),
        version: Set(session.version),
        lease_token: Set(0),
        lease_deadline: Set(None),
        snapshot_seq: Set(None),
        snapshot_fingerprint: Set(None),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
    run::ActiveModel {
        run_id: Set(run_id.into()),
        schedule_id: Set(task.schedule_id.clone()),
        owner_user_id: Set(1),
        occurrence_identity: Set("result-rest-occurrence".into()),
        source: Set("calendar".into()),
        scheduled_at: Set(Some(ms)),
        requested_at: Set(ms),
        schedule_revision: Set(task.revision),
        recovery_epoch: Set(0),
        task_snapshot_json: Set(serde_json::to_string(&task).unwrap()),
        conversation_id: Set(session.conversation_id.clone()),
        turn_id: Set(turn_id),
        status: Set("succeeded".into()),
        start_deadline: Set(ms + 60_000),
        lease_epoch: Set(1),
        lease_owner: Set(None),
        lease_deadline: Set(None),
        attempt: Set(1),
        failure_accounted: Set(true),
        error_kind: Set(None),
        result_ref: Set(Some("message:answer".into())),
        missed_count: Set(0),
        started_at: Set(Some(ms)),
        finished_at: Set(Some(ms)),
        cancel_requested_at: Set(None),
        created_at: Set(ms),
        updated_at: Set(ms),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
    let app = test::init_service(
        App::new()
            .wrap(
                SessionMiddleware::builder(CookieSessionStore::default(), Key::generate())
                    .cookie_secure(false)
                    .build(),
            )
            .app_data(web::Data::new(
                desk_signal_facade::model::connection::SharedConnectionMap::default(),
            ))
            .route("/seed-owner", web::get().to(seed_owner))
            .route("/seed-code", web::get().to(seed_code))
            .service(
                web::scope("/api")
                    .wrap(from_fn(enforce_device_scope))
                    .service(get_device_assistant_session),
            ),
    )
    .await;
    let url = format!(
        "/api/my/device-assistant-session?scheduled_task={}&scheduled_run={run_id}&message_limit=1",
        task.schedule_id
    );
    let anonymous =
        test::try_call_service(&app, test::TestRequest::get().uri(&url).to_request()).await;
    assert_eq!(
        anonymous.unwrap_err().error_response().status().as_u16(),
        401
    );
    let login = test::call_service(
        &app,
        test::TestRequest::get().uri("/seed-code").to_request(),
    )
    .await;
    let code_cookie = login.response().cookies().next().unwrap().into_owned();
    let denied = test::try_call_service(
        &app,
        test::TestRequest::get()
            .uri(&url)
            .cookie(code_cookie)
            .to_request(),
    )
    .await;
    assert_eq!(denied.unwrap_err().error_response().status().as_u16(), 403);
    let login = test::call_service(
        &app,
        test::TestRequest::get().uri("/seed-owner").to_request(),
    )
    .await;
    let cookie = login.response().cookies().next().unwrap().into_owned();
    let result = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&url)
            .cookie(cookie.clone())
            .to_request(),
    )
    .await;
    let body: serde_json::Value = test::read_body_json(result).await;
    assert_eq!(body["code"], DeskErrorCode::SUCCESS.code());
    assert_eq!(body["data"]["messages"][0]["text"], "Original saved result");
    assert_eq!(body["data"]["messagePage"]["nextBeforeMessageId"], "answer");
    let older = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("{url}&message_before=answer"))
            .cookie(cookie.clone())
            .to_request(),
    )
    .await;
    let body: serde_json::Value = test::read_body_json(older).await;
    assert_eq!(body["data"]["messages"][0]["text"], "Original question");
    for uri in [format!("{url}&session=original-session"), format!("{url}&connection=host"), "/api/my/device-assistant-session?scheduled_task=other&scheduled_run=schedule-run-rest-test".into()] {
        let response = test::call_service(&app, test::TestRequest::get().uri(&uri).cookie(cookie.clone()).to_request()).await;
        let body: serde_json::Value = test::read_body_json(response).await;
        assert_eq!(body["code"], DeskErrorCode::PERMISSION_ERROR.code());
    }
    for field in ["actor", "device"] {
        let (actor, device) = if field == "actor" {
            ("2", "device-1")
        } else {
            ("1", "foreign-device")
        };
        agent_session::Entity::update_many()
            .set(agent_session::ActiveModel {
                actor_id: Set(actor.into()),
                device_id: Set(device.into()),
                ..Default::default()
            })
            .filter(agent_session::Column::ConversationId.eq("original-session"))
            .exec(db)
            .await
            .unwrap();
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&url)
                .cookie(cookie.clone())
                .to_request(),
        )
        .await;
        let body: serde_json::Value = test::read_body_json(response).await;
        assert_eq!(body["code"], DeskErrorCode::PERMISSION_ERROR.code());
        assert!(body.get("data").is_none_or(serde_json::Value::is_null));
    }
}
