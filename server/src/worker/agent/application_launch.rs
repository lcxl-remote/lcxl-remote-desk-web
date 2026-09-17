//! User-session application discovery and independent launch dispatch.
pub(crate) mod admitted;
pub(crate) mod catalog;
pub(crate) mod catalog_store;
pub(crate) mod dispatch;
pub(crate) mod journal;
#[cfg(target_os = "linux")]
mod linux_environment;
#[cfg(target_os = "linux")]
pub(crate) mod linux_process;
#[cfg(target_os = "linux")]
mod linux_session;
#[cfg(target_os = "macos")]
pub(crate) mod macos_process;
#[cfg(unix)]
pub(crate) mod unix_catalog_host;
#[cfg(windows)]
pub(crate) mod windows_package;

pub(crate) async fn resolve(
    request: desk_agent_protocol::application_launch::LaunchApplicationRequest,
) -> Result<
    desk_agent_protocol::application_launch::ResolvedApplicationIdentity,
    desk_agent_protocol::application_launch::LaunchFailureReason,
> {
    use desk_agent_protocol::application_launch::{ApplicationTargetKind, LaunchFailureReason};
    request
        .validate()
        .map_err(|_| LaunchFailureReason::InvalidTarget)?;
    #[cfg(windows)]
    {
        if !request.run_as_admin {
            return crate::windows_application_host::prepare(&request)
                .await
                .map(|host| host.identity);
        }
        tokio::task::spawn_blocking(move || match request.target.kind {
            ApplicationTargetKind::Executable => {
                let session = windows_process::catalog_session_identity()?;
                let (sid, session) = session
                    .rsplit_once(':')
                    .ok_or(LaunchFailureReason::SessionUnavailable)?;
                let prepared = windows_process::prepare(
                    &request,
                    sid,
                    session
                        .parse()
                        .map_err(|_| LaunchFailureReason::SessionUnavailable)?,
                )?;
                Ok(prepared.identity.clone())
            }
            ApplicationTargetKind::WindowsAppId => windows_package::resolve(&request),
            ApplicationTargetKind::MacosBundle => Err(LaunchFailureReason::Unsupported),
        })
        .await
        .map_err(|_| LaunchFailureReason::NativeFailure)?
    }
    #[cfg(target_os = "macos")]
    {
        tokio::task::spawn_blocking(move || macos_process::resolve(&request))
            .await
            .map_err(|_| LaunchFailureReason::NativeFailure)?
    }
    #[cfg(target_os = "linux")]
    {
        linux_process::prepare(&request)
            .await
            .map(|prepared| prepared.identity)
    }
}
#[cfg(windows)]
pub(crate) mod windows_process;

#[cfg(windows)]
mod windows_dispatch;

/// Dispatch is journaled independently of task cancellation and command cleanup.
/// Authority is checked again immediately before the one native submission.
pub(crate) async fn execute(
    journal: std::sync::Arc<journal::LaunchJournal>,
    dispatch_id: String,
    binding: desk_diagnose_core::application_launch::LaunchApprovalBinding,
    subject: desk_diagnose_core::application_launch::LaunchApprovalSubject,
    authority: std::sync::Arc<
        dyn Fn() -> Result<(), desk_agent_protocol::application_launch::LaunchFailureReason>
            + Send
            + Sync,
    >,
) -> desk_agent_protocol::application_launch::LaunchApplicationResult {
    #[cfg(windows)]
    {
        windows_dispatch::run(journal, dispatch_id, binding, subject, authority).await
    }
    #[cfg(target_os = "linux")]
    {
        dispatch::dispatch(
            &journal,
            &dispatch_id,
            &binding,
            &subject,
            || authority(),
            || async {
                let prepared = linux_process::prepare(binding.request()).await?;
                let identity = prepared.identity.clone();
                Ok((prepared, identity))
            },
            |prepared, id| prepared.invoke(id),
        )
        .await
    }
    #[cfg(target_os = "macos")]
    {
        let fallback = dispatch::receipt(
            &binding,
            &dispatch_id,
            desk_agent_protocol::application_launch::LaunchOutcome::OutcomeUnknown,
            None,
        );
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let binding_ref = &binding;
            runtime.block_on(dispatch::dispatch(
                &journal,
                &dispatch_id,
                &binding,
                &subject,
                || authority(),
                || async { Ok(((), macos_process::resolve(binding.request())?)) },
                |(), id| async move {
                    macos_process::invoke(binding_ref.request(), binding_ref.identity(), id)
                },
            ))
        })
        .await
        .unwrap_or(fallback)
    }
}
