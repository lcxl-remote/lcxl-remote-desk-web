use std::collections::HashMap;
use std::env;
use std::os::fd::OwnedFd as StdOwnedFd;
use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use zbus::zvariant::{DeserializeDict, OwnedFd, OwnedObjectPath, OwnedValue, Type, Value};
use zbus::{Connection, Proxy};

use crate::{
    AuthorizationTarget, DEVICE_TYPE_KEYBOARD, DEVICE_TYPE_POINTER, LivePortalSession,
    PortalAvailability, PortalBackend, PortalError, PortalStream, PreparedPortalSession,
    REQUIRED_INPUT_DEVICE_TYPES, SOURCE_TYPE_MONITOR,
};

const PORTAL_DESTINATION: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const REMOTE_DESKTOP_INTERFACE: &str = "org.freedesktop.portal.RemoteDesktop";
const SCREENCAST_INTERFACE: &str = "org.freedesktop.portal.ScreenCast";
const REGISTRY_INTERFACE: &str = "org.freedesktop.host.portal.Registry";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";
const SESSION_INTERFACE: &str = "org.freedesktop.portal.Session";
const PERSIST_MODE_EXPLICITLY_REVOKED: u32 = 2;

#[derive(DeserializeDict, Type, Debug)]
#[zvariant(signature = "dict")]
struct CreateSessionResponse {
    session_handle: String,
}

#[derive(DeserializeDict, Type, Debug)]
#[zvariant(signature = "dict")]
struct StartStreamInfo {
    id: Option<String>,
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
    mapping_id: Option<String>,
}

#[derive(DeserializeDict, Type, Debug)]
#[zvariant(signature = "dict")]
struct StartResponse {
    devices: Option<u32>,
    streams: Option<Vec<(u32, StartStreamInfo)>>,
    restore_token: Option<String>,
}

pub struct XdgPortalBackend {
    app_id: String,
}

impl XdgPortalBackend {
    pub fn new(app_id: impl Into<String>) -> Self {
        Self {
            app_id: app_id.into(),
        }
    }

    async fn connect_and_probe(&self) -> Result<(Connection, PortalAvailability), PortalError> {
        let connection = Connection::session().await?;
        let stable_app_id = if desktop_entry_is_installed(&self.app_id) {
            register_app_id(&connection, &self.app_id).await
        } else {
            log::info!(
                "No installed {}.desktop entry found; Portal persistence disabled",
                self.app_id
            );
            false
        };
        let remote = portal_proxy(&connection, REMOTE_DESKTOP_INTERFACE).await?;
        let screencast = portal_proxy(&connection, SCREENCAST_INTERFACE).await?;
        let remote_desktop_version = remote.get_property::<u32>("version").await.unwrap_or(0);
        let available_device_types = remote
            .get_property::<u32>("AvailableDeviceTypes")
            .await
            .unwrap_or(0);
        let available_source_types = screencast
            .get_property::<u32>("AvailableSourceTypes")
            .await
            .unwrap_or(0);
        Ok((
            connection,
            PortalAvailability {
                remote_desktop_version,
                available_source_types,
                available_device_types,
                monitor_available: available_source_types & SOURCE_TYPE_MONITOR != 0,
                keyboard_available: available_device_types & DEVICE_TYPE_KEYBOARD != 0,
                pointer_available: available_device_types & DEVICE_TYPE_POINTER != 0,
                stable_app_id,
                persistent_restore: stable_app_id && remote_desktop_version >= 2,
            },
        ))
    }
}

#[async_trait]
impl PortalBackend for XdgPortalBackend {
    async fn probe(&self) -> Result<PortalAvailability, PortalError> {
        self.connect_and_probe()
            .await
            .map(|(_, availability)| availability)
    }

    async fn prepare(
        &self,
        target: AuthorizationTarget,
        restore_token: Option<String>,
        cancel: CancellationToken,
    ) -> Result<PreparedPortalSession, PortalError> {
        let (connection, availability) = self.connect_and_probe().await?;
        if !availability.monitor_available {
            return Err(PortalError::Unsupported(
                "ScreenCast portal does not offer monitor capture".into(),
            ));
        }
        if target.needs_input()
            && availability.available_device_types & REQUIRED_INPUT_DEVICE_TYPES
                != REQUIRED_INPUT_DEVICE_TYPES
        {
            return Err(PortalError::Unsupported(
                "RemoteDesktop portal does not offer keyboard and pointer input".into(),
            ));
        }

        let remote = portal_proxy(&connection, REMOTE_DESKTOP_INTERFACE).await?;
        let screencast = portal_proxy(&connection, SCREENCAST_INTERFACE).await?;

        let session_token = token("session");
        let create_token = token("create");
        let expected_session = session_path(&connection, &session_token)?;
        let mut create_options = HashMap::new();
        create_options.insert("handle_token", Value::from(create_token.as_str()));
        create_options.insert("session_handle_token", Value::from(session_token.as_str()));
        let create: CreateSessionResponse = call_request(
            &connection,
            &remote,
            "CreateSession",
            &(create_options,),
            &create_token,
            &cancel,
        )
        .await?;
        if create.session_handle != expected_session.as_str() {
            return Err(PortalError::InvalidSession(
                "portal returned a mismatched session handle".into(),
            ));
        }
        let closed = session_closed_token(&connection, &expected_session).await?;

        if target.needs_input() {
            let select_token = token("devices");
            let mut options = HashMap::new();
            options.insert("handle_token", Value::from(select_token.as_str()));
            options.insert("types", Value::from(REQUIRED_INPUT_DEVICE_TYPES));
            if availability.persistent_restore {
                options.insert("persist_mode", Value::from(PERSIST_MODE_EXPLICITLY_REVOKED));
                if let Some(token) = restore_token.as_deref() {
                    options.insert("restore_token", Value::from(token));
                }
            }
            let _: HashMap<String, OwnedValue> = call_request(
                &connection,
                &remote,
                "SelectDevices",
                &(&expected_session, options),
                &select_token,
                &cancel,
            )
            .await?;
        }

        let source_token = token("sources");
        let mut source_options = HashMap::new();
        source_options.insert("handle_token", Value::from(source_token.as_str()));
        source_options.insert("types", Value::from(SOURCE_TYPE_MONITOR));
        source_options.insert("multiple", Value::from(false));
        let _: HashMap<String, OwnedValue> = call_request(
            &connection,
            &screencast,
            "SelectSources",
            &(&expected_session, source_options),
            &source_token,
            &cancel,
        )
        .await?;

        let start_token = token("start");
        let mut start_options = HashMap::new();
        start_options.insert("handle_token", Value::from(start_token.as_str()));
        let response: StartResponse = call_request(
            &connection,
            &remote,
            "Start",
            &(&expected_session, "", start_options),
            &start_token,
            &cancel,
        )
        .await?;

        let mut streams = response.streams.unwrap_or_default();
        if streams.len() != 1 {
            close_session(&connection, &expected_session).await;
            return Err(PortalError::InvalidSession(format!(
                "portal returned {} streams; exactly one is required",
                streams.len()
            )));
        }
        let (node_id, stream_info) = streams.remove(0);
        if node_id == 0 {
            close_session(&connection, &expected_session).await;
            return Err(PortalError::InvalidSession(
                "portal returned an invalid zero PipeWire node id".into(),
            ));
        }
        let selected_device_types = response.devices.unwrap_or(0);
        if target.needs_input()
            && selected_device_types & REQUIRED_INPUT_DEVICE_TYPES != REQUIRED_INPUT_DEVICE_TYPES
        {
            close_session(&connection, &expected_session).await;
            return Err(PortalError::InputDevicesNotGranted);
        }

        let fd_options: HashMap<&str, Value<'_>> = HashMap::new();
        let remote_fd: OwnedFd = screencast
            .call("OpenPipeWireRemote", &(&expected_session, fd_options))
            .await?;
        let remote_fd: StdOwnedFd = remote_fd.into();
        let session = XdgLiveSession {
            connection,
            handle: expected_session,
            target,
            stream: PortalStream {
                node_id,
                id: stream_info.id,
                position: stream_info.position,
                size: stream_info.size,
                mapping_id: stream_info.mapping_id,
            },
            closed,
            remote_fd: Mutex::new(remote_fd),
        };
        Ok(PreparedPortalSession {
            session: std::sync::Arc::new(session),
            selected_device_types,
            restore_token: response.restore_token,
        })
    }
}

struct XdgLiveSession {
    connection: Connection,
    handle: OwnedObjectPath,
    target: AuthorizationTarget,
    stream: PortalStream,
    remote_fd: Mutex<StdOwnedFd>,
    closed: CancellationToken,
}

#[async_trait]
impl LivePortalSession for XdgLiveSession {
    fn target(&self) -> AuthorizationTarget {
        self.target
    }

    fn stream(&self) -> &PortalStream {
        &self.stream
    }

    fn closure_token(&self) -> CancellationToken {
        self.closed.clone()
    }

    fn duplicate_pipewire_fd(&self) -> Result<StdOwnedFd, PortalError> {
        Ok(self
            .remote_fd
            .lock()
            .map_err(|_| PortalError::Backend("PipeWire fd lock poisoned".into()))?
            .try_clone()?)
    }

    async fn notify_pointer_motion_absolute(&self, x: f64, y: f64) -> Result<(), PortalError> {
        let options: HashMap<&str, Value<'_>> = HashMap::new();
        self.remote_proxy()
            .await?
            .call_method(
                "NotifyPointerMotionAbsolute",
                &(&self.handle, options, self.stream.node_id, x, y),
            )
            .await?;
        Ok(())
    }

    async fn notify_pointer_button(&self, button: u32, state: u32) -> Result<(), PortalError> {
        let options: HashMap<&str, Value<'_>> = HashMap::new();
        self.remote_proxy()
            .await?
            .call_method(
                "NotifyPointerButton",
                &(&self.handle, options, button, state),
            )
            .await?;
        Ok(())
    }

    async fn notify_pointer_axis(&self, delta_x: f64, delta_y: f64) -> Result<(), PortalError> {
        let options: HashMap<&str, Value<'_>> = HashMap::new();
        self.remote_proxy()
            .await?
            .call_method(
                "NotifyPointerAxis",
                &(&self.handle, options, delta_x, delta_y),
            )
            .await?;
        Ok(())
    }

    async fn notify_keyboard_keycode(&self, keycode: i32, state: u32) -> Result<(), PortalError> {
        let options: HashMap<&str, Value<'_>> = HashMap::new();
        self.remote_proxy()
            .await?
            .call_method(
                "NotifyKeyboardKeycode",
                &(&self.handle, options, keycode, state),
            )
            .await?;
        Ok(())
    }

    async fn close(&self) -> Result<(), PortalError> {
        self.closed.cancel();
        close_session(&self.connection, &self.handle).await;
        Ok(())
    }
}

impl XdgLiveSession {
    async fn remote_proxy(&self) -> Result<Proxy<'_>, PortalError> {
        Ok(portal_proxy(&self.connection, REMOTE_DESKTOP_INTERFACE).await?)
    }
}

async fn register_app_id(connection: &Connection, app_id: &str) -> bool {
    let Ok(proxy) = portal_proxy(connection, REGISTRY_INTERFACE).await else {
        return false;
    };
    if proxy.get_property::<u32>("version").await.is_err() {
        return false;
    }
    let options: HashMap<&str, Value<'_>> = HashMap::new();
    match proxy.call_method("Register", &(app_id, options)).await {
        Ok(_) => true,
        Err(error) => {
            log::info!(
                "Portal host app-id registration unavailable; persistence disabled: {error}"
            );
            false
        }
    }
}

fn desktop_entry_is_installed(app_id: &str) -> bool {
    desktop_entry_exists_in(app_id, &xdg_data_roots())
}

fn desktop_entry_exists_in(app_id: &str, roots: &[PathBuf]) -> bool {
    let desktop_file = format!("{app_id}.desktop");
    roots
        .iter()
        .any(|root| root.join("applications").join(&desktop_file).is_file())
}

fn xdg_data_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let data_home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".local/share"))
        });
    if let Some(data_home) = data_home {
        roots.push(data_home);
    }

    let data_dirs = env::var_os("XDG_DATA_DIRS")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    roots.extend(env::split_paths(&data_dirs).filter(|path| path.is_absolute()));
    roots
}

async fn portal_proxy<'a>(
    connection: &'a Connection,
    interface: &'static str,
) -> Result<Proxy<'a>, zbus::Error> {
    Proxy::new(connection, PORTAL_DESTINATION, PORTAL_PATH, interface).await
}

async fn call_request<T, B>(
    connection: &Connection,
    portal: &Proxy<'_>,
    method: &str,
    body: &B,
    handle_token: &str,
    cancel: &CancellationToken,
) -> Result<T, PortalError>
where
    T: for<'de> Deserialize<'de> + Type,
    B: serde::ser::Serialize + Type,
{
    let request_path = request_path(connection, handle_token)?;
    let request = Proxy::new(
        connection,
        PORTAL_DESTINATION,
        request_path,
        REQUEST_INTERFACE,
    )
    .await?;
    let mut responses = request.receive_signal("Response").await?;
    portal.call_method(method, body).await?;
    let message = tokio::select! {
        _ = cancel.cancelled() => {
            let _ = request.call_method("Close", &()).await;
            return Err(PortalError::Cancelled);
        }
        message = responses.next() => message.ok_or_else(|| {
            PortalError::Backend("Portal request ended without a Response signal".into())
        })?,
    };
    let (code, result): (u32, T) = message.body().deserialize()?;
    match code {
        0 => Ok(result),
        1 => Err(PortalError::Cancelled),
        other => Err(PortalError::Backend(format!(
            "Portal request failed with response code {other}"
        ))),
    }
}

fn request_path(
    connection: &Connection,
    handle_token: &str,
) -> Result<OwnedObjectPath, PortalError> {
    peer_path(connection, "request", handle_token)
}

fn session_path(
    connection: &Connection,
    session_token: &str,
) -> Result<OwnedObjectPath, PortalError> {
    peer_path(connection, "session", session_token)
}

fn peer_path(
    connection: &Connection,
    kind: &str,
    token: &str,
) -> Result<OwnedObjectPath, PortalError> {
    let unique_name = connection
        .unique_name()
        .ok_or_else(|| PortalError::Backend("D-Bus connection has no unique name".into()))?;
    let peer = unique_name.trim_start_matches(':').replace('.', "_");
    Ok(OwnedObjectPath::try_from(format!(
        "/org/freedesktop/portal/desktop/{kind}/{peer}/{token}"
    ))?)
}

fn token(prefix: &str) -> String {
    format!("lrd_{prefix}_{}", rand::random::<u64>())
}

async fn session_closed_token(
    connection: &Connection,
    session: &OwnedObjectPath,
) -> Result<CancellationToken, PortalError> {
    let closed = CancellationToken::new();
    let signal = closed.clone();
    let connection = connection.clone();
    let session = session.clone();
    let proxy = Proxy::new(&connection, PORTAL_DESTINATION, session, SESSION_INTERFACE).await?;
    let mut signals = proxy.receive_signal("Closed").await?;
    tokio::spawn(async move {
        let _keep_connection_alive = connection;
        let _ = signals.next().await;
        signal.cancel();
    });
    Ok(closed)
}

async fn close_session(connection: &Connection, session: &OwnedObjectPath) {
    if let Ok(proxy) = Proxy::new(
        connection,
        PORTAL_DESTINATION,
        session.clone(),
        SESSION_INTERFACE,
    )
    .await
    {
        let _ = proxy.call_method("Close", &()).await;
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn stable_app_id_requires_matching_installed_desktop_entry() {
        let temp = tempfile::tempdir().expect("tempdir");
        let applications = temp.path().join("applications");
        fs::create_dir_all(&applications).expect("create applications");
        fs::write(
            applications.join("com.lcxl.remote-desk.desktop"),
            "[Desktop Entry]\nName=Lcxl Remote Desk\n",
        )
        .expect("write desktop entry");

        let roots = [temp.path().to_path_buf()];
        assert!(desktop_entry_exists_in("com.lcxl.remote-desk", &roots));
        assert!(!desktop_entry_exists_in("com.example.portable", &roots));
    }
}
