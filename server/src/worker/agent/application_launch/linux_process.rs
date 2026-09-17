//! User-manager transient services hand off cgroup lifetime independently of workers.
use desk_agent_protocol::application_launch::{
    ApplicationTargetKind, ArgumentDelivery, LaunchApplicationRequest, LaunchApplicationResult,
    LaunchFailureReason, LaunchOutcome,
};
use desk_diagnose_core::application_launch::ResolvedApplicationIdentity;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::CString,
    io::Read,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::Path,
    time::Duration,
};
use zbus::zvariant::{OwnedObjectPath, Value};

pub(crate) struct PreparedExecutable {
    request: LaunchApplicationRequest,
    pub(crate) identity: ResolvedApplicationIdentity,
    connection: zbus::Connection,
    environment: Vec<String>,
    unset_environment: Vec<String>,
}

async fn manager(connection: &zbus::Connection) -> Result<zbus::Proxy<'_>, LaunchFailureReason> {
    zbus::Proxy::new(
        connection,
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
    )
    .await
    .map_err(|_| LaunchFailureReason::LifetimeIsolationUnavailable)
}

pub(crate) async fn prepare(
    request: &LaunchApplicationRequest,
) -> Result<PreparedExecutable, LaunchFailureReason> {
    request
        .validate()
        .map_err(|_| LaunchFailureReason::InvalidTarget)?;
    if request.run_as_admin || request.target.kind != ApplicationTargetKind::Executable {
        return Err(LaunchFailureReason::Unsupported);
    }
    let host =
        super::unix_catalog_host::current().map_err(|_| LaunchFailureReason::SessionUnavailable)?;
    let runtime =
        std::env::var("XDG_RUNTIME_DIR").map_err(|_| LaunchFailureReason::SessionUnavailable)?;
    let bus_path = Path::new(&runtime).join("bus");
    let bus_metadata = std::fs::symlink_metadata(&bus_path)
        .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
    if bus_metadata.uid() != unsafe { libc::geteuid() } || !bus_metadata.file_type().is_socket() {
        return Err(LaunchFailureReason::SessionUnavailable);
    }
    // Use the verified user's runtime bus, never a model/service-provided address.
    let address = format!(
        "unix:path={}",
        bus_path
            .to_str()
            .ok_or(LaunchFailureReason::SessionUnavailable)?
    );
    if address.contains(',') || address.contains(';') || address.contains('%') {
        return Err(LaunchFailureReason::SessionUnavailable);
    }
    let builder = zbus::connection::Builder::address(address.as_str())
        .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
    let connection = tokio::time::timeout(Duration::from_secs(5), builder.build())
        .await
        .map_err(|_| LaunchFailureReason::SessionUnavailable)?
        .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
    super::linux_session::verify(&connection, &host.session_id).await?;
    let manager = manager(&connection).await?;
    let version: String = manager
        .get_property("Version")
        .await
        .map_err(|_| LaunchFailureReason::LifetimeIsolationUnavailable)?;
    let major = version
        .trim_start_matches('v')
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|v| v.parse::<u32>().ok());
    // ExitType=cgroup keeps forked applications alive after the initial process exits.
    if major.is_none_or(|v| v < 250) {
        return Err(LaunchFailureReason::LifetimeIsolationUnavailable);
    }
    let manager_environment: Vec<String> = manager
        .get_property("Environment")
        .await
        .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
    let mut session = BTreeMap::new();
    for key in super::linux_environment::SESSION_KEYS {
        if let Some(value) = std::env::var_os(key) {
            session.insert(
                (*key).to_owned(),
                value
                    .into_string()
                    .map_err(|_| LaunchFailureReason::Unsupported)?,
            );
        }
    }
    let (environment, unset_environment) = super::linux_environment::build(
        &manager_environment,
        &session,
        host.home
            .to_str()
            .ok_or(LaunchFailureReason::SessionUnavailable)?,
        &host.user_name,
        &runtime,
    )?;
    let identity = resolve_identity(request, &host.home, &host.session_id)?;
    Ok(PreparedExecutable {
        request: request.clone(),
        identity,
        connection,
        environment,
        unset_environment,
    })
}

fn resolve_identity(
    request: &LaunchApplicationRequest,
    home: &Path,
    session: &str,
) -> Result<ResolvedApplicationIdentity, LaunchFailureReason> {
    let target = Path::new(&request.target.value);
    let cwd = request.cwd.as_deref().map(Path::new).unwrap_or(home);
    if !target.is_absolute() || !cwd.is_absolute() || !cwd.is_dir() {
        return Err(LaunchFailureReason::InvalidTarget);
    }
    let target = std::fs::canonicalize(target).map_err(|_| LaunchFailureReason::InvalidTarget)?;
    let cwd = std::fs::canonicalize(cwd).map_err(|_| LaunchFailureReason::InvalidTarget)?;
    let target_text = target.to_str().ok_or(LaunchFailureReason::InvalidTarget)?;
    let c_path = CString::new(target_text).map_err(|_| LaunchFailureReason::InvalidTarget)?;
    if unsafe { libc::access(c_path.as_ptr(), libc::X_OK) } != 0 {
        return Err(LaunchFailureReason::PermissionDenied);
    }
    let mut file = std::fs::File::open(&target).map_err(|_| LaunchFailureReason::InvalidTarget)?;
    let metadata = file
        .metadata()
        .map_err(|_| LaunchFailureReason::InvalidTarget)?;
    if !metadata.is_file() || metadata.mode() & 0o6000 != 0 {
        return Err(LaunchFailureReason::ElevationRequired);
    }
    let mut hash = Sha256::new();
    hash.update(metadata.dev().to_le_bytes());
    hash.update(metadata.ino().to_le_bytes());
    let mut bytes = [0u8; 65536];
    loop {
        let read = file
            .read(&mut bytes)
            .map_err(|_| LaunchFailureReason::InvalidTarget)?;
        if read == 0 {
            break;
        }
        hash.update(&bytes[..read]);
    }
    Ok(ResolvedApplicationIdentity {
        canonical_target: target_text.into(),
        identity_digest: format!("{:x}", hash.finalize()),
        resolved_cwd: Some(
            cwd.to_str()
                .ok_or(LaunchFailureReason::InvalidTarget)?
                .into(),
        ),
        user_identity: super::linux_session::current_identity()?,
        session_identity: session.into(),
    })
}

impl PreparedExecutable {
    /// The durable Invoking claim and current approval must precede this method.
    pub(crate) async fn invoke(self, dispatch_id: String) -> LaunchApplicationResult {
        let mut result = LaunchApplicationResult {
            dispatch_id: dispatch_id.clone(),
            launch_outcome: LaunchOutcome::LaunchFailed,
            argument_delivery: if self.request.args.is_empty() {
                ArgumentDelivery::NotRequested
            } else {
                ArgumentDelivery::Unknown
            },
            failure_reason: None,
            requested_admin: false,
            created_process_id: None,
            created_process_elevated: None,
            observations: vec![],
        };
        let host = match super::unix_catalog_host::current() {
            Ok(host) => host,
            Err(_) => {
                result.failure_reason = Some(LaunchFailureReason::SessionUnavailable);
                return result;
            }
        };
        if resolve_identity(&self.request, &host.home, &host.session_id).as_ref()
            != Ok(&self.identity)
        {
            result.failure_reason = Some(LaunchFailureReason::IdentityChanged);
            return result;
        }
        if let Err(reason) = super::linux_session::verify(&self.connection, &host.session_id).await
        {
            result.failure_reason = Some(reason);
            return result;
        }
        let manager = match manager(&self.connection).await {
            Ok(manager) => manager,
            Err(reason) => {
                result.failure_reason = Some(reason);
                return result;
            }
        };
        let name = format!(
            "lcxl-application-{:x}.service",
            Sha256::digest(dispatch_id.as_bytes())
        );
        let argv: Vec<String> = std::iter::once(self.identity.canonical_target.clone())
            .chain(self.request.args.clone())
            .collect();
        let properties = vec![
            ("Description", Value::from("LCXL approved application")),
            ("Type", Value::from("exec")),
            ("ExitType", Value::from("cgroup")),
            ("CollectMode", Value::from("inactive-or-failed")),
            ("NoNewPrivileges", Value::from(true)),
            (
                "WorkingDirectory",
                Value::from(self.identity.resolved_cwd.as_deref().expect("prepared cwd")),
            ),
            ("Environment", Value::from(self.environment)),
            ("UnsetEnvironment", Value::from(self.unset_environment)),
            (
                "ExecStart",
                Value::from(vec![(self.identity.canonical_target.clone(), argv, false)]),
            ),
        ];
        // No BindsTo/PartOf link to the daemon, no task cancellation StopUnit call.
        let auxiliary: Vec<(&str, Vec<(&str, Value<'_>)>)> = vec![];
        let parameters = (name, "fail", properties, auxiliary);
        let submitted = tokio::time::timeout(
            Duration::from_secs(15),
            manager.call::<_, _, OwnedObjectPath>("StartTransientUnit", &parameters),
        )
        .await;
        match submitted {
            Ok(Ok(_)) => {
                result.launch_outcome = LaunchOutcome::LaunchAccepted;
                result.argument_delivery = if self.request.args.is_empty() {
                    ArgumentDelivery::NotRequested
                } else {
                    ArgumentDelivery::Submitted
                };
            }
            // The bus may have disconnected after submission. Never reinterpret
            // transport failure as proof that no user unit was created.
            Ok(Err(_)) | Err(_) => result.launch_outcome = LaunchOutcome::OutcomeUnknown,
        }
        result
    }
}
