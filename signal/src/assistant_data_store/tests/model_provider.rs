use super::*;
use crate::entity::agent_session as session_row;
#[cfg(test)]
use desk_assistant_data_store::entity_table::EntityTableExt;
use desk_diagnose_core::model_profile::WireProtocol;
use sea_orm::sea_query::Expr;

pub(super) async fn configure_model(
    db: &crate::config::connection::DatabaseConnection,
    base_url: &str,
) -> crate::model_provider::ModelProviderConfig {
    crate::ai_assistant_gate::enable_test_host();
    let schema = Schema::new(db.get_database_backend());
    db.execute(
        schema
            .entity_table(crate::entity::model_probe_observation::Entity)
            .if_not_exists(),
    )
    .await
    .unwrap();
    let mut config = db.config_read().await.ai_gateway.clone();
    config.wire_protocol = Some(WireProtocol::OpenAiChatCompletions);
    config.model = Some("owner-model".into());
    config.base_url = Some(base_url.into());
    config.api_key = Some("private-fixture-credential".into());
    config.max_context_bytes = Some(131072);
    config.runtime_max_output_tokens = 16_384;
    crate::model_provider::save(db, config).await.unwrap();
    crate::model_provider::load(db).await.unwrap()
}

pub(super) async fn reserve_main_budget(
    db: &crate::config::connection::DatabaseConnection,
    session: &mut desk_diagnose_core::session::PersistedAgentSession,
) -> desk_diagnose_core::subagent::reservation::DelegationCallReservation {
    session.control_revision = session.control_revision.max(1);
    session.delegation_group_id = None;
    let schema = sea_orm::Schema::new(db.get_database_backend());
    for mut table in [
        schema.entity_table(crate::entity::agent_delegation_group::Entity),
        schema.entity_table(crate::entity::agent_delegation_reservation::Entity),
    ] {
        db.execute(table.if_not_exists()).await.unwrap();
    }
    let txn = crate::db::begin_write(db, session_row::Entity)
        .await
        .unwrap();
    let owner = session
        .conversation
        .iter()
        .rev()
        .find(|m| m.role == desk_diagnose_core::chat::ChatRole::User)
        .unwrap()
        .clone();
    crate::agent_subagent_store::initialize_input_group_on(
        &txn,
        session,
        owner,
        None,
        None,
        chrono::Utc::now().timestamp_millis(),
        Default::default(),
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    let row = session_row::Entity::find().one(db).await.unwrap().unwrap();
    session_row::Entity::update_many()
        .col_expr(
            session_row::Column::StateJson,
            Expr::value(session.encode_json_for_storage().unwrap()),
        )
        .filter(session_row::Column::Id.eq(row.id))
        .filter(session_row::Column::StateJson.eq(row.state_json))
        .exec(db)
        .await
        .unwrap();
    let admitted = crate::agent_subagent_store::SubAgentStore::new(db.clone())
        .reserve_runtime_call(
            session,
            "main-original-call",
            desk_diagnose_core::subagent::reservation::DelegationCallKind::Model,
            &"a".repeat(64),
            desk_diagnose_core::goal::GoalUsage {
                input_tokens: 100,
                model_calls: 1,
                ..Default::default()
            },
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .unwrap();
    let desk_diagnose_core::subagent::reservation::CallAdmission::Reserved(call) = admitted else {
        panic!("actual main budget expected");
    };
    call
}
