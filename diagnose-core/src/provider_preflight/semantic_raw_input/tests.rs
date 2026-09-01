use super::*;
use crate::device_assistant::*;
use desk_agent_protocol::computer_use::{
    RawInputKey, RawInputMouseButton, RawInputScreenContext, RawInputStep,
};
use serde_json::json;

fn now() -> u64 {
    1_800_000_000_000
}

fn action() -> RawInputAction {
    RawInputAction {
        screen: RawInputScreenContext {
            display: r"\\.\DISPLAY1".into(),
            width: 1920,
            height: 1080,
            dpi_x: 96,
            dpi_y: 96,
        },
        step: RawInputStep::Click {
            x: 100,
            y: 200,
            button: RawInputMouseButton::Primary,
        },
    }
}

fn call(action: RawInputAction) -> ToolCall {
    ToolCall {
        id: "call".into(),
        name: EXECUTE_CONFIRMED_RAW_INPUT_TOOL.into(),
        arguments_json: json!({
            "target": {
                "token": "edge-only-application",
                "snapshot_id": "snapshot",
                "object_kind": "application",
                "expires_at": chrono::DateTime::from_timestamp_millis((now() + 60_000) as i64)
                    .unwrap()
                    .to_rfc3339()
            },
            "action": action
        })
        .to_string(),
    }
}

#[test]
fn both_orchestrators_derive_one_exact_r3_raw_input_authority() {
    let registry = device_assistant_provider_registry();
    for action in [
        action(),
        RawInputAction {
            step: RawInputStep::KeyPress {
                key: RawInputKey::Enter,
            },
            ..action()
        },
        RawInputAction {
            step: RawInputStep::TypeText {
                text: "bounded text".into(),
            },
            ..action()
        },
        RawInputAction {
            step: RawInputStep::Scroll {
                horizontal: 0,
                vertical: 120,
            },
            ..action()
        },
    ] {
        let call = call(action.clone());
        for surface in [
            ProductSurface::OssPersonalOwner,
            ProductSurface::ManagerPersonalOwner,
        ] {
            let input = RawInputCallPreflight::build(&registry, surface, &call, now()).unwrap();
            assert_eq!(input.action(), &action);
            let subject = ProviderCallSubject {
                actor_id: "owner",
                run_id: "run",
                target_device_id: "device",
                policy_revision: 1,
                readiness_revision: 7,
                now_unix_ms: now(),
            };
            let authority = input.grant_call(&subject).unwrap();
            assert_eq!(authority.surface, surface);
            assert_eq!(authority.risk_tier, CapabilityRiskTier::R3);
            assert_eq!(
                authority.resource_scope,
                fresh_object_resource_scope(std::slice::from_ref(input.target()))
            );
            assert_eq!(authority.operation_scope, ["use_selected_object"]);
            assert_eq!(authority.item_count, 1);
            assert!(authority.export_destinations.is_empty());
        }
    }
}

#[test]
fn raw_input_decoder_rejects_changed_authority_and_unbounded_inputs() {
    for case in 0..9 {
        let mut call = call(action());
        let mut value: serde_json::Value = serde_json::from_str(&call.arguments_json).unwrap();
        match case {
            0 => value["target"]["object_kind"] = json!("ui_element"),
            1 => value["target"]["token"] = json!(" "),
            2 => value["target"]["snapshot_id"] = json!(""),
            3 => value["target"]["expires_at"] = json!("not-a-date"),
            4 => value["action"]["screen"]["width"] = json!(0),
            5 => value["action"]["step"]["params"]["x"] = json!(1920),
            6 => {
                value["action"]["step"] =
                    json!({"kind":"type_text","params":{"text":"x".repeat(4097)}})
            }
            7 => value["risk_tier"] = json!("r0"),
            _ => call.name = EXECUTE_CONFIRMED_UI_ACTION_TOOL.into(),
        }
        call.arguments_json = value.to_string();
        assert!(raw_input_from_call(&call).is_err(), "case {case}");
    }
}

#[test]
fn raw_input_reference_deadline_and_subject_facts_are_not_renewed() {
    let registry = device_assistant_provider_registry();
    let call = call(action());
    let input = RawInputCallPreflight::build(
        &registry,
        ProductSurface::ManagerPersonalOwner,
        &call,
        now(),
    )
    .unwrap();
    assert_eq!(input.valid_until_unix_ms(), now() + 60_000);
    assert!(
        RawInputCallPreflight::build(
            &registry,
            ProductSurface::ManagerPersonalOwner,
            &call,
            now() + 60_000
        )
        .is_err()
    );
    for (policy, revision, clock) in [
        (0, 7, now()),
        (1, 0, now()),
        (1, 7, now() + 60_000),
        (1, 7, 0),
    ] {
        assert!(
            input
                .grant_call(&ProviderCallSubject {
                    actor_id: "owner",
                    run_id: "run",
                    target_device_id: "device",
                    policy_revision: policy,
                    readiness_revision: revision,
                    now_unix_ms: clock,
                })
                .is_err()
        );
    }
}
