use super::*;

const TEST_CONNECTION_EPOCH: &str = "test-connection-epoch";

/// Keep virtual-display test cases focused on their mode inputs while
/// serializing the terminal wire shape (which always carries an epoch).
#[derive(Debug, Clone, serde::Deserialize)]
struct ChangeDisplaySettingsPayload {
    width: u32,
    height: u32,
    refresh_hz: u32,
    auto: bool,
}

impl serde::Serialize for ChangeDisplaySettingsPayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        desk_signal_facade::model::virtual_display::ChangeDisplaySettingsPayload {
            connection_epoch: TEST_CONNECTION_EPOCH.to_string(),
            width: self.width,
            height: self.height,
            refresh_hz: self.refresh_hz,
            auto: self.auto,
        }
        .serialize(serializer)
    }
}

async fn make_ctx() -> RouterContext {
    let (outbound_tx, _) = broadcast::channel::<String>(16);
    let shared =
        crate::model::settings::SharedSettings::from(crate::model::settings::Settings::default());
    let settings = web::Data::new(shared);
    let settings_coordinator = Arc::new(
        crate::model::settings_coordinator::SettingsCoordinator::from_settings(
            settings.clone().into_inner(),
        )
        .await,
    );
    let pc_registry = PcRegistry::new();
    let (worker_mgr, _) = WorkerManager::new(settings.clone(), pc_registry.clone());
    #[cfg(target_os = "linux")]
    worker_mgr.set_wayland_portal_snapshot(desk_wayland_portal::PortalSnapshot {
        phase: desk_wayland_portal::PortalPhase::Ready,
        capabilities: desk_wayland_portal::PortalCapabilities {
            screen_ready: true,
            input_ready: true,
        },
        availability: desk_wayland_portal::PortalAvailability::default(),
        target: Some(desk_wayland_portal::AuthorizationTarget::ScreenAndInput),
        operation_id: None,
        generation: 1,
        restore_token_persisted: false,
        requires_local_action: false,
        reason_code: None,
        reason: None,
    });
    let host_control_hub = Arc::new(HostControlHub::new_local());
    host_control_hub
        .remote_access_gate()
        .initialize_from_store(crate::daemon::remote_access::RemoteAccessState::unlocked(1));
    RouterContext {
        exec_capacity: Arc::new(crate::daemon::exec_capacity::ExecCapacity::new()),
        exec_ledger: Arc::new(
            crate::daemon::exec_ledger::ExecLedger::open_in_memory()
                .await
                .expect("in-memory ledger"),
        ),
        pc_registry,
        admission_origin: crate::daemon::pc_manager::AdmissionOrigin::Local,
        manager_credential_link: None,
        outbound_tx,
        settings,
        policy: crate::model::policy_access::PolicyAccess::authoritative(Arc::clone(
            &settings_coordinator,
        )),
        host_control_hub,
        worker_mgr,
        virtual_display: None,
        diagnose_orchestrator: None,
        remote_read: None,
        exec_supported: false,
        exec_approvals: Arc::new(crate::daemon::exec_approval::PendingApprovalStore::new()),
        session_approvals: Arc::new(crate::daemon::session_approval::SessionApprovalStore::new()),
        command_templates: Arc::new(crate::daemon::command_templates::CommandTemplateCache::new()),
        command_blocklist: Arc::new(crate::daemon::command_blocklist::CommandBlocklistCache::new()),
        audit: Arc::new(crate::worker::agent::audit_sink::LogAuditSink),
        inbound_authz: None,
        inbound_request_remote_authz: None,
        inbound_start_terminal_authz: None,
        edge_exec_pending: Default::default(),
        support_link_state: Arc::new(crate::daemon::support_link_state::SupportLinkState::new()),
    }
}

async fn seed_test_desktop_pc(ctx: &RouterContext, connection_id: &str) {
    let request = desk_signal_facade::model::signal::RequestRemoteModel {
        session_target_id: None,
        requested_wayland_control_mode: Some("auto".to_string()),
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
        org_id: None,
    };
    let pc = ctx
        .pc_registry
        .create_for_request_remote(
            connection_id,
            &request,
            &crate::model::settings::Settings::default(),
        )
        .await
        .expect("seed test desktop PC");
    pc.write().await.connection_epoch = TEST_CONNECTION_EPOCH.to_string();
}

mod access_policy;
mod agent;
mod exec_confirm;
mod exec_edge;
mod exec_lifecycle;
mod remote_display;
mod routing;

use agent::{invoke_agent_capability_model, read_outcome};
use routing::{make_ctx_with_attached_supervisor, make_ctx_with_rx, read_response};
