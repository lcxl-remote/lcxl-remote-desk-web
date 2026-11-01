#[tokio::test]
async fn conversation_alias_filters_real_history_without_rewriting_native_producer_arguments() {
    use desk_diagnose_core::chat::{ChatMessage, ToolCallRef};
    use sea_orm::sea_query::Expr;
    let mut frame = Frame::configured("alias-search", true, |_| {
        vec![ToolCall {
            id: "alias-query".into(),
            name: data_tools::SEARCH_CONVERSATIONS.into(),
            arguments_json: r#"{ "queries": ["report"], "conversation_ids": ["cv1"] }"#.into(),
        }]
    })
    .await;
    let selected = capture(
        &frame,
        "past-A",
        "alias-A",
        "report selected",
        frame.now - 20,
    )
    .await;
    capture(
        &frame,
        "past-B",
        "alias-B",
        "report excluded",
        frame.now - 10,
    )
    .await;
    let call = ChatMessage::assistant_tool_calls(
        "alias-source",
        "",
        vec![ToolCallRef {
            id: "alias-observation".into(),
            name: "read_conversation".into(),
            arguments_json: "{}".into(),
        }],
    );
    let mut observation = ChatMessage::tool_result(
        "alias-result",
        "alias-observation",
        serde_json::json!({"conversation_id":"past-A"}).to_string(),
    );
    observation.data_envelope =
        desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
            frame
                .session
                .conversation
                .iter()
                .find_map(|m| m.data_envelope.as_ref()),
            "alias-observation",
            &observation.text,
            "read_conversation",
        )
        .unwrap();
    frame.session.conversation.extend([call, observation]);
    frame
        .session
        .ui_references
        .observe(&frame.session.conversation)
        .unwrap();
    frame.session.ui_references =
        serde_json::from_str(&serde_json::to_string(&frame.session.ui_references).unwrap())
            .unwrap();
    agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::StateJson,
            Expr::value(frame.session.encode_json_for_storage().unwrap()),
        )
        .filter(agent_session::Column::ConversationId.eq(&frame.session.conversation_id))
        .exec(&frame.store.db)
        .await
        .unwrap();
    let raw = frame.context().tool_call.arguments_json.clone();
    let result = search::search(&frame.store.db, frame.context(), &request(&frame, 0))
        .await
        .unwrap();
    assert_eq!(
        result
            .result
            .hits
            .iter()
            .map(|h| h.source_id.clone())
            .collect::<Vec<_>>(),
        vec![selected]
    );
    assert_eq!(frame.context().tool_call.arguments_json, raw);
    let proof = result.projection.as_ref().unwrap();
    assert_eq!(
        proof.request_sha256,
        desk_diagnose_core::ai_assistant::read_projection::request_sha256(
            data_tools::SEARCH_CONVERSATIONS,
            &raw
        )
        .unwrap()
    );
    let replay = search::search(&frame.store.db, frame.context(), &request(&frame, 0))
        .await
        .unwrap();
    assert_eq!(replay.result, result.result);
    assert_eq!(replay.projection, result.projection);
}
