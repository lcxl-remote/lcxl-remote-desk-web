//! Private ordinary-user launch host; no server, UI, or model endpoint is started.
use crate::worker::agent::application_launch::{windows_package, windows_process};
use desk_agent_protocol::application_launch::*;
use serde::{Deserialize, Serialize};
use std::{io, os::windows::io::AsRawHandle, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions},
};
use windows::{
    Win32::{
        Foundation::{HANDLE, HLOCAL, LocalFree},
        Security::{
            Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
            PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
        },
        System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId},
    },
    core::PCWSTR,
};

pub const MODE: &str = "application-launch-user-host";
const PIPE_PREFIX: &str = r"\\.\pipe\lrd-application-host-";
const MAX_FRAME: usize = 128 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    version: u32,
    user_sid: String,
    session_id: u32,
    request: LaunchApplicationRequest,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Invoke {
    dispatch_id: String,
}

async fn read_frame<T: serde::de::DeserializeOwned>(
    reader: &mut (impl AsyncRead + Unpin),
) -> io::Result<T> {
    let size = reader.read_u32_le().await? as usize;
    if size == 0 || size > MAX_FRAME {
        return Err(io::Error::other("invalid launch host frame"));
    }
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}
async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &impl Serialize,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.is_empty() || bytes.len() > MAX_FRAME {
        return Err(io::Error::other("oversized launch host frame"));
    }
    writer.write_u32_le(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

fn server(pipe: &str, sid: &str) -> io::Result<NamedPipeServer> {
    if !sid.starts_with("S-1-")
        || !sid
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'-' || b == b'S')
    {
        return Err(io::Error::other("invalid launch host user"));
    }
    let descriptor: Vec<u16> = format!("D:P(A;;GA;;;{sid})S:(ML;;NW;;;ME)")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut security = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(descriptor.as_ptr()),
            1,
            &mut security,
            None,
        )
        .map_err(io::Error::other)?;
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: security.0,
            bInheritHandle: false.into(),
        };
        let result = ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                pipe,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            );
        let _ = LocalFree(Some(HLOCAL(security.0)));
        result
    }
}

pub(crate) struct PreparedHost {
    pipe: NamedPipeServer,
    pub(crate) identity: ResolvedApplicationIdentity,
    request: LaunchApplicationRequest,
}
impl PreparedHost {
    pub(crate) async fn invoke(mut self, dispatch_id: String) -> LaunchApplicationResult {
        let unknown = LaunchApplicationResult {
            dispatch_id: dispatch_id.clone(),
            launch_outcome: LaunchOutcome::OutcomeUnknown,
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
        let result = tokio::time::timeout(TIMEOUT, async {
            write_frame(
                &mut self.pipe,
                &Invoke {
                    dispatch_id: dispatch_id.clone(),
                },
            )
            .await?;
            read_frame::<LaunchApplicationResult>(&mut self.pipe).await
        })
        .await;
        match result {
            Ok(Ok(result)) if result.dispatch_id == dispatch_id && !result.requested_admin => {
                result
            }
            _ => unknown,
        }
    }
}

pub(crate) async fn prepare(
    request: &LaunchApplicationRequest,
) -> Result<PreparedHost, LaunchFailureReason> {
    if request.run_as_admin {
        return Err(LaunchFailureReason::PermissionDenied);
    }
    request
        .validate()
        .map_err(|_| LaunchFailureReason::InvalidTarget)?;
    let session = windows_process::catalog_session_identity()?;
    let (sid, session_id) = session
        .rsplit_once(':')
        .ok_or(LaunchFailureReason::SessionUnavailable)?;
    let session_id = session_id
        .parse()
        .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
    let name = format!("{PIPE_PREFIX}{}", uuid::Uuid::new_v4());
    let pipe = server(&name, sid).map_err(|_| LaunchFailureReason::NativeFailure)?;
    let executable = std::env::current_exe().map_err(|_| LaunchFailureReason::NativeFailure)?;
    let executable = executable
        .to_str()
        .ok_or(LaunchFailureReason::InvalidTarget)?;
    let helper = LaunchApplicationRequest {
        target: ApplicationTarget {
            kind: ApplicationTargetKind::Executable,
            value: executable.into(),
        },
        args: vec![MODE.into(), name, std::process::id().to_string()],
        cwd: None,
        run_as_admin: false,
    };
    let created = windows_process::prepare(&helper, sid, session_id)?
        .invoke_hidden_host(uuid::Uuid::new_v4().to_string());
    let pid = created.created_process_id.ok_or(
        created
            .failure_reason
            .unwrap_or(LaunchFailureReason::NativeFailure),
    )?;
    let mut pipe = pipe;
    let exchange = tokio::time::timeout(TIMEOUT, async {
        pipe.connect()
            .await
            .map_err(|_| LaunchFailureReason::NativeFailure)?;
        let mut peer = 0;
        unsafe { GetNamedPipeClientProcessId(HANDLE(pipe.as_raw_handle()), &mut peer) }
            .map_err(|_| LaunchFailureReason::PermissionDenied)?;
        if peer != pid {
            return Err(LaunchFailureReason::PermissionDenied);
        }
        write_frame(
            &mut pipe,
            &Prepare {
                version: 1,
                user_sid: sid.into(),
                session_id,
                request: request.clone(),
            },
        )
        .await
        .map_err(|_| LaunchFailureReason::NativeFailure)?;
        let identity: Result<ResolvedApplicationIdentity, LaunchFailureReason> =
            read_frame(&mut pipe)
                .await
                .map_err(|_| LaunchFailureReason::NativeFailure)?;
        identity
    })
    .await
    .map_err(|_| LaunchFailureReason::NativeFailure)??;
    Ok(PreparedHost {
        pipe,
        identity: exchange,
        request: request.clone(),
    })
}

/// Strict private entry point shared by the standalone server and Tauri binary.
/// This runs before either application initializes a UI, settings, or logging.
pub fn run_if_requested() -> Option<i32> {
    let mut args = std::env::args_os();
    let _ = args.next();
    if args.next().as_deref() != Some(std::ffi::OsStr::new(MODE)) {
        return None;
    }
    let parsed = (|| {
        let name = args.next()?.into_string().ok()?;
        let pid: u32 = args.next()?.into_string().ok()?.parse().ok()?;
        let suffix = name.strip_prefix(PIPE_PREFIX)?;
        uuid::Uuid::parse_str(suffix).ok()?;
        if pid == 0 || args.next().is_some() {
            return None;
        }
        Some((name, pid))
    })();
    let Some((name, pid)) = parsed else {
        return Some(2);
    };
    // The watchdog exits only this helper, never its launched process tree.
    // Parent-side Invoking remains unknown if the native call does not return.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(50));
        std::process::exit(124);
    });
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(v) => v,
        Err(_) => return Some(1),
    };
    Some(match runtime.block_on(serve(&name, pid)) {
        Ok(()) => 0,
        Err(_) => 1,
    })
}

async fn serve(name: &str, parent_pid: u32) -> io::Result<()> {
    if windows_process::current_elevated().map_err(|_| io::Error::other("invalid host token"))? {
        return Err(io::Error::other("host must be an ordinary user"));
    }
    let mut pipe = ClientOptions::new().open(name)?;
    let mut server_pid = 0;
    unsafe { GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut server_pid) }
        .map_err(io::Error::other)?;
    if server_pid != parent_pid {
        return Err(io::Error::other("unexpected host parent"));
    }
    let request: Prepare = tokio::time::timeout(TIMEOUT, read_frame(&mut pipe))
        .await
        .map_err(io::Error::other)??;
    let session = windows_process::catalog_session_identity()
        .map_err(|_| io::Error::other("host user unavailable"))?;
    if request.version != 1
        || request.request.run_as_admin
        || session != format!("{}:{}", request.user_sid, request.session_id)
    {
        return Err(io::Error::other("launch host identity mismatch"));
    }
    let executable = if request.request.target.kind == ApplicationTargetKind::Executable {
        Some(windows_process::prepare(
            &request.request,
            &request.user_sid,
            request.session_id,
        ))
    } else {
        None
    };
    let identity = match &executable {
        Some(Ok(prepared)) => Ok(prepared.identity.clone()),
        Some(Err(reason)) => Err(*reason),
        None => windows_package::resolve(&request.request),
    };
    write_frame(&mut pipe, &identity).await?;
    let Ok(identity) = identity else {
        return Ok(());
    };
    let invoke: Invoke = tokio::time::timeout(TIMEOUT, read_frame(&mut pipe))
        .await
        .map_err(io::Error::other)??;
    if invoke.dispatch_id.is_empty()
        || invoke.dispatch_id.len() > 256
        || invoke.dispatch_id.chars().any(char::is_control)
    {
        return Err(io::Error::other("invalid launch dispatch"));
    }
    let result = match executable {
        Some(Ok(prepared)) => prepared.invoke(invoke.dispatch_id),
        _ => windows_package::invoke(&request.request, &identity, invoke.dispatch_id),
    };
    write_frame(&mut pipe, &result).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn private_host_pipe_uses_a_valid_local_namespace() {
        let sid = windows_process::storage_user_sid().unwrap();
        let name = format!("{PIPE_PREFIX}{}", uuid::Uuid::new_v4());
        let pipe = server(&name, &sid).expect("private launch pipe must be creatable");
        let client = ClientOptions::new().open(&name).unwrap();
        pipe.connect().await.unwrap();
        let mut peer = 0;
        unsafe { GetNamedPipeClientProcessId(HANDLE(pipe.as_raw_handle()), &mut peer) }.unwrap();
        assert_eq!(peer, std::process::id());
        drop(client);
    }
    #[tokio::test]
    async fn protocol_rejects_oversized_and_unreviewed_fields() {
        let (mut writer, mut reader) = tokio::io::duplex(32);
        writer.write_u32_le((MAX_FRAME + 1) as u32).await.unwrap();
        assert!(read_frame::<Invoke>(&mut reader).await.is_err());
        assert!(
            serde_json::from_str::<Invoke>(r#"{"dispatch_id":"one","run_as_admin":true}"#).is_err()
        );
        assert!(serde_json::from_str::<Prepare>(r#"{"version":1,"user_sid":"S-1-5-21-1","session_id":1,"request":{"target":{"kind":"executable","value":"C:\\app.exe"}},"command":"unreviewed"}"#).is_err());
    }
    #[tokio::test]
    async fn closing_preflight_without_invoke_is_not_an_invocation() {
        let (writer, mut reader) = tokio::io::duplex(32);
        drop(writer);
        assert_eq!(
            read_frame::<Invoke>(&mut reader).await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}
