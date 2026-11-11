use super::*;

/// The seam normalizes an agent turn before the loop sees it: an optional null
/// is omitted and an unknown read-only field is ignored with a notice.
#[actix_web::test]
async fn agent_turns_are_normalized_before_the_loop_sees_them() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = actix_web::HttpServer::new(|| {
        actix_web::App::new().route(
            "/v1/chat/completions",
            actix_web::web::post().to(|| async {
                let arguments = serde_json::json!({"queries":["Calculator"],"verbose":true,"max_depth":null}).to_string();
                let delta = serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"inspect_desktop_ui","arguments":arguments}}]},"finish_reason":"tool_calls"}]});
                actix_web::HttpResponse::Ok().content_type("text/event-stream").body(format!(
                    "data: {delta}\n\ndata: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":5,\"completion_tokens\":2}}}}\n\ndata: [DONE]\n\n"
                ))
            }),
        )
    })
    .workers(1)
    .listen(listener)
    .unwrap()
    .run();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    let config = crate::model_provider::ModelProviderConfig {
        wire_protocol: Some(desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions),
        model: Some("fake-model".into()),
        base_url: Some(format!("http://{address}/v1")),
        api_key: Some("fixture-key".into()),
        max_context_bytes: Some(131_072),
        runtime_max_output_tokens: 4096,
        ..Default::default()
    };
    let model = MeteredSignalModel {
        inner: SignalModelSeam::from_config(&config).unwrap(),
    };
    let tool = desk_diagnose_core::ai_assistant::ai_assistant_provider_registry()
        .capability_for_tool("inspect_desktop_ui")
        .unwrap()
        .registered_tool();
    let mut request = ModelRequest::text_only(
        vec![desk_diagnose_core::chat::ChatMessage::text(
            "user",
            desk_diagnose_core::chat::ChatRole::User,
            "Find Calculator",
        )],
        desk_diagnose_core::prompt::ResponseFormatSpec::None,
    );
    request.tools = vec![tool.spec];
    request.tool_requirements =
        desk_diagnose_core::model_capability::ModelRequirements::for_registered_tools(
            std::iter::once(&tool_for_requirements()),
        );
    let turn = model
        .call(request, &mut desk_diagnose_core::seam::NullTurnSink)
        .await
        .unwrap();
    handle.stop(true).await;
    let arguments: serde_json::Value =
        serde_json::from_str(&turn.tool_calls[0].arguments_json).unwrap();
    assert_eq!(arguments, serde_json::json!({"queries":["Calculator"]}));
    assert_eq!(
        turn.provider_meta.normalized_inputs["call-1"].notice,
        "Ignored unsupported input fields: verbose. Null optional fields were treated as omitted: max_depth."
    );
}

fn tool_for_requirements() -> desk_diagnose_core::registry::RegisteredTool {
    desk_diagnose_core::ai_assistant::ai_assistant_provider_registry()
        .capability_for_tool("inspect_desktop_ui")
        .unwrap()
        .registered_tool()
}
