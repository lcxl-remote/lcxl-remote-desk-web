use super::*;
use desk_agent_protocol::computer_use::{
    COMPUTER_USE_SCHEMA_VERSION, ComputerUseAdapterKind, ComputerUseAdapterRef,
    ComputerUseCapabilityReadiness, ComputerUseContextReference,
};

const TOOLS: [&str; 4] = [
    "inspect_live_document",
    "inspect_live_presentation",
    "inspect_live_spreadsheet",
    "inspect_office_selection",
];

fn readiness() -> ComputerUseReadiness {
    ComputerUseReadiness {
        schema_version: COMPUTER_USE_SCHEMA_VERSION,
        revision: 1,
        observed_at: "2026-08-31T00:00:00Z".into(),
        expires_at: "2026-08-31T00:01:00Z".into(),
        server_api_version: 1,
        os: "macos".into(),
        interactive_session_incarnation: "worker-1".into(),
        local_ceiling_revision: 1,
        capabilities: TOOLS
            .iter()
            .map(|name| ComputerUseCapabilityReadiness {
                capability: target_kind(name).unwrap().0,
                adapter: ComputerUseAdapterRef {
                    kind: ComputerUseAdapterKind::OfficeExcel,
                    version: "test".into(),
                },
                supported: true,
                ready: true,
                reason: None,
            })
            .collect(),
        context_references: TOOLS
            .iter()
            .map(|name| ComputerUseContextReference {
                capability: target_kind(name).unwrap().0,
                object_ref: ObjectRef {
                    token: format!("token-{name}"),
                    snapshot_id: "snapshot-1".into(),
                    object_kind: target_kind(name).unwrap().1,
                    expires_at: "2026-08-31T00:02:00Z".into(),
                },
            })
            .collect(),
    }
}

fn now() -> u64 {
    millis("2026-08-31T00:00:01Z").unwrap()
}

fn selection(names: &[&str]) -> ReadContextSelection {
    ReadContextSelection {
        tool_names: names.iter().map(|s| (*s).into()).collect(),
        expires_at: None,
        object_attachments: vec![],
        live_targets: vec![],
    }
}

fn captured() -> ReadContextSelection {
    let mut selection = selection(&TOOLS);
    selection.live_targets = capture(&selection, Some(&readiness()), now()).unwrap();
    selection
}

#[test]
fn captures_only_exposed_live_reads_and_never_batch_file_targets() {
    let mut input = selection(&["inspect_live_document", "inspect_selected_pages_with_iwork"]);
    input.live_targets = capture(&input, Some(&readiness()), now()).unwrap();
    assert_eq!(input.live_targets.len(), 1);
    assert_eq!(input.live_targets[0].tool_name, "inspect_live_document");
    assert_eq!(
        input.live_targets[0].object_ref,
        readiness().context_references[0].object_ref
    );
    assert!(
        capture(&selection(&["read_system_info"]), None, now())
            .unwrap()
            .is_empty()
    );
    assert!(capture(&selection(&["inspect_live_document"]), None, now()).is_err());
    assert_eq!(captured().live_targets.len(), 4);
}

#[test]
fn fresh_readiness_does_not_extend_the_original_deadline() {
    let selection = captured();
    let before = selection.clone();
    let mut current = readiness();
    current.revision += 1;
    current.expires_at = "2026-08-31T00:03:00Z".into();
    validate_current(&selection, Some(&current), now()).unwrap();
    let deadline = millis("2026-08-31T00:01:00Z").unwrap();
    assert_eq!(
        expiry(&selection, &selection.live_targets[0]).unwrap(),
        deadline
    );
    assert!(validate_current(&selection, Some(&current), deadline).is_err());
    let mut shorter = selection.clone();
    shorter.expires_at = Some("2026-08-31T00:00:30Z".into());
    assert!(target(&shorter, TOOLS[0], millis("2026-08-31T00:00:30Z").unwrap()).is_err());
    assert_eq!(selection, before);
}

#[test]
fn changed_worker_object_snapshot_expiry_and_unready_reports_are_rejected() {
    let selection = captured();
    for variant in 0..9 {
        let mut current = readiness();
        match variant {
            0 => current.interactive_session_incarnation = "worker-2".into(),
            1 => current.context_references[0].object_ref.token = "other-document".into(),
            2 => current.context_references[0].object_ref.snapshot_id = "other-snapshot".into(),
            3 => {
                current.context_references[0].object_ref.expires_at = "2026-08-31T00:03:00Z".into()
            }
            4 => current.capabilities[0].ready = false,
            5 => current.capabilities[0].supported = false,
            6 => current.context_references.clear(),
            7 => current.expires_at = "2026-08-31T00:00:01Z".into(),
            8 => current.observed_at = "2026-08-31T00:00:32Z".into(),
            _ => unreachable!(),
        }
        assert!(
            validate_current(&selection, Some(&current), now()).is_err(),
            "variant {variant}"
        );
    }
    assert!(validate_current(&selection, None, now()).is_err());
}

#[test]
fn invalid_duplicate_unknown_or_unselected_targets_never_validate() {
    for variant in 0..8 {
        let mut selection = captured();
        match variant {
            0 => selection.live_targets.swap(0, 1),
            1 => selection
                .live_targets
                .push(selection.live_targets[0].clone()),
            2 => selection.live_targets[0].tool_name = "read_system_info".into(),
            3 => {
                selection.tool_names.remove(0);
            }
            4 => selection.live_targets[0].object_ref.object_kind = ObjectKind::File,
            5 => selection.live_targets[0].object_ref.token = " ".into(),
            6 => selection.live_targets[0].interactive_session_incarnation = "x".repeat(4097),
            7 => selection.live_targets[0].object_ref.expires_at = "invalid".into(),
            _ => unreachable!(),
        }
        assert!(selection.validate().is_err(), "variant {variant}");
    }
}

#[test]
fn legacy_json_round_trips_but_cannot_supply_a_missing_original_target() {
    let old = serde_json::json!({"tool_names":["inspect_live_document"], "expires_at":null});
    let selection: ReadContextSelection = serde_json::from_value(old.clone()).unwrap();
    selection.validate().unwrap();
    assert_eq!(serde_json::to_value(&selection).unwrap(), old);
    assert!(target(&selection, "inspect_live_document", now()).is_err());
    let current = captured();
    assert_eq!(
        serde_json::from_str::<ReadContextSelection>(&serde_json::to_string(&current).unwrap())
            .unwrap(),
        current
    );
    let mut corrupt = serde_json::to_value(current).unwrap();
    corrupt["live_targets"][0]["unexpected"] = true.into();
    assert!(serde_json::from_value::<ReadContextSelection>(corrupt).is_err());
}

#[test]
fn object_deadline_is_independent_of_readiness_deadline() {
    let mut current = readiness();
    current.context_references[0].object_ref.expires_at = "2026-08-31T00:00:20Z".into();
    let mut selection = selection(&[TOOLS[0]]);
    selection.live_targets = capture(&selection, Some(&current), now()).unwrap();
    let deadline = millis("2026-08-31T00:00:20Z").unwrap();
    validate_current(&selection, Some(&current), deadline - 1).unwrap();
    assert!(validate_current(&selection, Some(&current), deadline).is_err());
}

#[test]
fn accepted_clock_skew_is_preserved_but_original_targets_cannot_be_recaptured() {
    let mut current = readiness();
    current.observed_at = "2026-08-31T00:00:31Z".into();
    let mut selection = selection(&[TOOLS[0]]);
    selection.live_targets = capture(&selection, Some(&current), now()).unwrap();
    validate_current(&selection, Some(&current), now()).unwrap();
    assert!(capture(&selection, Some(&current), now()).is_err());
}
