use desk_diagnose_core::{
    model_observability::{ConfigurationScope, DEFINITION_VERSION, Protocol},
    replay::SourceContextKey,
};

fn adapter(base_url: &str, protocol: WireProtocol) -> AdapterParts {
    let config = crate::model_provider::ModelProviderConfig {
        wire_protocol: Some(protocol),
        model: Some("metrics-model".into()),
        base_url: Some(base_url.into()),
        api_key: Some("fixture-secret".into()),
        max_context_bytes: Some(131_072),
        ..Default::default()
    };
    let source = SourceContextKey::derive_for_endpoint(
        protocol,
        "oss-singleton:1",
        base_url,
        "oss-model:1",
        "metrics-model",
    );
    AdapterParts {
        model: Box::new(crate::model_dial::SignalModelSeam::from_config(&config).unwrap()),
        policy: PinnedContextPolicy::window(source, config.profile_revision, 131_072).unwrap(),
        attribution: Attribution {
            provider_id: "local.agent.1".into(),
            model_id: "local.agent.1".into(),
            model_name: "metrics-model".into(),
            configuration_revision: "1".into(),
            contract_revision: DEFINITION_VERSION.to_string(),
            purpose: Purpose::Agent,
            surface: Surface::Assistant,
            origin: Origin::User,
            configuration_scope: ConfigurationScope::Local,
            protocol: match protocol {
                WireProtocol::OpenAiChatCompletions => Protocol::OpenAiChatCompletions,
                WireProtocol::AnthropicMessages => Protocol::AnthropicMessages,
                WireProtocol::OpenAiResponses => panic!("unsupported production dialect"),
            },
        },
    }
}

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../signal-facade/tests/fixtures/model_metrics_end_to_end.rs"
));
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../signal-facade/tests/fixtures/model_metrics_transport.rs"
));
