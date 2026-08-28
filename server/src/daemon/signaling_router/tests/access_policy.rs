use super::*;

/// Daemon-owned: WebRTC SDP/ICE/PC lifecycle + daemon-emitted
/// notifications + connection bookkeeping + WS heartbeat.
/// Pinning these prevents accidental classification flips: the
/// only way to move a daemon type back to the worker should be a
/// deliberate code review.
#[test]
pub(super) fn classify_daemon_owned_types() {
    for t in [
        SignalingType::RequestRemoteAccess,
        SignalingType::RemoteAccessInitialized,
        SignalingType::Offer,
        SignalingType::Answer,
        SignalingType::IceCandidate,
        SignalingType::ReleaseControl,
        SignalingType::ControlReleased,
        SignalingType::CloseRemoteSession,
        SignalingType::RequireControl,
        SignalingType::ControlAccepted,
        SignalingType::ControlDenied,
        SignalingType::PrivateScreenStateChanged,
        SignalingType::AudioPlaybackFailed,
        SignalingType::MediaPipelineStateChanged,
        SignalingType::RetryMediaPipeline,
        SignalingType::ApplyRemoteSessionSettings,
        SignalingType::RemoteSessionSettingsApplied,
        SignalingType::UpdateAdaptiveVideoQuality,
        SignalingType::SystemAudioCaptureStateChanged,
        SignalingType::SystemInfoRetrieved,
        SignalingType::TerminalOutputProduced,
        SignalingType::TerminalStarted,
        SignalingType::TerminalClosed,
        SignalingType::DesktopSwitching,
        SignalingType::DesktopReady,
        SignalingType::FetchConnections,
        SignalingType::ConnectionsFetched,
        SignalingType::ConnectionRemoved,
        SignalingType::SendHeartbeat,
        SignalingType::HeartbeatAcknowledged,
        // Error / Unknown are daemon-owned.
        SignalingType::Error,
        SignalingType::Unknown,
        // AgentCapabilityCompleted only flows worker → control end.
        SignalingType::AgentCapabilityCompleted,
        // Fleet exec: request handled inline (PEP + dispatch); result is
        // daemon-emitted toward the manager.
        SignalingType::ExecuteEdgePlan,
        SignalingType::EdgeExecutionCompleted,
        // Temporary-support code: manager → daemon, consumed locally.
        SignalingType::SupportCodeIssued,
    ] {
        assert_eq!(
            classify(t),
            RouteOwnership::Daemon,
            "{t:?} should be daemon-owned",
        );
    }
}

/// Worker-bound: user-session resources (files, terminal request
/// types, overlays, approval, manager queries). The 3
/// terminal *reverse* notification types (`TerminalOutputProduced`,
/// `TerminalStarted`, `TerminalClosed`) are classified as
/// daemon-owned because they only flow worker → browser; an
/// inbound copy is a protocol error to swallow.
#[test]
pub(super) fn classify_worker_owned_types() {
    for t in [
        SignalingType::SetPrivateScreenVisibility,
        SignalingType::GetSystemInfo,
        SignalingType::ListFiles,
        SignalingType::StartTerminal,
        SignalingType::SendTerminalInput,
        SignalingType::ResizeTerminal,
        SignalingType::CloseTerminal,
        SignalingType::ListTerminalCommands,
        SignalingType::ChangeDisplaySettings,
        SignalingType::InvokeAgentCapability,
    ] {
        assert_eq!(
            classify(t),
            RouteOwnership::Worker,
            "{t:?} should be worker-owned",
        );
    }
}

/// Exhaustive door1 capability matrix over **every** `SignalingType`
/// (enumerated via `EnumIter`, so a newly-added variant is automatically
/// checked). A capped session may use only the baseline frames plus the three
/// connection-scoped capability families whose ceiling dimension is not an
/// explicit `Some(false)`. Everything else — owner-plane `Manager*` /
/// display / AI-exec, plus any unknown / future type — is fail-closed denied.
#[test]
pub(super) fn capped_session_permits_matrix_over_all_signaling_types() {
    use SignalingType::*;
    use strum::IntoEnumIterator;

    // Support default: every capability hard-denied → only baseline passes.
    let deny_all = SecuritySettings {
        allow_remote_control: Some(false),
        allow_clipboard_sync: Some(false),
        allow_private_screen: Some(false),
        allow_whiteboard: Some(false),
        allow_terminal: Some(false),
        allow_file_browse: Some(false),
        allow_file_transfer: Some(false),
        ..Default::default()
    };
    // Permissive: the three door1 families reach their service-layer gate.
    let allow_families = SecuritySettings {
        allow_terminal: Some(true),
        allow_file_browse: Some(true),
        allow_private_screen: Some(true),
        ..Default::default()
    };

    let terminal_family = [
        StartTerminal,
        SendTerminalInput,
        ResizeTerminal,
        CloseTerminal,
        ListTerminalCommands,
    ];
    let file_family = [ListFiles, DeleteFile];

    for t in SignalingType::iter() {
        let baseline = is_baseline_signaling_type(t);
        let is_connection_settings =
            matches!(t, ApplyRemoteSessionSettings | UpdateAdaptiveVideoQuality);
        let is_family = terminal_family.contains(&t)
            || file_family.contains(&t)
            || t == SetPrivateScreenVisibility;

        // A baseline type must never also be a capability family (no overlap).
        assert!(
            !(baseline && is_family),
            "{t:?} is both baseline and a family"
        );

        // Deny-all ceiling: only baseline passes.
        assert_eq!(
            capped_session_permits(t, &deny_all),
            baseline || is_connection_settings,
            "deny-all ceiling: {t:?}"
        );
        // Permissive ceiling: baseline + the three families pass; owner-plane /
        // unknown stays denied (the `_ => false` fail-closed arm).
        assert_eq!(
            capped_session_permits(t, &allow_families),
            baseline || is_family || is_connection_settings,
            "permissive ceiling: {t:?}"
        );
    }

    // Spot-check owner-plane frames: no worker-side meet gate
    // protects them, so door1 must deny them for a capped session even under a
    // permissive ceiling.
    for t in [
        GetSystemInfo,
        ChangeDisplaySettings,
        InvokeAgentCapability,
        PreviewExecution,
        ResolveExecution,
        AskTerminalCopilot,
        CollectEvidence,
        ExecuteEdgePlan,
        InvokeRemoteTool,
    ] {
        assert!(
            !capped_session_permits(t, &allow_families),
            "owner-plane {t:?} must be denied for a capped session"
        );
    }
}

/// door1's per-family `Some(false)` early-reject vs. `None` pass-through: an
/// explicit deny short-circuits at the router, while an unset dimension passes
/// to the service-layer `meet` gate (which handles the prompt/deny).
#[test]
pub(super) fn capped_session_permits_early_rejects_only_explicit_deny() {
    use SignalingType::*;
    let ceiling = SecuritySettings {
        allow_terminal: Some(true),
        allow_file_browse: Some(false), // explicit deny → early reject
        // allow_private_screen left None → passes to the service meet gate
        ..Default::default()
    };
    assert!(capped_session_permits(StartTerminal, &ceiling));
    assert!(!capped_session_permits(ListFiles, &ceiling));
    assert!(capped_session_permits(SetPrivateScreenVisibility, &ceiling));
}

#[test]
pub(super) fn capped_session_requires_browse_and_delete_for_file_delete() {
    use SignalingType::*;

    let browse_only = SecuritySettings {
        allow_file_browse: Some(true),
        allow_file_delete: Some(false),
        ..Default::default()
    };
    assert!(capped_session_permits(ListFiles, &browse_only));
    assert!(!capped_session_permits(DeleteFile, &browse_only));

    let delete_only = SecuritySettings {
        allow_file_browse: Some(false),
        allow_file_delete: Some(true),
        ..Default::default()
    };
    assert!(!capped_session_permits(ListFiles, &delete_only));
    assert!(!capped_session_permits(DeleteFile, &delete_only));

    let browse_and_delete = SecuritySettings {
        allow_file_browse: Some(true),
        allow_file_delete: Some(true),
        ..Default::default()
    };
    assert!(capped_session_permits(DeleteFile, &browse_and_delete));
}

/// The admission-based door1 gate: a session admitted as owner passes
/// everything; a capped session (a redeemed grant, including a temporary-support
/// session) runs the capability matrix; an un-admitted connection is fail-closed
/// for connection-scoped capability frames (the pre-`RequestRemoteAccess` window
/// where the worker has no ceiling — pre-admission), while owner-plane frames pass here
/// and are authorized at the central.
#[test]
pub(super) fn door1_permits_gates_capped_sessions_and_fails_closed_unadmitted_capability() {
    use SignalingType::*;
    let capped = SecuritySettings {
        allow_terminal: Some(true),
        ..Default::default()
    };

    // Admitted owner: everything passes.
    assert!(door1_permits(
        &ConnectionGate::KnownOwnerFull,
        GetSystemInfo
    ));
    // Admitted capped: owner-plane denied, permitted family allowed.
    assert!(!door1_permits(
        &ConnectionGate::KnownCapped(capped.clone()),
        GetSystemInfo
    ));
    assert!(door1_permits(
        &ConnectionGate::KnownCapped(capped),
        StartTerminal
    ));
    // Un-admitted WS connection: a connection-scoped capability frame is
    // denied — it would otherwise reach the worker before any ceiling was
    // provisioned and be evaluated against the host global
    // pre-RequestRemoteAccess window). `StartTerminal` is deliberately NOT in this
    // list: like `RequestRemoteAccess` it is the admission-establishing frame for the
    // terminal WS, gated by its own source-gate + handler, so it must reach the
    // handler on an un-admitted connection (asserted permitted below).
    for t in [
        SendTerminalInput,
        ResizeTerminal,
        CloseTerminal,
        ListTerminalCommands,
        ListFiles,
        DeleteFile,
        SetPrivateScreenVisibility,
    ] {
        assert!(
            !door1_permits(&ConnectionGate::UnadmittedConnection, t),
            "un-admitted capability frame {t:?} must be denied at door1"
        );
    }
    // Un-admitted owner-plane / baseline / admission-establishing frames still
    // pass here (owner-plane is authorized at the central; a code-session cannot
    // originate them; `RequestRemoteAccess` / `StartTerminal` are gated by their own
    // source-gate + handler).
    assert!(door1_permits(
        &ConnectionGate::UnadmittedConnection,
        GetSystemInfo
    ));
    assert!(door1_permits(
        &ConnectionGate::UnadmittedConnection,
        RequestRemoteAccess
    ));
    assert!(
        door1_permits(&ConnectionGate::UnadmittedConnection, StartTerminal),
        "StartTerminal is admission-establishing and must pass door1 un-admitted"
    );
    // Server-internal frames may still serve explicitly authorized internal
    // terminal operations, but file-manager frames require a controller
    // connection now that their REST entry points no longer exist.
    assert!(!door1_permits(&ConnectionGate::ServerInternal, ListFiles));
    assert!(!door1_permits(&ConnectionGate::ServerInternal, DeleteFile));
    assert!(!door1_permits(
        &ConnectionGate::ServerInternal,
        ApplyRemoteSessionSettings
    ));
    assert!(!door1_permits(
        &ConnectionGate::ServerInternal,
        UpdateAdaptiveVideoQuality
    ));
    assert!(door1_permits(
        &ConnectionGate::ServerInternal,
        ListTerminalCommands
    ));
}

/// `classify_connection` reads the registry admission map — an id with no
/// admission record is `UnknownConnection`, never silently owner.
#[tokio::test]
pub(super) async fn classify_connection_reads_admission_map() {
    let registry = PcRegistry::new();

    // A missing connection id is a server-internal (service-generated) frame.
    assert!(matches!(
        classify_connection(&registry, None).await,
        ConnectionGate::ServerInternal
    ));
    // A real stamped id with no admission is an un-admitted WS connection,
    // never silently owner.
    assert!(matches!(
        classify_connection(&registry, Some("ghost")).await,
        ConnectionGate::UnadmittedConnection
    ));

    registry
        .record_admission("conn-owner", pc_manager::Admission::OwnerFull)
        .await;
    assert!(matches!(
        classify_connection(&registry, Some("conn-owner")).await,
        ConnectionGate::KnownOwnerFull
    ));

    registry
        .record_admission(
            "conn-cap",
            pc_manager::Admission::Capped(SecuritySettings::default()),
        )
        .await;
    assert!(matches!(
        classify_connection(&registry, Some("conn-cap")).await,
        ConnectionGate::KnownCapped(_)
    ));
}
