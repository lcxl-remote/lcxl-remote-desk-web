//! Compact model-only UI receipts. Native and controller wire references stay intact.

use desk_agent_protocol::{OperationOutput, ReadContextOutput};
use serde_json::{Value, json};

pub fn serialize(output: &OperationOutput) -> Result<String, serde_json::Error> {
    let OperationOutput::ReadContext(ReadContextOutput::DesktopUiInspect(ui)) = output else {
        return serde_json::to_string(output);
    };
    let mut value = serde_json::to_value(output)?;
    add_window_discovery_hint(&mut value);
    let body = &mut value["ReadContext"]["DesktopUiInspect"];
    if ui.nodes.is_empty() {
        body["search_hint"] = json!(
            "No results matched this bounded query. If your root is a macOS DesktopSession, this searched running application names only, not controls: when truncated=false and known localized/English application names have no match, open the requested application through an authorized launch tool, then search the session again. Do not increase UI depth or search button/date labels to find a non-running app. A truncated listing or read error does not prove absence. If your root is an application/window/control, this does not establish that the UI or operation is unsupported. Try queries with both localized and English candidates (up to 16), because native identifiers often remain English even on a Chinese UI: 日期/时间/date/time/input/dialog/popover, or an observed native_id. Locate a dialog/popover then search within its root. Use allow_unfiltered=true only explicitly when targeted searches are insufficient."
        );
    }
    if ui.truncated {
        body["truncation_hint"] = json!(
            "This bounded UI tree is incomplete. Missing display text does not mean no result. Inspect the exact window with a larger max_depth (at least 12) and adequate max_nodes/max_bytes before deciding the next action."
        );
    }
    if let Some(first) = ui.nodes.first() {
        let defaults = json!({
            "snapshot_id": first.object_ref.snapshot_id,
            "object_kind": first.object_ref.object_kind,
            "expires_at": first.object_ref.expires_at,
        });
        body["reference_defaults"] = defaults.clone();
        body["element_id_is_reference_token"] = json!(true);
        body["node_defaults"] = json!({"enabled":true,"is_protected":false});
        for node in body["nodes"].as_array_mut().expect("serialized UI nodes") {
            let element_id = node.get("element_id").cloned();
            let reference = node["object_ref"]
                .as_object_mut()
                .expect("serialized reference");
            if element_id
                .as_ref()
                .is_some_and(|id| reference.get("token") == Some(id))
            {
                reference.remove("token");
            }
            for key in ["snapshot_id", "object_kind", "expires_at"] {
                if reference.get(key) == defaults.get(key) {
                    reference.remove(key);
                }
            }
            let fields = node.as_object_mut().expect("serialized UI node");
            fields.retain(|key, value| {
                !value.is_null()
                    && !(key == "enabled" && value == &Value::Bool(true))
                    && !(key == "is_protected" && value == &Value::Bool(false))
                    && !(key == "supported_actions" && value.as_array().is_some_and(Vec::is_empty))
            });
        }
    }
    serde_json::to_string(&value)
}

/// Apply after reference expansion, before projecting IDs for the model.
pub(crate) fn add_window_discovery_hint(value: &mut Value) {
    let Some(body) = value
        .pointer_mut("/ReadContext/DesktopUiInspect")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    if body
        .get("owner_selectable_windows")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        body.remove("owner_selectable_windows");
    }
    let applications_only = body
        .get("nodes")
        .and_then(Value::as_array)
        .is_some_and(|nodes| {
            !nodes.is_empty()
                && nodes.iter().all(|node| {
                    node.pointer("/object_ref/object_kind").or_else(|| {
                        body.get("reference_defaults")
                            .and_then(|defaults| defaults.get("object_kind"))
                    }) == Some(&json!("application"))
                })
        });
    if applications_only {
        body.insert("window_discovery_hint".into(), json!(
            "This is an application catalog; windows have not been queried. Use the returned application object_ref.id as inspect_desktop_ui root_id with queries=[\"窗口\",\"window\"] to discover its windows. Then pass owner_selectable_windows[].object_ref.id as read_current_screen window_id. An application ID is not a window ID. Missing window entries here do not mean the application has no windows or cannot be captured."
        ));
    }
}

/// Expand only the model representation for existing typed validators.
pub fn deserialize(text: &str) -> Result<OperationOutput, serde_json::Error> {
    let mut value: Value = serde_json::from_str(text)?;
    expand_value(&mut value);
    serde_json::from_value(value)
}

/// Restore internal reference defaults without allocating the large operation enum.
pub fn expand_value(value: &mut Value) {
    if let Some(body) = value
        .pointer_mut("/ReadContext/DesktopUiInspect")
        .and_then(Value::as_object_mut)
    {
        body.remove("truncation_hint");
        body.remove("search_hint");
        body.remove("window_discovery_hint");
        let stable_token = body.remove("element_id_is_reference_token") == Some(json!(true));
        if let Some(defaults) = body.remove("reference_defaults") {
            body.remove("node_defaults");
            if let Some(nodes) = body.get_mut("nodes").and_then(Value::as_array_mut) {
                for node in nodes {
                    let element_id = node.get("element_id").cloned();
                    if let (Some(base), Some(reference)) = (
                        defaults.as_object(),
                        node.get_mut("object_ref").and_then(Value::as_object_mut),
                    ) {
                        if stable_token && let Some(id) = element_id {
                            reference.entry("token").or_insert(id);
                        }
                        for (key, value) in base {
                            reference.entry(key.clone()).or_insert(value.clone());
                        }
                    }
                    if let Some(fields) = node.as_object_mut() {
                        fields.entry("enabled").or_insert(json!(true));
                        fields.entry("is_protected").or_insert(json!(false));
                        fields.entry("supported_actions").or_insert(json!([]));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::computer_use::*;

    #[test]
    fn window_receipts_keep_capture_ids_and_do_not_claim_catalog_results() {
        let windows = json!([{"object_ref":{"token":"window-1","object_kind":"window"}}]);
        let mut value = json!({"ReadContext":{"DesktopUiInspect":{
            "nodes":[{"object_ref":{"object_kind":"ui_element"}}],
            "owner_selectable_windows":windows.clone()
        }}});
        add_window_discovery_hint(&mut value);
        let body = &value["ReadContext"]["DesktopUiInspect"];
        assert_eq!(body["owner_selectable_windows"], windows);
        assert!(body.get("window_discovery_hint").is_none());
    }

    #[test]
    fn application_catalog_retains_state_without_search_match_indices() {
        for state in [
            ApplicationState::Foreground,
            ApplicationState::Background,
            ApplicationState::Hidden,
        ] {
            let output = OperationOutput::ReadContext(ReadContextOutput::DesktopUiInspect(
                UiInspectOutput {
                    snapshot_id: "catalog".into(),
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::MacosAccessibility,
                        version: "test".into(),
                    },
                    nodes: vec![UiNodeProjection {
                        application_state: Some(state),
                        element_id: None,
                        matched_queries: vec![],
                        collapsed_children: 0,
                        native_id: None,
                        object_ref: ObjectRef {
                            token: "app".into(),
                            snapshot_id: "identity".into(),
                            object_kind: ObjectKind::Application,
                            expires_at: String::new(),
                        },
                        parent_index: None,
                        role: "application".into(),
                        name: Some("Calendar".into()),
                        value: None,
                        is_protected: false,
                        enabled: true,
                        supported_actions: vec![],
                    }],
                    owner_selectable_windows: vec![],
                    truncated: false,
                },
            ));
            let encoded = serialize(&output).unwrap();
            assert!(!encoded.contains("matched_queries"));
            assert!(!encoded.contains("\"owner_selectable_windows\":"));
            let mut projected = serde_json::from_str(&encoded).unwrap();
            expand_value(&mut projected);
            add_window_discovery_hint(&mut projected);
            assert!(
                projected["ReadContext"]["DesktopUiInspect"]["window_discovery_hint"]
                    .as_str()
                    .unwrap()
                    .contains("windows have not been queried")
            );
            let value: Value = serde_json::from_str(&encoded).unwrap();
            assert_eq!(
                value["ReadContext"]["DesktopUiInspect"]["nodes"][0]["application_state"],
                serde_json::to_value(state).unwrap()
            );
            assert_eq!(deserialize(&encoded).unwrap(), output);
        }
    }

    #[test]
    fn empty_search_guidance_distinguishes_app_discovery_from_control_search() {
        for truncated in [false, true] {
            let output = OperationOutput::ReadContext(ReadContextOutput::DesktopUiInspect(
                UiInspectOutput {
                    snapshot_id: "snapshot".into(),
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::MacosAccessibility,
                        version: "test".into(),
                    },
                    nodes: vec![],
                    owner_selectable_windows: vec![],
                    truncated,
                },
            ));
            let encoded = serialize(&output).unwrap();
            assert!(encoded.contains("running application names only"));
            assert!(encoded.contains("truncated=false"));
            assert!(encoded.contains("A truncated listing or read error does not prove absence"));
            assert_eq!(deserialize(&encoded).unwrap(), output);
        }
    }

    #[test]
    fn compact_receipt_preserves_exact_references_and_non_default_states() {
        let nodes = (0..100)
            .map(|i| UiNodeProjection {
                application_state: None,
                element_id: (i % 2 == 0).then(|| format!("opaque-token-{i}")),
                matched_queries: Vec::new(),
                collapsed_children: 0,
                native_id: None,
                object_ref: ObjectRef {
                    token: format!("opaque-token-{i}"),
                    snapshot_id: "snapshot".into(),
                    object_kind: ObjectKind::UiElement,
                    expires_at: if i == 99 {
                        "2026-09-09T01:00:01Z"
                    } else {
                        "2026-09-09T01:00:00Z"
                    }
                    .into(),
                },
                parent_index: (i > 0).then_some(0),
                role: "AXButton".into(),
                name: Some(i.to_string()),
                value: None,
                is_protected: i == 99,
                enabled: i != 99,
                supported_actions: vec![],
            })
            .collect();
        let output =
            OperationOutput::ReadContext(ReadContextOutput::DesktopUiInspect(UiInspectOutput {
                snapshot_id: "snapshot".into(),
                adapter: ComputerUseAdapterRef {
                    kind: ComputerUseAdapterKind::MacosAccessibility,
                    version: "test".into(),
                },
                nodes,
                owner_selectable_windows: vec![],
                truncated: false,
            }));
        let raw = serde_json::to_value(&output).unwrap();
        let encoded = serialize(&output).unwrap();
        assert_eq!(deserialize(&encoded).unwrap(), output);
        let compact: Value = serde_json::from_str(&encoded).unwrap();
        let body = &compact["ReadContext"]["DesktopUiInspect"];
        for (i, node) in body["nodes"].as_array().unwrap().iter().enumerate() {
            let mut reference = body["reference_defaults"].as_object().unwrap().clone();
            reference.extend(node["object_ref"].as_object().unwrap().clone());
            if let Some(id) = node.get("element_id") {
                assert!(node["object_ref"].get("token").is_none());
                reference.insert("token".into(), id.clone());
            }
            assert_eq!(
                Value::Object(reference),
                raw["ReadContext"]["DesktopUiInspect"]["nodes"][i]["object_ref"]
            );
        }
        assert_eq!(body["nodes"][99]["enabled"], false);
        assert_eq!(body["nodes"][99]["is_protected"], true);
        assert!(encoded.len() < serde_json::to_string(&output).unwrap().len() * 2 / 3);
    }
}
