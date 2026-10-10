//! Opt-in protocol regression with two real calls and no tool execution.
use super::*;
use desk_diagnose_core::{chat::ToolSpec, seam::NullTurnSink};
use std::time::{Duration, Instant};

#[actix_web::test]
#[ignore = "requires explicit project-model credentials and public network access"]
async fn live_project_model_thinking_replay() {
    let config = ModelProviderConfig {
        wire_protocol: Some(WireProtocol::OpenAiChatCompletions),
        model: Some(std::env::var("LRD_LIVE_DEEPSEEK_MODEL").unwrap()),
        base_url: Some(std::env::var("LRD_LIVE_DEEPSEEK_BASE_URL").unwrap()),
        api_key: Some(std::env::var("LRD_LIVE_DEEPSEEK_API_KEY").unwrap()),
        request_options: json!({"thinking":{"type":"enabled"}}),
        max_context_bytes: Some(131_072),
        runtime_max_output_tokens: 1024,
        ..Default::default()
    };
    let seam = SignalModelSeam::from_config(&config).unwrap();
    let tool = ToolSpec {
        name: "read_supplied_result".into(),
        description: "Read a supplied test result only if it is missing from the conversation."
            .into(),
        parameters_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
    };
    let started = Instant::now();
    let mut request = ModelRequest::text_only(
        vec![
            ChatMessage::text(
                "system",
                ChatRole::System,
                "Only use supplied information. Do not call tools when the answer is already supplied. Keep replies short.",
            ),
            ChatMessage::text(
                "owner",
                ChatRole::User,
                "A background task is calculating 17+25. Its result will arrive later. For now respond only: waiting. Do not call any tools.",
            ),
        ],
        ResponseFormatSpec::None,
    );
    request.tools = vec![tool];
    let first = tokio::time::timeout(
        Duration::from_secs(90),
        seam.call(request.clone(), &mut NullTurnSink),
    )
    .await
    .expect("first model call timed out")
    .expect("first thinking call failed");
    assert_eq!(first.stop_reason, StopReason::EndTurn);
    assert!(first.tool_calls.is_empty());
    let Some(ReplayDisposition::Present { envelope }) = &first.provider_meta.replay else {
        panic!("first plain answer must contain opaque thinking replay");
    };
    let replay_bytes = envelope.payload.as_str().unwrap().len();
    assert!(replay_bytes > 0, "thinking must actually be enabled");
    let mut message = ChatMessage::text("first-answer", ChatRole::Assistant, first.text.clone());
    message.reasoning = first.provider_meta.display_reasoning.clone();
    message.replay_disposition = first.provider_meta.replay.clone();
    // Reopen the serialized message before feeding it into the normal request builder.
    let restored: ChatMessage =
        serde_json::from_str(&serde_json::to_string(&message).unwrap()).unwrap();
    request.messages.push(restored);
    request.messages.push(ChatMessage::system_event(
        "completion",
        "The background subagent completed: 17+25=42. Return only 42. Do not call tools.",
    ));
    let body = build_openai_body_profiled(
        &config.model.clone().unwrap(),
        &request,
        WireProtocol::OpenAiChatCompletions,
        &seam.profile,
        resolve_effective_output_limit(
            request.use_case,
            seam.profile.runtime_max_output_tokens,
            request.caller_output_hard_cap,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        body["messages"][2]["reasoning_content"]
            .as_str()
            .map(str::len),
        Some(replay_bytes)
    );
    assert!(body["messages"][2].get("tool_calls").is_none());
    let second = tokio::time::timeout(
        Duration::from_secs(90),
        seam.call(request, &mut NullTurnSink),
    )
    .await
    .expect("completion model call timed out")
    .expect("thinking completion replay failed");
    let passed = second.stop_reason == StopReason::EndTurn
        && second.tool_calls.is_empty()
        && second.text.trim() == "42";
    let evidence = json!({
        "scope":"production_oss_model_transport_real_provider_synthetic_completion_serialized_message_no_device_io",
        "profile":{"model":config.model,"thinking":"enabled","output_limit":1024},
        "results":[{"case":"thinking_replay","passed":passed,"functional_passed":passed,
            "model_calls":2,"plain_answer_replay_bytes":replay_bytes,"reopened_replay_matches":true,
            "first_usage":first.usage,"second_usage":second.usage,"elapsed_ms":started.elapsed().as_millis()}]
    });
    std::fs::write(
        std::env::var("LRD_LIVE_SUBAGENT_EVIDENCE_PATH").unwrap(),
        serde_json::to_string_pretty(&evidence).unwrap(),
    )
    .unwrap();
    assert!(
        passed,
        "completion must return the supplied result without tools"
    );
}
