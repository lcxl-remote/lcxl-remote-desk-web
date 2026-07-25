use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::local_access_control::{
    HostAccessControlAction, LocalAccessAuthChallenge, LocalAccessAuthChallengeRequest,
    LocalAccessControlRequest, LocalAccessControlResult, LocalAccessControlService, LocalAuthProof,
    VerifiedLocalPeer, elevated_proof_signature,
};
use crate::host_control::HostRemoteAccessStatus;

const MAX_FRAME_BYTES: usize = 1024 * 1024;
#[cfg(target_os = "windows")]
pub const LOCAL_ACCESS_CONTROL_ENDPOINT: &str = r"\\.\pipe\lcxl-remote-desk-local-access-control";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NativeLocalAccessRequest {
    Status,
    Challenge(LocalAccessAuthChallengeRequest),
    Execute(LocalAccessControlRequest),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NativeLocalAccessResponse {
    Status { status: HostRemoteAccessStatus },
    Challenge { challenge: LocalAccessAuthChallenge },
    Result { result: LocalAccessControlResult },
    Error { message: String },
}

pub fn endpoint_for_config(config_file_path: &str) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let _ = config_file_path;
        PathBuf::from(LOCAL_ACCESS_CONTROL_ENDPOINT)
    }
    #[cfg(not(target_os = "windows"))]
    {
        Path::new(config_file_path)
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("remote-access-control.sock")
    }
}

async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<NativeLocalAccessRequest> {
    let len = reader.read_u32_le().await? as usize;
    if len == 0 || len > MAX_FRAME_BYTES {
        bail!("invalid local-access frame length");
    }
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).context("invalid local-access request")
}

async fn write_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    response: &NativeLocalAccessResponse,
) -> Result<()> {
    let bytes = serde_json::to_vec(response)?;
    writer.write_u32_le(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

async fn write_request<W: AsyncWrite + Unpin>(
    writer: &mut W,
    request: &NativeLocalAccessRequest,
) -> Result<()> {
    let bytes = serde_json::to_vec(request)?;
    writer.write_u32_le(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_response<R: AsyncRead + Unpin>(reader: &mut R) -> Result<NativeLocalAccessResponse> {
    let len = reader.read_u32_le().await? as usize;
    if len == 0 || len > MAX_FRAME_BYTES {
        bail!("invalid local-access response length");
    }
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).context("invalid local-access response")
}

async fn handle_connection<S>(
    mut stream: S,
    peer: VerifiedLocalPeer,
    service: Arc<LocalAccessControlService>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let request = match read_frame(&mut stream).await {
            Ok(request) => request,
            Err(error) if is_disconnect(&error) => return Ok(()),
            Err(error) => return Err(error),
        };
        let response = match request {
            NativeLocalAccessRequest::Status => NativeLocalAccessResponse::Status {
                status: service.status(),
            },
            NativeLocalAccessRequest::Challenge(request) => {
                match service.issue_challenge(&peer, request) {
                    Ok(challenge) => NativeLocalAccessResponse::Challenge { challenge },
                    Err(error) => NativeLocalAccessResponse::Error {
                        message: error.to_string(),
                    },
                }
            }
            NativeLocalAccessRequest::Execute(request) => {
                match service.execute(&peer, request).await {
                    Ok(result) => NativeLocalAccessResponse::Result { result },
                    Err(error) => NativeLocalAccessResponse::Error {
                        message: error.to_string(),
                    },
                }
            }
        };
        write_response(&mut stream, &response).await?;
    }
}

fn is_disconnect(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
        )
    })
}

fn executable_is_from_installation(path: &Path) -> bool {
    let Ok(client) = path.canonicalize() else {
        return false;
    };
    let Ok(daemon) = std::env::current_exe().and_then(|path| path.canonicalize()) else {
        return false;
    };
    if client.parent() != daemon.parent() {
        return false;
    }
    let name = client
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        name.as_str(),
        "lcxl-remote-desk-server"
            | "lcxl-remote-desk-server.exe"
            | "lcxl-remote-desk-tauri"
            | "lcxl-remote-desk-tauri.exe"
    )
}

#[cfg(target_os = "windows")]
pub async fn serve(_endpoint: PathBuf, service: Arc<LocalAccessControlService>) -> Result<()> {
    loop {
        let session_id = super::session_monitor::get_active_session_id();
        let session_sid = match super::pipe_security::query_session_user_sid(session_id) {
            Ok(sid) => sid,
            Err(error) => {
                log::warn!("failed to resolve active-session SID for local-access pipe: {error}");
                None
            }
        };
        let daemon_sid = if session_sid.is_none() {
            match super::pipe_security::query_current_process_user_sid() {
                Ok(sid) => Some(sid),
                Err(error) => {
                    log::warn!(
                        "failed to resolve daemon user SID for local-access pipe fallback: {error}"
                    );
                    None
                }
            }
        } else {
            None
        };
        let allowed_sid = choose_local_access_user_sid(session_sid, daemon_sid);
        let sddl = super::pipe_security::build_pipe_sddl(allowed_sid.as_deref());
        let server = create_windows_pipe(LOCAL_ACCESS_CONTROL_ENDPOINT, &sddl)?;
        server.connect().await?;
        let peer = windows_peer(&server)?;
        let service = service.clone();
        actix_web::rt::spawn(async move {
            if let Err(error) = handle_connection(server, peer, service).await {
                log::warn!("local-access named-pipe client failed: {error:#}");
            }
        });
    }
}

fn choose_local_access_user_sid(
    session_sid: Option<String>,
    daemon_sid: Option<String>,
) -> Option<String> {
    session_sid.or(daemon_sid)
}

#[cfg(target_os = "windows")]
fn create_windows_pipe(
    endpoint: &str,
    sddl: &str,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt as _;
    use tokio::net::windows::named_pipe::ServerOptions;
    use windows::Win32::Foundation::{FALSE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_core::PCWSTR;

    let wide: Vec<u16> = std::ffi::OsStr::new(sddl)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(wide.as_ptr()),
            1,
            &mut descriptor,
            None,
        )?;
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0 as *mut c_void,
            bInheritHandle: FALSE,
        };
        let result = ServerOptions::new().create_with_security_attributes_raw(
            endpoint,
            &mut attributes as *mut _ as *mut c_void,
        );
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        Ok(result?)
    }
}

#[cfg(target_os = "windows")]
fn windows_peer(
    pipe: &tokio::net::windows::named_pipe::NamedPipeServer,
) -> Result<VerifiedLocalPeer> {
    use std::os::windows::io::AsRawHandle as _;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Pipes::GetNamedPipeClientProcessId;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows_core::PWSTR;

    let pipe_handle = HANDLE(pipe.as_raw_handle());
    let mut pid = 0u32;
    unsafe { GetNamedPipeClientProcessId(pipe_handle, &mut pid)? };
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)? };
    let mut path = vec![0u16; 32768];
    let mut len = path.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            process,
            Default::default(),
            PWSTR(path.as_mut_ptr()),
            &mut len,
        )?;
    }
    let executable_path = PathBuf::from(String::from_utf16(&path[..len as usize])?);
    let (user_id, elevated) = windows_token_identity(process)?;
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(process);
    }
    Ok(VerifiedLocalPeer::from_native_transport(
        pid,
        user_id,
        executable_path.clone(),
        elevated,
        executable_is_from_installation(&executable_path),
    ))
}

#[cfg(target_os = "windows")]
fn windows_token_identity(process: windows::Win32::Foundation::HANDLE) -> Result<(String, bool)> {
    use std::ffi::c_void;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER, TokenElevation, TokenUser,
    };
    use windows::Win32::System::Threading::OpenProcessToken;
    use windows_core::PWSTR;

    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token)? };
    let result = (|| -> Result<(String, bool)> {
        let mut needed = 0u32;
        unsafe {
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        }
        let mut buffer = vec![0u8; needed as usize];
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(buffer.as_mut_ptr() as *mut c_void),
                needed,
                &mut needed,
            )?;
        }
        let user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
        let mut sid = PWSTR::null();
        unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid)? };
        let user_id = unsafe { sid.to_string()? };
        unsafe {
            let _ = LocalFree(Some(HLOCAL(sid.0 as *mut _)));
        }

        let mut elevation = TOKEN_ELEVATION::default();
        let mut elevation_size = std::mem::size_of::<TOKEN_ELEVATION>() as u32;
        unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut elevation as *mut _ as *mut c_void),
                elevation_size,
                &mut elevation_size,
            )?;
        }
        Ok((user_id, elevation.TokenIsElevated != 0))
    })();
    unsafe {
        let _ = CloseHandle(token);
    }
    result
}

#[cfg(unix)]
pub async fn serve(endpoint: PathBuf, service: Arc<LocalAccessControlService>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    use tokio::net::UnixListener;

    if endpoint.exists() {
        std::fs::remove_file(&endpoint)?;
    }
    if let Some(parent) = endpoint.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&endpoint)?;
    std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600))?;
    loop {
        let (stream, _) = listener.accept().await?;
        let credentials = stream.peer_cred()?;
        let pid = credentials
            .pid()
            .and_then(|pid| u32::try_from(pid).ok())
            .context("local-access peer pid unavailable")?;
        let uid = credentials.uid();
        let executable_path = unix_executable_path(pid)?;
        let peer = VerifiedLocalPeer::from_native_transport(
            pid,
            uid.to_string(),
            executable_path.clone(),
            uid == 0,
            executable_is_from_installation(&executable_path),
        );
        let service = service.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, peer, service).await {
                log::warn!("local-access unix client failed: {error:#}");
            }
        });
    }
}

#[cfg(target_os = "linux")]
fn unix_executable_path(pid: u32) -> Result<PathBuf> {
    Ok(std::fs::read_link(format!("/proc/{pid}/exe"))?)
}

#[cfg(target_os = "macos")]
fn unix_executable_path(pid: u32) -> Result<PathBuf> {
    libproc::libproc::proc_pid::pidpath(pid as i32)
        .map(PathBuf::from)
        .map_err(anyhow::Error::msg)
}

pub async fn execute_native(
    endpoint: &Path,
    request_id: String,
    action: HostAccessControlAction,
) -> Result<LocalAccessControlResult> {
    #[cfg(target_os = "windows")]
    let mut stream = tokio::net::windows::named_pipe::ClientOptions::new()
        .open(endpoint.to_string_lossy().as_ref())?;
    #[cfg(unix)]
    let mut stream = tokio::net::UnixStream::connect(endpoint).await?;

    let auth_proof = if let HostAccessControlAction::Unlock { expected_version } = action {
        write_request(
            &mut stream,
            &NativeLocalAccessRequest::Challenge(LocalAccessAuthChallengeRequest {
                request_id: request_id.clone(),
                action: HostAccessControlAction::Unlock { expected_version },
                expected_version,
            }),
        )
        .await?;
        let challenge = match read_response(&mut stream).await? {
            NativeLocalAccessResponse::Challenge { challenge } => challenge,
            NativeLocalAccessResponse::Error { message } => bail!(message),
            _ => bail!("unexpected local-access challenge response"),
        };
        Some(LocalAuthProof {
            nonce: challenge.nonce.clone(),
            action_digest: challenge.action_digest,
            signature: elevated_proof_signature(&challenge),
        })
    } else {
        None
    };

    write_request(
        &mut stream,
        &NativeLocalAccessRequest::Execute(LocalAccessControlRequest {
            request_id,
            action,
            auth_proof,
        }),
    )
    .await?;
    match read_response(&mut stream).await? {
        NativeLocalAccessResponse::Result { result } => Ok(result),
        NativeLocalAccessResponse::Error { message } => bail!(message),
        _ => bail!("unexpected local-access result response"),
    }
}

pub async fn query_native(endpoint: &Path) -> Result<HostRemoteAccessStatus> {
    #[cfg(target_os = "windows")]
    let mut stream = tokio::net::windows::named_pipe::ClientOptions::new()
        .open(endpoint.to_string_lossy().as_ref())?;
    #[cfg(unix)]
    let mut stream = tokio::net::UnixStream::connect(endpoint).await?;
    write_request(&mut stream, &NativeLocalAccessRequest::Status).await?;
    match read_response(&mut stream).await? {
        NativeLocalAccessResponse::Status { status } => Ok(status),
        NativeLocalAccessResponse::Error { message } => bail!(message),
        _ => bail!("unexpected local-access status response"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_protocol_round_trips_without_verified_peer_fields() {
        let request = NativeLocalAccessRequest::Execute(LocalAccessControlRequest {
            request_id: "lock-1".into(),
            action: HostAccessControlAction::LockAll,
            auth_proof: None,
        });
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("lock_all"));
        assert!(!json.contains("protected_executable"));
        assert!(!json.contains("elevated"));
    }

    #[test]
    fn local_access_acl_prefers_session_sid_and_falls_back_to_daemon_user() {
        assert_eq!(
            choose_local_access_user_sid(Some("session".into()), Some("daemon".into())),
            Some("session".into())
        );
        assert_eq!(
            choose_local_access_user_sid(None, Some("daemon".into())),
            Some("daemon".into())
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_current_process_executable_path_resolves() {
        let path = unix_executable_path(std::process::id()).unwrap();

        assert!(path.is_absolute());
        assert!(path.file_name().is_some());
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn portable_user_can_open_local_access_pipe() {
        use tokio::net::windows::named_pipe::ClientOptions;

        let sid = super::super::pipe_security::query_current_process_user_sid().unwrap();
        let sddl = super::super::pipe_security::build_pipe_sddl(Some(&sid));
        let endpoint = format!(
            r"\\.\pipe\lcxl-remote-desk-local-access-control-test-{}",
            uuid::Uuid::new_v4()
        );
        let server = create_windows_pipe(&endpoint, &sddl).unwrap();
        let client_endpoint = endpoint.clone();
        let client =
            tokio::task::spawn_blocking(move || ClientOptions::new().open(client_endpoint));

        server.connect().await.unwrap();
        let client = client.await.unwrap().unwrap();
        drop(client);
    }
}
