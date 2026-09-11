//! Compact model-only UI receipts. Native and controller wire references stay intact.

use desk_agent_protocol::{OperationOutput, ReadContextOutput};
use serde_json::{Value, json};

pub fn serialize(output: &OperationOutput) -> Result<String, serde_json::Error> {
    let OperationOutput::ReadContext(ReadContextOutput::DesktopUiInspect(ui)) = output else {
        return serde_json::to_string(output);
    };
    let mut value = serde_json::to_value(output)?;
    let body = &mut value["ReadContext"]["DesktopUiInspect"];
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

/// Expand only the model representation for existing typed validators.
pub fn deserialize(text: &str) -> Result<OperationOutput, serde_json::Error> {
    let mut value: Value = serde_json::from_str(text)?;
    if let Some(body) = value
        .pointer_mut("/ReadContext/DesktopUiInspect")
        .and_then(Value::as_object_mut)
    {
        body.remove("truncation_hint");
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
    serde_json::from_value(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::computer_use::*;

    #[test]
    fn compact_receipt_preserves_exact_references_and_non_default_states() {
        let nodes = (0..100)
            .map(|i| UiNodeProjection {
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
