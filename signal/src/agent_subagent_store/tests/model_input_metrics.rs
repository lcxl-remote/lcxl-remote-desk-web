async fn configure_limit(db: &crate::config::connection::DatabaseConnection, unfinished: u32) {
    let current = crate::subagent_policy::read(db).await.unwrap();
    crate::subagent_policy::update(
        db,
        &desk_agent_protocol::ai_assistant::subagent_policy::UpdateSubAgentPolicy {
            expected_revision: current.revision,
            limits: desk_agent_protocol::ai_assistant::subagent_policy::SubAgentLimits {
                max_unfinished_per_root: unfinished,
            },
        },
    )
    .await
    .unwrap();
}

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../signal-facade/tests/fixtures/model_metrics_subagent_inputs.rs"
));
