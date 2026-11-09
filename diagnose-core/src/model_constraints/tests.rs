use super::*;
use crate::{ai_assistant::data_tools, registry::RegisteredTool};
use desk_agent_protocol::ai_assistant::limits::AssistantDataLimits;

fn tools(limits: &AssistantDataLimits) -> Vec<RegisteredTool> {
    let mut tools = crate::read_tools::read_tool_registry();
    tools.extend(crate::read_tools::ai_assistant_read_tool_registry());
    tools.extend(crate::ai_assistant::ai_assistant_provider_registry().registered_tools());
    tools.extend(crate::permission_tools::permission_planning_tool_registry());
    tools.extend(crate::task_status_tools::task_status_tool_registry());
    tools.extend(crate::subagent::tools::registry());
    tools.extend(crate::capability_disclosure::capability_discovery_tool_registry());
    tools.extend(crate::goal_tools::registry());
    tools.extend(crate::goal_tools::open_registry());
    tools.extend(crate::directory_tools::registry());
    tools.extend(crate::schedule::proposal::registry());
    tools.extend(crate::conversation_history::conversation_history_tool_registry());
    tools.extend(crate::wait_tools::wait_tool_registry());
    tools.extend(
        crate::schedule::management_tools::specs()
            .into_iter()
            .map(|spec| RegisteredTool {
                spec,
                required_device_capability: None,
                effect: crate::registry::ToolEffect::ReadOnly,
            }),
    );
    tools.extend(data_tools::memory_registry(limits));
    tools.extend(data_tools::conversation_registry(limits));
    tools
}

#[test]
fn configured_limits_stay_in_schema_and_descriptions_use_one_reminder() {
    for (groups, characters, hits, reads, references) in [(2, 81, 7, 11, 1), (6, 300, 13, 60, 9)] {
        let mut limits = AssistantDataLimits::default();
        limits.retrieval.query_groups = groups;
        limits.retrieval.query_chars = characters;
        limits.retrieval.relevance_hits = hits;
        limits.retrieval.read_messages = reads;
        limits.memory.source_refs = references;
        let mut registry = tools(&limits);
        for tool in &mut registry {
            crate::ui_model_ids::project_tool(&mut tool.spec);
        }
        let find = |name: &str| {
            &registry
                .iter()
                .find(|tool| tool.name() == name)
                .unwrap()
                .spec
        };
        let search = find(data_tools::SEARCH_MEMORIES);
        assert_eq!(
            search.parameters_schema["properties"]["queries"]["maxItems"],
            groups
        );
        assert_eq!(
            search.parameters_schema["properties"]["queries"]["items"]["maxLength"],
            characters
        );
        assert_eq!(
            search.parameters_schema["properties"]["limit"]["maximum"],
            hits
        );
        assert!(
            search.parameters_schema["properties"]["queries"]
                .get("description")
                .is_none()
        );
        let history = find(data_tools::READ_MEMORY_HISTORY);
        assert_eq!(
            history.parameters_schema["properties"]["limit"]["maximum"],
            reads
        );
        assert!(
            history.parameters_schema["properties"]["limit"]
                .get("description")
                .is_none()
        );
        let write = find(data_tools::MANAGE_MEMORIES);
        for variant in write.parameters_schema["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .take(2)
        {
            assert_eq!(
                variant["properties"]["source_refs"]["maxItems"],
                references - 1
            );
            assert_eq!(
                variant["x-maxCombinedUtf8Bytes"]["fields"],
                json!(["content", "context"])
            );
            assert_eq!(
                variant["x-maxCombinedUtf8Bytes"]["maximum"],
                limits.memory.draft_bytes
            );
        }
        for tool in registry
            .iter()
            .filter(|tool| data_tools::is_data_tool(tool.name()))
        {
            assert_eq!(tool.spec.description.matches(SCHEMA_REMINDER).count(), 1);
            assert!(!tool.spec.description.contains("keyword groups,"));
            assert!(!tool.spec.description.contains("Unicode characters/group"));
            assert!(!tool.spec.description.contains("at most"));
            assert!(!tool.spec.description.contains("pages allow"));
        }
    }
}

#[test]
fn native_limits_keep_their_schema_values_without_numeric_captions() {
    let mut registry = tools(&AssistantDataLimits::default());
    for tool in &mut registry {
        crate::ui_model_ids::project_tool(&mut tool.spec);
    }
    let services = &registry
        .iter()
        .find(|tool| tool.name() == "read_service_status")
        .unwrap()
        .spec;
    for filter in [
        "queries",
        "start_types",
        "unit_file_states",
        "launch_policies",
    ] {
        let field = &services.parameters_schema["properties"][filter];
        assert_eq!(field["maxItems"], 16);
        assert!(field.get("description").is_none());
    }
    let permission = &registry
        .iter()
        .find(|tool| tool.name() == "request_permissions")
        .unwrap()
        .spec;
    let actions = permission
        .parameters_schema
        .pointer("/properties/items/items/properties/application_scope/properties/actions")
        .unwrap();
    assert_eq!(actions["minItems"], 1);
    assert_eq!(actions["maxItems"], 6);
    assert!(actions.get("description").is_none());
    assert!(!permission.description.contains("at most 14"));
    let mut native = ToolSpec {
        name: "create_text_artifact".into(),
        description: "Create text".into(),
        parameters_schema: json!({"type":"object","properties":{"content_utf8":{"type":"string"}}}),
    };
    project(&mut native);
    assert_eq!(
        native.parameters_schema["properties"]["content_utf8"]["x-maxUtf8Bytes"],
        65536
    );
    assert!(
        native.parameters_schema["properties"]["content_utf8"]
            .get("description")
            .is_none()
    );
    let input = json!({"content_utf8":"x".repeat(65537)});
    let error = crate::model_input::validate_format_reported(
        &native.name,
        &native.parameters_schema,
        &input,
    )
    .unwrap_err();
    assert!(error.message.contains("$.content_utf8"));
    assert!(
        error
            .message
            .contains("maximum 65536 UTF-8 bytes, observed 65537")
    );
}

#[test]
fn conditions_and_literal_data_are_unchanged_by_the_reminder() {
    let literal = json!({"type":"array","maxItems":99,"properties":{"text":{"type":"string"}}});
    let mut tool = ToolSpec {
        name: "sample".into(),
        description: "Sample tool".into(),
        parameters_schema: json!({"type":"object","properties":{
        "queries":{"type":"array","maxItems":5},
        "payload":{"type":"object","const":literal,"default":literal,"examples":[literal],"enum":[literal]},
        "selection":{"oneOf":[{"type":"array","minItems":1,"maxItems":1},{"type":"array","minItems":2,"maxItems":3}]}
    },"if":{"required":["element_only"]},"then":{"properties":{"queries":{"maxItems":0}}},"$defs":{"options":{"type":"array","minItems":2}}}),
    };
    let original = tool.parameters_schema.clone();
    project(&mut tool);
    assert_eq!(tool.parameters_schema, original);
    assert_eq!(tool.description, format!("Sample tool {SCHEMA_REMINDER}"));
    project(&mut tool);
    assert_eq!(tool.parameters_schema, original);
    assert_eq!(tool.description, format!("Sample tool {SCHEMA_REMINDER}"));
}

#[test]
fn already_batched_tools_get_one_reminder_without_rewrapping_the_schema() {
    let registry = crate::ai_assistant::ai_assistant_provider_registry();
    for name in ["execute_ui_actions", "send_background_input"] {
        let mut tool = registry
            .capability_for_tool(name)
            .unwrap()
            .registered_tool()
            .spec;
        crate::ui_model_ids::project_tool(&mut tool);
        let first = tool.clone();
        crate::ui_model_ids::project_tool(&mut tool);
        assert_eq!(tool, first);
        assert_eq!(tool.description.matches(SCHEMA_REMINDER).count(), 1);
        assert!(!tool.description.contains("1–20"));
        assert_eq!(
            tool.parameters_schema["properties"]["steps"]["maxItems"],
            crate::application_batch::MAX_STEPS
        );
        assert!(
            tool.parameters_schema["properties"]["steps"]
                .get("description")
                .is_none()
        );
    }
}

#[test]
fn search_mode_limits_and_compound_metadata_use_the_configured_schema() {
    let mut limits = AssistantDataLimits::default();
    limits.retrieval.relevance_hits = 3;
    limits.retrieval.time_hits = 8;
    limits.retrieval.search_window_days = 2;
    limits.memory.draft_bytes = 8;
    let mut search = data_tools::conversation_registry(&limits).remove(0).spec;
    crate::ui_model_ids::project_tool(&mut search);
    assert_eq!(
        search.parameters_schema["properties"]["time_range"]["x-maxSpanDays"],
        2
    );
    for (sort, maximum) in [(None, 3), (Some("relevance"), 3), (Some("time"), 8)] {
        let mut input = json!({"queries":["topic"],"limit":maximum});
        if let Some(sort) = sort {
            input["sort"] = json!(sort);
        }
        crate::model_input::validate_format_reported(
            &search.name,
            &search.parameters_schema,
            &input,
        )
        .unwrap();
        input["limit"] = json!(maximum + 1);
        let error = crate::model_input::validate_format_reported(
            &search.name,
            &search.parameters_schema,
            &input,
        )
        .unwrap_err();
        assert!(error.message.contains("$.limit"));
        assert!(error.message.contains(&format!("maximum={maximum}")));
    }
    let write = data_tools::memory_registry(&limits)
        .into_iter()
        .find(|tool| tool.name() == data_tools::MANAGE_MEMORIES)
        .unwrap()
        .spec;
    assert_eq!(
        write.parameters_schema["oneOf"][0]["x-maxCombinedUtf8Bytes"]["maximum"],
        8
    );
    let call = crate::chat::ToolCall {
        id: "write".into(),
        name: data_tools::MANAGE_MEMORIES.into(),
        arguments_json: json!({"operation":"remember","content":"abcde","context":"fghi"})
            .to_string(),
    };
    assert!(data_tools::parse_with_limits(&call, &limits, &Default::default()).is_err());
}

#[test]
fn schema_errors_identify_the_field_and_expected_limits() {
    let mut limits = AssistantDataLimits::default();
    limits.retrieval.query_groups = 2;
    limits.retrieval.query_chars = 3;
    limits.retrieval.relevance_hits = 5;
    let mut search = data_tools::memory_registry(&limits).remove(0).spec;
    crate::ui_model_ids::project_tool(&mut search);
    for (input, path, bound) in [
        (
            json!({"queries":["a","b","c"],"limit":1}),
            "$.queries",
            "maxItems=2",
        ),
        (
            json!({"queries":["abcd"],"limit":1}),
            "$.queries[0]",
            "maxLength=3",
        ),
        (json!({"queries":["a"],"limit":6}), "$.limit", "maximum=5"),
        (json!({"limit":1}), "$.queries", "missing required field"),
        (
            json!({"queries":["a"],"limit":"wrong"}),
            "$.limit",
            "expected type",
        ),
        (
            json!({"queries":["a"],"limit":1,"scope":"wrong"}),
            "$.scope",
            "allowed values",
        ),
    ] {
        let error = crate::model_input::validate_format_reported(
            &search.name,
            &search.parameters_schema,
            &input,
        )
        .unwrap_err();
        assert!(error.message.contains(path), "{error:?}");
        assert!(error.message.contains(bound), "{error:?}");
    }
}

#[test]
fn all_registered_tool_projections_remain_idempotent_on_one_and_two_mib_stacks() {
    for stack in [1024 * 1024, 2 * 1024 * 1024] {
        std::thread::Builder::new()
            .stack_size(stack)
            .spawn(|| {
                let mut projected = 0;
                for mut registered in tools(&AssistantDataLimits::default()) {
                    crate::ui_model_ids::project_tool(&mut registered.spec);
                    let first = registered.spec.clone();
                    crate::ui_model_ids::project_tool(&mut registered.spec);
                    assert_eq!(registered.spec, first, "{}", first.name);
                    assert_eq!(
                        first.description.matches(SCHEMA_REMINDER).count(),
                        1,
                        "{}",
                        first.name
                    );
                    projected += 1;
                }
                assert!(projected > 50);
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
