use super::*;

/// Fresh audit event id.
pub(super) fn new_audit_event_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Baseline session-establishment / control-plane frames that every session —
/// even a fully capped grant / support session — may use. Deliberately minimal:
/// session establishment (`RequestRemote` / `Offer` / `Answer` / `Canid`), the
/// control plane (`RequireControl` / `CloseControl`), teardown
/// (`ConnectionRemoved`), `Heartbeat`, and the manager's `SupportCodeIssued`
/// host-inbound notification (display + arm TTL; triggers no privileged action).
pub(super) fn is_baseline_signaling_type(t: SignalingType) -> bool {
    matches!(
        t,
        SignalingType::RequestRemote
            | SignalingType::Offer
            | SignalingType::Answer
            | SignalingType::Canid
            | SignalingType::RequireControl
            | SignalingType::CloseControl
            | SignalingType::ConnectionRemoved
            | SignalingType::Heartbeat
            | SignalingType::SupportCodeIssued
    )
}

/// Connection-scoped capability frames whose enforcement is a per-dimension
/// `access_ceiling` gate: the terminal I/O family, the file-browse family, and
/// private-screen enable. These are only ever legitimate **after** the connection's
/// admission has been recorded (owner → `OwnerFull`, redeemed grant → `Capped`) and
/// its worker-side ceiling provisioned. An un-admitted connection sending one is
/// anomalous: no ceiling has reached the worker yet, so the worker-side `meet` gate
/// would fall back to the host global (fail-open). door1 therefore denies these for
/// an un-admitted connection.
///
/// `StartTerminal` is deliberately **excluded**: like `RequestRemote`, it is the
/// admission-*establishing* frame for the terminal WS (a distinct connection that
/// never does a `RequestRemote`). Its own source-gate (`gate_start_terminal_frame`)
/// requires and validates a capability stamp on the trusted-central link, and
/// `handle_start_terminal_inbound` records the admission + ceiling from that stamp —
/// so it must be allowed to reach the handler on an un-admitted connection, exactly
/// as `RequestRemote` is. The remaining terminal I/O frames stay gated here and pass
/// once `StartTerminal` has established the admission.
pub(super) fn is_connection_scoped_capability_frame(t: SignalingType) -> bool {
    use SignalingType::*;
    matches!(
        t,
        SendDataToTerminal
            | ResizeTerminal
            | CloseTerminal
            | ListTerminal
            | ManagerFileList
            | ManagerFileDelete
            | EnablePrivateScreen
    )
}

/// The first fail-closed door for a capability-capped session (a redeemed grant
/// or a legacy support session, both carrying an `access_ceiling`). Permits the
/// baseline frames unconditionally, plus the connection-scoped capability
/// families whose ceiling dimension is not an explicit `Some(false)` — so the
/// frame can reach its worker-side `meet(ceiling, global)` gate. Everything else
/// is denied: owner-plane frames (`Manager*` settings / system-info, display,
/// AI / exec / remote-tool) have **no** worker-side meet gate, so door1 is their
/// only enforcement point against a capped session; and any unknown / future
/// signaling type falls through the `_ => false` arm (deliberate fail-closed —
/// this is not the `handle_message` exhaustiveness rule).
pub(super) fn capped_session_permits(t: SignalingType, ceiling: &SecuritySettings) -> bool {
    use SignalingType::*;
    if is_baseline_signaling_type(t) {
        return true;
    }
    match t {
        // Terminal family — the whole terminal UI including enumeration.
        StartTerminal | SendDataToTerminal | ResizeTerminal | CloseTerminal | ListTerminal => {
            ceiling.allow_terminal != Some(false)
        }
        ManagerFileList => ceiling.allow_file_browse != Some(false),
        ManagerFileDelete => {
            ceiling.allow_file_browse != Some(false) && ceiling.allow_file_delete != Some(false)
        }
        EnablePrivateScreen => ceiling.allow_private_screen != Some(false),
        _ => false,
    }
}

/// Classification of a `from_connection_id` for the door1 gate, derived from its
/// **admission record** (kept for the whole signaling connection, independent of
/// the PC lifecycle) rather than the PC's live state — so a capped connection
/// that dropped its PC via `CloseControl` is still classified as capped.
pub(super) enum ConnectionGate {
    /// Admitted as a full owner session — no capability ceiling.
    KnownOwnerFull,
    /// Admitted as a redeemed-grant / legacy-support session, capped by a ceiling.
    KnownCapped(SecuritySettings),
    /// A server-internal frame has no originating WebSocket connection id.
    /// Its producing service already performed the applicable authorization.
    /// File-manager operations are never server-internal because their REST
    /// entry points were removed; they require an admitted controller connection.
    /// Client frames always receive a server-stamped id, so a client cannot
    /// manufacture this classification.
    ServerInternal,
    /// A WS connection carrying a real stamped id but no admission record: it
    /// never did an authorized `RequestRemote` on this instance (a management-only
    /// connection, or a session before its `RequestRemote`), or the id is spoofed.
    /// door1 is fail-closed for connection-scoped capability frames here (see
    /// [`door1_permits`]) — a capped session that has not yet been admitted must
    /// not slip a capability frame through the pre-admission window where the
    /// worker has no ceiling and would fall back to the host global.
    UnadmittedConnection,
}

/// Classify a `from_connection_id` for the door1 gate from the registry's
/// admission map. The server stamps `from_connection_id` authoritatively
/// (`ConnectionState::send_to_peer`), so this cannot be spoofed by the client.
pub(super) async fn classify_connection(
    registry: &PcRegistry,
    connection_id: Option<&str>,
) -> ConnectionGate {
    let Some(cid) = connection_id else {
        return ConnectionGate::ServerInternal;
    };
    match registry.admission(cid).await {
        Some(pc_manager::Admission::OwnerFull) => ConnectionGate::KnownOwnerFull,
        Some(pc_manager::Admission::Capped(c)) => ConnectionGate::KnownCapped(c),
        None => ConnectionGate::UnadmittedConnection,
    }
}

/// The door1 decision for an inbound frame. A session admitted as owner passes
/// everything (route() drops non-inbound types anyway); a capped session (a
/// redeemed grant carrying an `access_ceiling`, including a temporary-support
/// session) runs the capability matrix (still capped after a `CloseControl` PC
/// teardown, since the admission outlives the PC).
///
/// A server-internal frame passes because its producing service already ran
/// the applicable authorization checks, except file-manager frames, which must
/// carry an admitted controller connection. An un-admitted WebSocket connection
/// is fail-closed for connection-scoped capability frames: those frames are
/// only legitimate after admission provisions the worker ceiling. Otherwise
/// the worker would evaluate the request against the host global setting in
/// the pre-admission window. Owner-plane management frames are authorized by
/// the central service, while AI and exec frames keep their dedicated
/// authorization gates.
pub(super) fn door1_permits(gate: &ConnectionGate, t: SignalingType) -> bool {
    match gate {
        ConnectionGate::KnownOwnerFull => true,
        ConnectionGate::KnownCapped(ceiling) => capped_session_permits(t, ceiling),
        ConnectionGate::ServerInternal => !matches!(
            t,
            SignalingType::ManagerFileList | SignalingType::ManagerFileDelete
        ),
        ConnectionGate::UnadmittedConnection => !is_connection_scoped_capability_frame(t),
    }
}

/// RFC3339 timestamp for an audit event.
pub(super) fn audit_now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Revoke every session-scoped exec approval held by the connection that sent
/// `model`. Called when the connection releases control (`CloseControl`) or ends
/// (`ConnectionRemoved`); a no-op when the connection had no grants.
pub(super) fn revoke_session_approvals(ctx: &RouterContext, model: &SignalingModel) {
    if let Some(conn) = model.from_connection_id.as_deref() {
        let revoked = ctx.session_approvals.revoke_connection(conn);
        if revoked > 0 {
            log::debug!("[router] revoked {revoked} session exec approval(s) for {conn}");
        }
    }
}

/// snake_case risk label for the audit `risk` column.
pub(super) fn risk_str(risk: desk_agent_protocol::RiskLevel) -> &'static str {
    use desk_agent_protocol::RiskLevel::*;
    match risk {
        Low => "low",
        Medium => "medium",
        High => "high",
        Critical => "critical",
        Blocked => "blocked",
    }
}

/// Route a signaling message.
///
/// Each `SignalingType` is exhaustively dispatched: PC / SDP / ICE /
/// SignalingState types run inline against `ctx.pc_registry`;
/// worker-bound types (terminal, manager queries, EnablePrivateScreen,
/// UpdateDeskSettings) are shipped to the worker via dedicated
/// `ServiceToWorker::*` typed IPC variants; daemon-emitted notifications
/// and dead-enum variants (`Answer`, `Init`, `Heartbeat`, `Error`,
/// `Unknown`, ...) are trace-logged + dropped. There is no fallback
/// path because the typed IPC path has no `SignalingMessage` bridge.
pub(super) async fn promote_desktop_resources(
    model: &SignalingModel,
    ctx: &RouterContext,
    reason: &str,
) -> Result<(), RouterError> {
    let connection_id = model
        .check_and_get_from_connection_id()
        .map_err(DeskError::from)?;
    if let Some(pc) = ctx.pc_registry.get(connection_id).await {
        let pc = pc.read().await;
        let mut state = pc.signaling_state.write().await;
        if state.purpose == RemoteSessionPurpose::FileManager {
            state.purpose = RemoteSessionPurpose::RemoteDesktop;
            log::info!("[router] promoted {connection_id} to remote_desktop for {reason}");
        }
    }

    let virtual_display_enabled = ctx.settings.read().await.virtual_display.enabled;
    if virtual_display_enabled && let Some(supervisor) = ctx.virtual_display.as_ref() {
        match supervisor
            .ensure_attached(VIRTUAL_DISPLAY_ATTACH_TIMEOUT)
            .await
        {
            EnsureAttachedOutcome::Attached => {}
            EnsureAttachedOutcome::TimedOut => {
                log::warn!("[router] virtual display attach timed out during {reason}");
            }
            EnsureAttachedOutcome::Unavailable(e) => {
                log::warn!("[router] virtual display unavailable during {reason}: {e}");
            }
        }
    }
    Ok(())
}
