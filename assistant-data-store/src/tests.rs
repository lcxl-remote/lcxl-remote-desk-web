use super::*;
use desk_agent_protocol::ai_assistant::user_instructions::AssistantDataPolicy;
use sea_orm::{
    ColumnTrait, ConnectionTrait, Database, EntityTrait, QueryFilter, Schema, TransactionTrait,
};
mod main_request;
mod memory_index;
mod memory_quota;
mod projection;
mod retrieval_cache;

#[tokio::test]
async fn device_roster_preserves_retained_generations_and_fences_stale_writes() {
    let TestStore { db } = setup().await;
    let txn = db.begin().await.unwrap();
    let initial = device_roster::replace(&txn, "owner", "root", 0, vec!["a".into(), "b".into()])
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(initial.revision, 1);
    assert_eq!(initial.members["a"], 1);
    assert_eq!(initial.members["b"], 1);

    let txn = db.begin().await.unwrap();
    let reordered = device_roster::replace(&txn, "owner", "root", 1, vec!["b".into(), "a".into()])
        .await
        .unwrap();
    assert_eq!(initial, reordered);
    let removed = device_roster::replace(&txn, "owner", "root", 1, vec!["b".into()])
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(removed.revision, 2);
    assert_eq!(removed.members["b"], 1);
    assert!(matches!(
        device_roster::load(&db, "another-owner", "root").await,
        Err(AssistantStoreError::Inaccessible)
    ));

    let txn = db.begin().await.unwrap();
    assert!(matches!(
        device_roster::replace(&txn, "owner", "root", 1, vec![]).await,
        Err(AssistantStoreError::Stale {
            current_revision: 2
        })
    ));
    let rejoined = device_roster::replace(&txn, "owner", "root", 2, vec!["a".into(), "b".into()])
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(rejoined.revision, 3);
    assert_eq!(rejoined.members["a"], 3);
    assert_eq!(rejoined.members["b"], 1);
    assert_eq!(
        device_roster::load(&db, "owner", "root").await.unwrap(),
        Some(rejoined)
    );
}

#[test]
fn unavailable_sources_are_tool_errors_but_authorization_and_storage_failures_are_not() {
    let source = AssistantStoreError::SourceUnavailable.into_agent_error();
    assert!(source.safe_for_model);
    assert_eq!(
        source.kind,
        desk_agent_protocol::AgentErrorKind::InvalidInput
    );
    for error in [
        AssistantStoreError::Inaccessible,
        AssistantStoreError::Invalid("native gate rejected"),
        AssistantStoreError::Backend(DbErr::Custom("unavailable".into())),
    ] {
        assert!(!error.into_agent_error().safe_for_model);
    }
}

struct TestStore {
    db: sea_orm::DatabaseConnection,
}
async fn setup() -> TestStore {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    entity::register_schema(sea_orm::SchemaBuilder::new(Schema::new(
        db.get_database_backend(),
    )))
    .sync(&db)
    .await
    .unwrap();
    TestStore { db }
}
fn destination(model: &str) -> desk_agent_protocol::data_lineage::DestinationIdentity {
    desk_agent_protocol::data_lineage::DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: model.into(),
        profile_revision: 1,
    }
}

#[tokio::test]
async fn schema_reopen_preserves_committed_assistant_rows_and_metadata_charge() {
    let store = setup().await;
    ensure_quota_on(&store.db, "owner").await.unwrap();
    crate::projection::prepare_on(
        &store.db,
        &projection::proof("{}", "schema"),
        "{}",
        &AssistantDataPolicy::default(),
        1001,
    )
    .await
    .unwrap();
    let before = entity::agent_assistant_quota::Entity::find()
        .all(&store.db)
        .await
        .unwrap();
    entity::register_schema(sea_orm::SchemaBuilder::new(Schema::new(
        store.db.get_database_backend(),
    )))
    .sync(&store.db)
    .await
    .unwrap();
    assert_eq!(
        entity::agent_assistant_quota::Entity::find()
            .all(&store.db)
            .await
            .unwrap(),
        before
    );
    assert!(
        crate::projection::load_on(
            &store.db,
            "1",
            &projection::proof("{}", "schema").usage.projection_id
        )
        .await
        .unwrap()
        .is_some()
    );
}

#[tokio::test]
async fn reports_follow_the_owner_model_within_one_gateway_after_backfill() {
    use crate::entity::agent_subagent_inbox as inbox;
    use sea_orm::{ActiveModelTrait, QueryOrder, Set};
    let store = setup().await;
    let old = destination("old-model");
    let row = |event: &str, stored: Option<String>| inbox::ActiveModel {
        event_id: Set(event.into()),
        root_conversation_id: Set("root".into()),
        group_id: Set("group".into()),
        task_id: Set("task".into()),
        actor_id: Set("1".into()),
        device_id: Set(String::new()),
        state_revision: Set(1),
        report_seq: Set(Some(1)),
        report_model_destination: Set(stored),
        event_kind: Set("report_completed".into()),
        event_json: Set("{}".into()),
        parent_input_revision: Set(1),
        parent_control_revision: Set(1),
        created_at: Set(1),
        ..Default::default()
    };
    // A row written before reports stored their gateway identity.
    row("legacy", Some(serde_json::to_string(&old).unwrap()))
        .insert(&store.db)
        .await
        .unwrap();
    row("protected", None).insert(&store.db).await.unwrap();
    assert_eq!(
        worker_report::normalize_report_gateways_on(&store.db)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        worker_report::normalize_report_gateways_on(&store.db)
            .await
            .unwrap(),
        0
    );
    let stored: Vec<_> = inbox::Entity::find()
        .order_by_asc(inbox::Column::Id)
        .all(&store.db)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.report_model_destination)
        .collect();
    // The owner's current model on the same gateway matches the stored value.
    let current = worker_report::report_gateway(&destination("new-model")).unwrap();
    assert_eq!(stored, [Some(current.clone()), None]);
    let mut other_connection = destination("new-model");
    if let desk_agent_protocol::data_lineage::DestinationIdentity::Model {
        connection_revision,
        ..
    } = &mut other_connection
    {
        *connection_revision = 2;
    }
    assert_ne!(
        worker_report::report_gateway(&other_connection).unwrap(),
        current
    );
}

#[tokio::test]
async fn a_delegated_child_follows_its_owner_conversation_devices() {
    use desk_diagnose_core::dynamic_run::AssistantInputTarget;
    use desk_diagnose_core::native_ids::{Kind, stable};
    use desk_diagnose_core::session::{AgentSessionSurface, PersistedAgentSession};
    use sea_orm::sea_query::{Alias, ColumnDef, Query, Table};
    let store = setup().await;
    // Both hosts store conversations with these subject columns.
    store
        .db
        .execute(
            &Table::create()
                .table(Alias::new("agent_session"))
                .col(
                    ColumnDef::new(Alias::new("conversation_id"))
                        .string()
                        .not_null(),
                )
                .col(ColumnDef::new(Alias::new("actor_id")).string().not_null())
                .col(ColumnDef::new(Alias::new("device_id")).string().not_null())
                .col(ColumnDef::new(Alias::new("state_json")).text().not_null())
                .to_owned(),
        )
        .await
        .unwrap();
    let scope = desk_agent_protocol::AgentScope {
        granted: vec![],
        mode: desk_agent_protocol::ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    };
    let device = |name: &str| AssistantInputTarget::fixed(name.into());
    let owner_state = |devices: &[AssistantInputTarget], revision: u64| {
        let mut owner = PersistedAgentSession::new("root", "owner", "", 1, scope.clone(), "now");
        owner.surface = AgentSessionSurface::AiAssistant;
        owner.input_revision = revision;
        owner.bind_assistant_input_targets(devices).unwrap();
        owner.encode_json_for_storage().unwrap()
    };
    let save_owner = |state: String| {
        let db = store.db.clone();
        async move {
            db.execute(
                &Query::delete()
                    .from_table(Alias::new("agent_session"))
                    .to_owned(),
            )
            .await
            .unwrap();
            db.execute(
                &Query::insert()
                    .into_table(Alias::new("agent_session"))
                    .columns([
                        Alias::new("conversation_id"),
                        Alias::new("actor_id"),
                        Alias::new("device_id"),
                        Alias::new("state_json"),
                    ])
                    .values_panic(["root".into(), "owner".into(), "".into(), state.into()])
                    .to_owned(),
            )
            .await
            .unwrap();
        }
    };
    save_owner(owner_state(&[device("mac"), device("linux")], 1)).await;
    let child = PersistedAgentSession::new_subagent(
        stable(Kind::ChildConversation, &"child"),
        "owner",
        "",
        1,
        scope.clone(),
        desk_diagnose_core::subagent::DelegatedTaskBinding {
            root_conversation_id: "root".into(),
            group_id: "group".into(),
            task_id: stable(Kind::Task, &"task"),
            source: desk_diagnose_core::subagent::DelegationSource::UserInput { input_revision: 1 },
            targets: vec![device("mac"), device("linux")],
            objective: "Compare".into(),
            acceptance_criteria: vec!["Report".into()],
            input_revision: 1,
            control_revision: 1,
            source_epoch: 1,
            deadline_ms: 1_799_999_999_999,
        },
        "now",
    )
    .unwrap();
    assert!(
        device_roster::permits_current_target(&store.db, &child, "linux")
            .await
            .unwrap()
    );
    // The owner's next input drops Linux: the child can no longer use it.
    save_owner(owner_state(&[device("mac")], 2)).await;
    assert!(
        !device_roster::permits_current_target(&store.db, &child, "linux")
            .await
            .unwrap()
    );
    assert!(
        device_roster::permits_current_target(&store.db, &child, "mac")
            .await
            .unwrap()
    );
    assert_eq!(
        device_roster::current_request_devices(&store.db, &child)
            .await
            .unwrap(),
        ["mac"]
    );
}

#[tokio::test]
async fn a_worker_keeps_one_session_across_works_until_no_request_refers_to_it() {
    use crate::{entity::agent_subagent_run as run, worker_request};
    use desk_diagnose_core::subagent::{
        SubAgentState,
        role::{DelegatedTaskBinding, DelegationSource},
        state::SubAgentRun,
    };
    use sea_orm::{
        ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QuerySelect, QueryTrait, Set,
    };
    let store = setup().await;
    let source = DelegationSource::WorkbenchOwner {
        workbench_id: "workbench".into(),
        input_revision: 1,
    };
    let insert = |task: &str, root: &str, child: &str, source: DelegationSource| {
        let state = SubAgentRun {
            worker_id: "worker".into(),
            child_conversation_id: child.into(),
            actor_id: "1".into(),
            device_id: String::new(),
            name: "Investigate".into(),
            binding: DelegatedTaskBinding {
                root_conversation_id: root.into(),
                group_id: format!("group-{task}"),
                task_id: task.into(),
                source,
                targets: Vec::new(),
                objective: "Check the reported behavior".into(),
                acceptance_criteria: vec!["Summarize the findings".into()],
                input_revision: 1,
                control_revision: 1,
                source_epoch: 1,
                deadline_ms: 123,
            },
            state: SubAgentState::Queued,
            state_revision: 1,
            source_paused: false,
            dependencies: Vec::new(),
            pending_question_id: None,
            partial_report: None,
            terminal_report: None,
            failure_reason: None,
            report_corrections_used: 0,
            created_at: "now".into(),
            updated_at: "now".into(),
        };
        run::ActiveModel {
            task_id: Set(task.into()),
            child_conversation_id: Set(child.into()),
            worker_id: Set("worker".into()),
            workbench_id: Set(state.binding.source.workbench_id().map(str::to_owned)),
            root_conversation_id: Set(root.into()),
            group_id: Set(state.binding.group_id.clone()),
            actor_id: Set("1".into()),
            device_id: Set(String::new()),
            origin_kind: Set("subagent_creation".into()),
            origin_ref: Set(task.into()),
            creation_arguments_sha256: Set(String::new()),
            creation_envelope_json: Set(String::new()),
            state: Set(state.state.as_str().into()),
            input_revision: Set(1),
            control_revision: Set(1),
            source_epoch: Set(1),
            state_revision: Set(1),
            state_json: Set(serde_json::to_string(&state).unwrap()),
            deadline_ms: Set(123),
            created_at: Set(1),
            updated_at: Set(1),
            ..Default::default()
        }
    };
    // A removed work no longer keeps the session.
    let removed = |ids: &[&str]| {
        run::Entity::find()
            .select_only()
            .column(run::Column::RootConversationId)
            .filter(run::Column::RootConversationId.is_in(ids.iter().copied()))
            .into_query()
    };
    insert("first", "work-a", "worker-session", source.clone())
        .insert(&store.db)
        .await
        .unwrap();
    let scope = || worker_request::in_scope("1", "", "", Some("workbench"));
    assert_eq!(
        worker_request::worker_session_on(&store.db, scope(), "worker", &source)
            .await
            .unwrap()
            .as_deref(),
        Some("worker-session")
    );
    // Goal and scheduled requests keep a session of their own.
    let goal = DelegationSource::Goal {
        goal_id: "goal".into(),
    };
    assert!(
        worker_request::worker_session_on(&store.db, scope(), "worker", &goal)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !worker_request::shared_with_other_root_on(
            &store.db,
            "worker-session",
            "work-a",
            removed(&[])
        )
        .await
        .unwrap()
    );
    insert("second", "work-b", "worker-session", source.clone())
        .insert(&store.db)
        .await
        .unwrap();
    assert!(
        worker_request::shared_with_other_root_on(
            &store.db,
            "worker-session",
            "work-a",
            removed(&[])
        )
        .await
        .unwrap()
    );
    assert!(
        !worker_request::shared_with_other_root_on(
            &store.db,
            "worker-session",
            "work-a",
            removed(&["work-b"]),
        )
        .await
        .unwrap()
    );
}
