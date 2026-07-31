use super::*;

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
        diagnose_tasks: Default::default(),
        inbound_authz: None,
        inbound_request_remote_authz: None,
        inbound_start_terminal_authz: None,
        edge_exec_pending: Default::default(),
        support_link_state: Arc::new(crate::daemon::support_link_state::SupportLinkState::new()),
    }
}

mod access_policy;
mod agent;
mod diagnose;
mod exec_confirm;
mod exec_edge;
mod exec_lifecycle;
mod remote_display;
mod routing;

use agent::{agent_request_model, read_outcome};
use routing::{make_ctx_with_attached_supervisor, make_ctx_with_rx, read_response};
