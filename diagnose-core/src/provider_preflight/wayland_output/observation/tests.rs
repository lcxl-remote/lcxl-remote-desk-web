use super::*;
use crate::chat::ToolCallRef;
use desk_agent_protocol::data_lineage::*;
use serde_json::json;

fn history() -> Vec<ChatMessage> {
    let mut call = ChatMessage::text("call", ChatRole::Assistant, "");
    call.tool_calls.push(ToolCallRef {
        id: "capture".into(),
        name: "read_current_screen".into(),
        arguments_json: "{}".into(),
    });
    let text = json!({"ReadContext":{"ScreenCaptureCurrent":{
        "display":"wayland-portal-display", "width":200, "height":100, "dpi_x":96, "dpi_y":96,
        "format":"png", "window":null, "window_geometry":null, "image":[], "truncated":false,
        "frame_observation":{"observation_id":"frame", "stream_generation":1, "received_at_unix_ms":1,
            "receipt_age_ms":0, "source_timestamp_ns":null, "freshness":"latest_observed",
            "output_reference":{"token":"output", "snapshot_id":"worker:frame", "object_kind":"desktop_output", "expires_at":""}}
    }}}).to_string();
    let hash = format!("{:x}", Sha256::digest(text.as_bytes()));
    let mut result = ChatMessage::tool_result("receipt", "capture", text.clone());
    result.data_envelope = Some(DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: "observation".into(),
        content: ContentRef::ImmutableBlob {
            blob_id: "blob".into(),
            sha256: hash.clone(),
            size_bytes: text.len() as u64,
            media_type: "application/json".into(),
        },
        provenance: DataProvenance {
            source_provider_id: crate::ai_assistant::CURRENT_SCREEN_PROVIDER_ID.into(),
            source_tool_name: "read_current_screen".into(),
            source_object_id: None,
            source_envelope_ids: vec![],
        },
        digest_sha256: hash,
        sensitivity: Sensitivity::Sensitive,
        allowed_destinations: vec![],
        retention: RetentionBoundary {
            expires_at_unix_ms: Some(100),
            delete_with_run: true,
        },
    });
    vec![call, result]
}

#[test]
fn intact_output_observation_allows_best_effort_without_relabeling() {
    let history = history();
    let (reference, screen, frame) = bind_observation(&history, "output", 2).unwrap();
    assert_eq!(reference.object_kind, ObjectKind::DesktopOutput);
    assert_eq!(screen.width, 200);
    let action = WaylandOutputInputAction {
        screen,
        frame,
        step: desk_agent_protocol::computer_use::RawInputStep::KeyPress {
            key: desk_agent_protocol::computer_use::RawInputKey::Enter,
        },
    };
    assert_eq!(
        action.frame.freshness,
        desk_agent_protocol::ScreenFrameFreshness::LatestObserved
    );
    assert!(action.validate().is_ok());
    let registry = crate::ai_assistant::ai_assistant_provider_registry();
    let mut call = crate::chat::ToolCall {
        id: "input".into(),
        name: crate::ai_assistant::linux::OUTPUT_TOOL.into(),
        arguments_json: json!({"target": reference, "action": action}).to_string(),
    };
    for surface in [
        ProductSurface::OssPersonalOwner,
        ProductSurface::ManagerPersonalOwner,
    ] {
        assert!(
            WaylandOutputInputPreflight::from_history(&registry, surface, &call, &history, 2)
                .is_ok()
        );
    }
    // Approval and model arguments must preserve the actual observation label.
    call.arguments_json = call.arguments_json.replace("latest_observed", "fresh");
    assert!(
        WaylandOutputInputPreflight::from_history(
            &registry,
            ProductSurface::OssPersonalOwner,
            &call,
            &history,
            2
        )
        .is_err()
    );
    assert!(bind_observation(&history, "invented", 2).is_err());
    assert!(bind_observation(&history, "output", 101).is_err());
}

#[test]
fn altered_or_wrong_source_observations_cannot_supply_output_references() {
    for alteration in 0..4 {
        let mut history = history();
        match alteration {
            0 => history[1].text = history[1].text.replace("latest_observed", "fresh"),
            1 => {
                history[1]
                    .data_envelope
                    .as_mut()
                    .unwrap()
                    .provenance
                    .source_tool_name = "inspect_desktop_ui".into()
            }
            2 => history[1].role = ChatRole::User,
            _ => history[0].tool_calls.clear(),
        }
        assert!(bind_observation(&history, "output", 2).is_err());
    }
}
