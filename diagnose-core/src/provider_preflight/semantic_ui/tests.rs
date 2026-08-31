use super::*;
use crate::device_assistant::*;
use serde_json::json;

fn now() -> u64 {
    1_800_000_000_000
}
fn call(action: UiSemanticAction) -> ToolCall {
    ToolCall { id: "call".into(), name: EXECUTE_CONFIRMED_UI_ACTION_TOOL.into(),
        arguments_json: json!({"target":{"token":"edge-only-reference","snapshot_id":"snapshot",
            "object_kind":"ui_element","expires_at":chrono::DateTime::from_timestamp_millis((now()+60_000) as i64).unwrap().to_rfc3339()},
            "action":action}).to_string() }
}

#[test]
fn both_orchestrators_derive_exact_ui_authority_for_the_same_bounded_actions() {
    let registry = device_assistant_provider_registry();
    for action in [
        UiSemanticAction::Invoke,
        UiSemanticAction::Select,
        UiSemanticAction::Focus,
        UiSemanticAction::Toggle { desired: true },
        UiSemanticAction::SetValue {
            value: "test value".into(),
        },
    ] {
        let call = call(action.clone());
        for surface in [
            ProductSurface::OssPersonalOwner,
            ProductSurface::ManagerPersonalOwner,
        ] {
            let input = UiCallPreflight::build(&registry, surface, &call, now()).unwrap();
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
            assert_eq!(authority.risk_tier, CapabilityRiskTier::R2);
            assert_eq!(
                authority.resource_scope,
                fresh_object_resource_scope(std::slice::from_ref(input.target()))
            );
            assert_eq!(authority.operation_scope, ["use_selected_object"]);
            assert!(authority.export_destinations.is_empty());
            assert_eq!(
                authority.canonical_input_digest_sha256,
                format!(
                    "{:x}",
                    Sha256::digest(input.canonical_input_json().as_bytes())
                )
            );
        }
    }
}

#[test]
fn ui_decoder_rejects_non_ui_targets_extra_authority_and_unbounded_actions() {
    for case in 0..9 {
        let mut call = call(UiSemanticAction::Invoke);
        let mut value: serde_json::Value = serde_json::from_str(&call.arguments_json).unwrap();
        match case {
            0 => value["target"]["object_kind"] = json!("browser_surface"),
            1 => value["target"]["token"] = json!(" "),
            2 => value["target"]["expires_at"] = json!("not-a-date"),
            3 => value["risk_tier"] = json!("r0"),
            4 => value["action"] = json!({"kind":"scroll","params":{"horizontal":0,"vertical":1}}),
            5 => {
                value["action"] =
                    json!({"kind":"set_value","params":{"value":"x".repeat(16*1024+1)}})
            }
            6 => call.name = "browser_open_page".into(),
            7 => value["target"]["snapshot_id"] = json!(""),
            _ => value["scope"] = json!(["arbitrary"]),
        }
        call.arguments_json = value.to_string();
        assert!(ui_action_from_call(&call).is_err(), "case {case}");
    }
}

#[test]
fn original_ui_reference_deadline_and_policy_are_checked_without_renewal() {
    let registry = device_assistant_provider_registry();
    let call = call(UiSemanticAction::Invoke);
    let input = UiCallPreflight::build(
        &registry,
        ProductSurface::ManagerPersonalOwner,
        &call,
        now(),
    )
    .unwrap();
    assert_eq!(input.valid_until_unix_ms(), now() + 60_000);
    assert!(
        UiCallPreflight::build(
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
                    now_unix_ms: clock
                })
                .is_err()
        );
    }
}
