use desk_agent_protocol::{
    agent_event::{AgentEvent, AgentEventKind},
    capability_provider::CapabilityInventorySnapshot,
    device_assistant::{
        DeviceAssistantAsk, DeviceAssistantContextUpdate, DeviceAssistantContextUpdated,
        DeviceAssistantObjectContextUpdate,
    },
};
use serde_json::Value;

fn contract() -> Value {
    serde_json::from_str(include_str!(
        "fixtures/mobile_device_assistant/contract-v1.json"
    ))
    .expect("mobile Device Assistant fixture must be valid JSON")
}

#[test]
fn canonical_mobile_contract_decodes_with_the_shared_protocol() {
    let root = contract();

    let minimal: DeviceAssistantAsk =
        serde_json::from_value(root["asks"]["minimal"].clone()).expect("minimal ask");
    let full: DeviceAssistantAsk =
        serde_json::from_value(root["asks"]["full"].clone()).expect("full ask");
    minimal.validate().expect("minimal ask is valid");
    full.validate().expect("full ask is valid");

    let events: Vec<AgentEvent> =
        serde_json::from_value(root["events"].clone()).expect("agent events");
    assert_eq!(events.len(), 10);
    assert!(
        events
            .iter()
            .any(|event| event.kind == AgentEventKind::Answer)
    );
    assert!(
        events
            .iter()
            .any(|event| event.kind == AgentEventKind::PermissionRequired)
    );

    let inventory: CapabilityInventorySnapshot =
        serde_json::from_value(root["inventory"].clone()).expect("inventory");
    inventory
        .validate()
        .expect("inventory is internally consistent");
    assert!(inventory.entries[0].context_selectable);

    let update: DeviceAssistantContextUpdate =
        serde_json::from_value(root["context_update"].clone()).expect("context update");
    update.validate().expect("context update is valid");
    let ack: DeviceAssistantContextUpdated =
        serde_json::from_value(root["context_updated"].clone()).expect("context ack");
    assert_eq!(ack.client_request_id, update.client_request_id);

    let object_update: DeviceAssistantObjectContextUpdate =
        serde_json::from_value(root["object_context_update"].clone()).expect("object update");
    object_update.validate().expect("object update is valid");
}

#[test]
fn canonical_feature_profile_and_mutations_keep_fail_closed_fields_explicit() {
    let root = contract();
    let profiles = &root["feature_profiles"];
    assert_eq!(profiles["oss"]["device_assistant"]["schema_version"], 1);
    assert_eq!(profiles["oss"]["device_assistant"]["turn_stream"], true);
    assert_eq!(
        profiles["manager_inventory_only"]["device_assistant"]["turn_stream"],
        false
    );
    assert!(profiles["legacy"].get("device_assistant").is_none());

    let mutations = &root["mutations"];
    assert_eq!(mutations["permission_deny"]["items"][0]["decision"], "deny");
    assert!(
        mutations["permission_deny"]["items"][0]
            .get("resourceScope")
            .is_none()
    );
    assert_eq!(
        mutations["background_cancel"]["requestId"],
        "cancel-request-1"
    );
}
