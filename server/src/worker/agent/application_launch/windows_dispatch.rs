//! Keep Windows token, environment and COM state on one native dispatch thread.
use super::{dispatch, journal::LaunchJournal, windows_package, windows_process};
use desk_agent_protocol::application_launch::{
    ApplicationTargetKind, LaunchApplicationResult, LaunchFailureReason,
};
use desk_diagnose_core::application_launch::{LaunchApprovalBinding, LaunchApprovalSubject};
use std::sync::Arc;

enum Prepared {
    UserHost(crate::windows_application_host::PreparedHost),
    Executable(windows_process::PreparedExecutable),
    Package,
}

pub(crate) async fn run(
    journal: Arc<LaunchJournal>,
    dispatch_id: String,
    binding: LaunchApprovalBinding,
    subject: LaunchApprovalSubject,
    authority: Arc<dyn Fn() -> Result<(), LaunchFailureReason> + Send + Sync>,
) -> LaunchApplicationResult {
    let fallback = dispatch::receipt(
        &binding,
        &dispatch_id,
        desk_agent_protocol::application_launch::LaunchOutcome::OutcomeUnknown,
        None,
    );
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        // PreparedExecutable owns thread-local native handles; it never crosses
        // the blocking thread boundary or enters the managed command runner.
        let binding_ref = &binding;
        runtime.block_on(dispatch::dispatch(
            &journal,
            &dispatch_id,
            &binding,
            &subject,
            || authority(),
            || async {
                let request = binding.request();
                if !request.run_as_admin {
                    let host = crate::windows_application_host::prepare(request).await?;
                    let identity = host.identity.clone();
                    return Ok((Prepared::UserHost(host), identity));
                }
                match request.target.kind {
                    ApplicationTargetKind::Executable => {
                        let identity = binding.identity();
                        let prepared = windows_process::prepare(
                            request,
                            &identity.user_identity,
                            identity
                                .session_identity
                                .parse()
                                .map_err(|_| LaunchFailureReason::SessionUnavailable)?,
                        )?;
                        let identity = prepared.identity.clone();
                        Ok((Prepared::Executable(prepared), identity))
                    }
                    ApplicationTargetKind::WindowsAppId => {
                        let identity = windows_package::resolve(request)?;
                        Ok((Prepared::Package, identity))
                    }
                    ApplicationTargetKind::MacosBundle => Err(LaunchFailureReason::Unsupported),
                }
            },
            |prepared, id| async move {
                match prepared {
                    Prepared::UserHost(host) => host.invoke(id).await,
                    Prepared::Executable(prepared) => prepared.invoke(id),
                    Prepared::Package => {
                        windows_package::invoke(binding_ref.request(), binding_ref.identity(), id)
                    }
                }
            },
        ))
    })
    .await
    .unwrap_or(fallback)
}
