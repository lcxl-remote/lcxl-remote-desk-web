use super::*;
use desk_agent_protocol::browser_control::*;
use serde_json::json;

fn surface() -> ObjectRef {
    ObjectRef {
        token: "opaque-edge-surface".into(),
        snapshot_id: "snapshot".into(),
        object_kind: ObjectKind::BrowserSurface,
        expires_at: "2026-08-30T00:01:00Z".into(),
    }
}

fn now() -> u64 {
    chrono::DateTime::parse_from_rfc3339("2026-08-30T00:00:00Z")
        .unwrap()
        .timestamp_millis() as u64
}

fn subject() -> ProviderCallSubject<'static> {
    ProviderCallSubject {
        actor_id: "owner",
        run_id: "run",
        target_device_id: "device",
        policy_revision: crate::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
        readiness_revision: 7,
        now_unix_ms: now(),
    }
}

fn open() -> ToolCall {
    ToolCall {
        id: "model-call".into(),
        name: "browser_open_page".into(),
        arguments_json: json!({"target": {
            "url": "https://example.com/path?q=hello",
            "origin": {"kind":"https", "host_ascii":"example.com", "port":443}
        }})
        .to_string(),
    }
}

fn page() -> BrowserPageRef {
    BrowserPageRef {
        schema_version: 1,
        adapter: BrowserAdapterRef {
            engine: BrowserEngineKind::ChromeExtension,
            device_id: "device".into(),
            os_session_id: "session".into(),
            browser_major_version: 151,
            browser_version: "151.0".into(),
            adapter_id: "chrome-extension".into(),
            adapter_version: "1".into(),
            profile_incarnation: "profile".into(),
            connection_revision: 7,
        },
        page_id: "page".into(),
        page_incarnation: "incarnation".into(),
        origin: BrowserOrigin {
            kind: BrowserOriginKind::Https,
            host_ascii: "example.com".into(),
            port: 443,
        },
        document_revision: 2,
        url_sha256: "a".repeat(64),
        observed_at_unix_ms: now(),
    }
}

fn element(page: &BrowserPageRef) -> BrowserElementRef {
    BrowserElementRef {
        page_id: page.page_id.clone(),
        page_incarnation: page.page_incarnation.clone(),
        document_revision: page.document_revision,
        element_id: "field".into(),
        role: BrowserElementRole::Textbox,
        accessible_name: "Field".into(),
        value: None,
        element_revision: 1,
    }
}

fn evaluate(call: &ToolCall, surface: &ObjectRef) -> Result<BrowserCallPreflight, AgentError> {
    BrowserCallPreflight::build(
        &crate::device_assistant::device_assistant_provider_registry(),
        ProductSurface::ManagerPersonalOwner,
        call,
        "server-call",
        surface,
        now(),
    )
}

#[test]
fn browser_preflight_derives_identical_authority_for_both_runtimes() {
    let registry = crate::device_assistant::device_assistant_provider_registry();
    let call = open();
    let selected = surface();
    let subject = subject();
    for product in [
        ProductSurface::OssPersonalOwner,
        ProductSurface::ManagerPersonalOwner,
    ] {
        let preflight =
            BrowserCallPreflight::build(&registry, product, &call, "server-call", &selected, now())
                .unwrap();
        let authority = preflight.grant_call(&subject).unwrap();
        assert_eq!(preflight.request().call_id, "server-call");
        assert_eq!(authority.surface, product);
        assert_eq!(authority.risk_tier, CapabilityRiskTier::R2);
        assert_eq!(
            authority.effect,
            desk_agent_protocol::capability_provider::CapabilityEffect::MutateApplication
        );
        assert_eq!(
            authority.resource_scope,
            fresh_object_resource_scope(&[selected.clone()])
        );
        assert_eq!(authority.operation_scope, ["use_selected_object"]);
        assert!(authority.export_destinations.is_empty());
        assert_eq!(
            authority.canonical_input_digest_sha256,
            format!(
                "{:x}",
                Sha256::digest(preflight.canonical_input_json().as_bytes())
            )
        );
        assert!(!authority.resource_scope.join("").contains(&selected.token));
    }
}

#[test]
fn strict_browser_input_rejects_extra_authority_and_bad_urls() {
    for injected in [
        "risk_tier",
        "resource_scope",
        "mutation_class",
        "action",
        "script",
        "export_destinations",
    ] {
        let mut call = open();
        let mut value: serde_json::Value = serde_json::from_str(&call.arguments_json).unwrap();
        value[injected] = json!("forged");
        call.arguments_json = value.to_string();
        assert!(evaluate(&call, &surface()).is_err(), "{injected}");
    }
    for url in [
        "javascript:alert(1)",
        "file:///tmp/private",
        "https://other.example/path",
    ] {
        let mut call = open();
        let mut value: serde_json::Value = serde_json::from_str(&call.arguments_json).unwrap();
        value["target"]["url"] = json!(url);
        call.arguments_json = value.to_string();
        assert!(evaluate(&call, &surface()).is_err(), "{url}");
    }
    let mut unknown = open();
    unknown.name = "execute_script".into();
    assert!(evaluate(&unknown, &surface()).is_err());
}

#[test]
fn browser_preflight_rechecks_expiry_and_does_not_accept_changed_policy() {
    for kind in [ObjectKind::Application, ObjectKind::File] {
        let mut selected = surface();
        selected.object_kind = kind;
        assert!(evaluate(&open(), &selected).is_err());
    }
    let preflight = evaluate(&open(), &surface()).unwrap();
    for which in ["policy", "readiness", "expiry", "subject"] {
        let mut subject = subject();
        match which {
            "policy" => subject.policy_revision = 0,
            "readiness" => subject.readiness_revision = 0,
            "expiry" => subject.now_unix_ms += 60_000,
            "subject" => subject.actor_id = "",
            _ => unreachable!(),
        }
        assert!(preflight.grant_call(&subject).is_err(), "{which}");
    }
    let mut expired = surface();
    expired.expires_at = "2026-08-30T00:00:00Z".into();
    assert!(evaluate(&open(), &expired).is_err());
}

#[test]
fn generic_form_and_activation_are_always_input_fallback() {
    let page = page();
    let element = element(&page);
    for (tool, input) in [
        (
            "browser_fill_form",
            json!({"page":page, "fields":[{"element":element, "value":"bounded"}]}),
        ),
        (
            "browser_activate_element",
            json!({"page":page, "element":element}),
        ),
    ] {
        let call = ToolCall {
            id: "call".into(),
            name: tool.into(),
            arguments_json: input.to_string(),
        };
        let preflight = evaluate(&call, &surface()).unwrap();
        let subject = subject();
        let authority = preflight.grant_call(&subject).unwrap();
        assert_eq!(authority.risk_tier, CapabilityRiskTier::R3);
        assert_eq!(
            authority.effect,
            desk_agent_protocol::capability_provider::CapabilityEffect::InputFallback
        );
        assert!(authority.export_destinations.is_empty());
        assert!(matches!(
            preflight.request().action,
            BrowserAction::FillForm {
                mutation_class: BrowserMutationClass::InputFallback,
                ..
            } | BrowserAction::ActivateElement {
                activation_class: BrowserActivationClass::InputFallback,
                ..
            }
        ));
    }
}

#[test]
fn process_command_lines_raise_the_shared_risk_floor() {
    let registry = crate::device_assistant::device_assistant_provider_registry();
    let descriptor = registry.capability_for_tool("read_process_list").unwrap();
    for (input, expected) in [
        ("{}", CapabilityRiskTier::R0),
        ("{\"include_command_line\":true}", CapabilityRiskTier::R1),
    ] {
        let call = ToolCall {
            id: "call".into(),
            name: "read_process_list".into(),
            arguments_json: input.into(),
        };
        assert_eq!(classify_provider_call(descriptor, &call).unwrap(), expected);
    }
}

#[test]
fn equivalent_json_keeps_the_digest_but_changed_input_or_surface_does_not() {
    let call = open();
    let first = evaluate(&call, &surface()).unwrap();
    let subject = subject();
    let original = first.grant_call(&subject).unwrap();
    let mut formatted = call.clone();
    formatted.arguments_json = serde_json::to_string_pretty(
        &serde_json::from_str::<serde_json::Value>(&call.arguments_json).unwrap(),
    )
    .unwrap();
    let equivalent = evaluate(&formatted, &surface()).unwrap();
    let same = equivalent.grant_call(&subject).unwrap();
    assert_eq!(
        original.canonical_input_digest_sha256,
        same.canonical_input_digest_sha256
    );
    assert_eq!(original.byte_count, same.byte_count);
    formatted.arguments_json = formatted.arguments_json.replace("q=hello", "q=changed");
    let changed = evaluate(&formatted, &surface()).unwrap();
    assert_ne!(
        original.canonical_input_digest_sha256,
        changed
            .grant_call(&subject)
            .unwrap()
            .canonical_input_digest_sha256
    );
    let mut other_surface = surface();
    other_surface.token.push_str("-different");
    let changed = evaluate(&call, &other_surface).unwrap();
    assert_ne!(
        original.resource_scope,
        changed.grant_call(&subject).unwrap().resource_scope
    );
    let mut oversized = call;
    oversized.arguments_json.push_str(&" ".repeat(1024 * 1024));
    assert!(evaluate(&oversized, &surface()).is_err());
}

#[test]
fn communication_handoffs_pin_destinations_and_cannot_become_send_actions() {
    use crate::device_assistant::{
        GMAIL_WEB_CURRENT_PROFILE_ACCOUNT_ID, SLACK_WEB_CURRENT_PROFILE_ACCOUNT_ID,
    };
    use desk_agent_protocol::capability_provider::CapabilityEffect;
    use desk_agent_protocol::communication::COMMUNICATION_SCHEMA_VERSION;

    for gmail in [false, true] {
        let mut page = page();
        page.origin.host_ascii = if gmail {
            "mail.google.com"
        } else {
            "app.slack.com"
        }
        .into();
        let field = |id: &str| {
            let mut field = element(&page);
            field.element_id = id.into();
            field
        };
        let (tool, input, destination) = if gmail {
            let mut to_field = field("to");
            to_field.role = BrowserElementRole::Combobox;
            (
                "prepare_gmail_web_draft_handoff",
                json!({"schema_version":COMMUNICATION_SCHEMA_VERSION,"page":page,"to_field":to_field,
                    "subject_field":field("subject"),"body_field":field("body"),"attachment":null,
                    "draft":{"schema_version":COMMUNICATION_SCHEMA_VERSION,"recipients":[{"role":"to","address":"alice@example.com","display_name":null}],
                        "subject":"Review","body_plain_text":"Draft only","attachment_labels":[]}}),
                DestinationIdentity::EmailAccount {
                    account_id: GMAIL_WEB_CURRENT_PROFILE_ACCOUNT_ID.into(),
                },
            )
        } else {
            (
                "prepare_slack_web_message_handoff",
                json!({"schema_version":COMMUNICATION_SCHEMA_VERSION,"page":page,"composer":field("composer"),"body_plain_text":"Draft only"}),
                DestinationIdentity::ChatAccount {
                    account_id: SLACK_WEB_CURRENT_PROFILE_ACCOUNT_ID.into(),
                },
            )
        };
        let mut call = ToolCall {
            id: "call".into(),
            name: tool.into(),
            arguments_json: input.to_string(),
        };
        for product in [
            ProductSurface::OssPersonalOwner,
            ProductSurface::ManagerPersonalOwner,
        ] {
            let preflight = BrowserCallPreflight::build(
                &crate::device_assistant::device_assistant_provider_registry(),
                product,
                &call,
                "server-call",
                &surface(),
                now(),
            )
            .unwrap();
            let subject = subject();
            let authority = preflight.grant_call(&subject).unwrap();
            assert_eq!(
                authority.export_destinations,
                std::slice::from_ref(&destination)
            );
            assert_eq!(authority.effect, CapabilityEffect::WriteExternalDraft);
            assert_eq!(authority.risk_tier, CapabilityRiskTier::R3);
            assert!(matches!(
                preflight.request().action,
                BrowserAction::FillForm {
                    mutation_class: BrowserMutationClass::WriteExternalDraft,
                    ..
                }
            ));
        }
        let mut injected = input;
        injected["send"] = json!(true);
        call.arguments_json = injected.to_string();
        assert!(evaluate(&call, &surface()).is_err());
    }
}

#[test]
fn outlook_handoff_pins_application_destination_and_manual_compose_request() {
    use desk_agent_protocol::{
        capability_provider::CapabilityEffect,
        communication::{COMMUNICATION_SCHEMA_VERSION, CommunicationSurfaceKind},
        computer_use::ComputerActionKind,
    };

    let application = ObjectRef {
        token: "opaque-outlook-application".into(),
        snapshot_id: "outlook-snapshot".into(),
        object_kind: ObjectKind::Application,
        expires_at: "2026-08-30T00:01:00Z".into(),
    };
    let call = ToolCall {
        id: "call".into(),
        name: "prepare_outlook_new_draft_handoff".into(),
        arguments_json: json!({"draft": {
            "schema_version": COMMUNICATION_SCHEMA_VERSION,
            "recipients": [{"role":"to","address":"alice@example.com","display_name":null}],
            "subject":"Review", "body_plain_text":"Draft only", "attachment_labels":[]
        }})
        .to_string(),
    };
    let registry = crate::device_assistant::device_assistant_provider_registry();
    for product in [
        ProductSurface::OssPersonalOwner,
        ProductSurface::ManagerPersonalOwner,
    ] {
        let preflight = OutlookCallPreflight::build(
            &registry,
            product,
            &call,
            "server-call",
            "run",
            "device",
            "interactive-session",
            7,
            &application,
            now(),
        )
        .unwrap();
        let subject = subject();
        let authority = preflight.grant_call(&subject).unwrap();
        assert_eq!(authority.effect, CapabilityEffect::WriteExternalDraft);
        assert_eq!(authority.risk_tier, CapabilityRiskTier::R3);
        assert_eq!(authority.operation_scope, ["use_selected_object"]);
        assert_eq!(
            authority.export_destinations,
            [DestinationIdentity::EmailAccount {
                account_id: crate::device_assistant::OUTLOOK_NEW_UNVERIFIED_ACCOUNT_ID.into(),
            }]
        );
        assert_eq!(preflight.target(), &application);
        assert_eq!(preflight.request().call_id, "server-call");
        assert_eq!(preflight.request().run_id, "run");
        assert_eq!(
            preflight.request().surface.kind,
            CommunicationSurfaceKind::OutlookNewDesktop
        );
        assert!(matches!(
            ComputerActionKind::Communication(preflight.request().clone()),
            ComputerActionKind::Communication(_)
        ));
    }
    for injected in ["send", "attachments", "export_destinations"] {
        let mut bad = serde_json::from_str::<serde_json::Value>(&call.arguments_json).unwrap();
        bad[injected] = json!(true);
        let mut bad_call = call.clone();
        bad_call.arguments_json = bad.to_string();
        assert!(
            OutlookCallPreflight::build(
                &registry,
                ProductSurface::ManagerPersonalOwner,
                &bad_call,
                "server-call",
                "run",
                "device",
                "interactive-session",
                7,
                &application,
                now(),
            )
            .is_err(),
            "{injected}"
        );
    }
}
