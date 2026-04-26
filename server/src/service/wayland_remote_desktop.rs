use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use serde::Deserialize;
use zbus::{
    blocking::Proxy,
    zvariant::{DeserializeDict, OwnedObjectPath, OwnedValue, Type},
};

use crate::error::DeskError;
use desk_capture_engine::image_capture::pipewire_utils::{
    get_zbus_connection, get_zbus_portal_request, wait_zbus_response,
};

#[derive(DeserializeDict, Type, Debug)]
#[zvariant(signature = "dict")]
struct RemoteDesktopCreateSessionResponse {
    session_handle: String,
}

#[derive(DeserializeDict, Type, Debug)]
#[zvariant(signature = "dict")]
struct RemoteDesktopStartStream {
    #[allow(dead_code)]
    id: Option<String>,
}

#[derive(DeserializeDict, Type, Debug)]
#[zvariant(signature = "dict")]
struct RemoteDesktopStartResponse {
    streams: Option<Vec<(u32, RemoteDesktopStartStream)>>,
}

pub struct WaylandRemoteDesktop {
    session: OwnedObjectPath,
    stream_id: u32,
    proxy: Proxy<'static>,
}

static SHARED_REMOTE_DESKTOP: OnceLock<Arc<WaylandRemoteDesktop>> = OnceLock::new();

impl WaylandRemoteDesktop {
    pub fn probe_portal() -> Result<(), DeskError> {
        let conn = get_zbus_connection()?;
        log::info!("Wayland RemoteDesktop: probing portal availability");
        let _proxy = Proxy::new(
            conn,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.RemoteDesktop",
        )?;
        log::info!("Wayland RemoteDesktop: portal is available");
        Ok(())
    }

    pub fn new(types: u32) -> Result<Self, DeskError> {
        let conn = get_zbus_connection()?;
        log::info!("Wayland RemoteDesktop: creating proxy, types={}", types);
        let proxy = Proxy::new(
            conn,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.RemoteDesktop",
        )?;

        let mut create_options = HashMap::new();
        let handle_token = rand::random::<u32>().to_string();
        let session_handle_token = rand::random::<u32>().to_string();
        let create_request = get_zbus_portal_request(conn, &handle_token)?;
        create_options.insert("handle_token", zbus::zvariant::Value::from(&handle_token));
        create_options.insert(
            "session_handle_token",
            zbus::zvariant::Value::from(&session_handle_token),
        );
        log::info!(
            "Wayland RemoteDesktop: calling CreateSession, handle_token={}, session_handle_token={}",
            handle_token,
            session_handle_token
        );
        let create_response_stream = create_request.receive_signal("Response")?;
        proxy.call_method("CreateSession", &(create_options))?;
        let response: RemoteDesktopCreateSessionResponse =
            wait_zbus_response(&create_request, create_response_stream)?;

        let unique_name = conn
            .unique_name()
            .ok_or(DeskError::ZbusError(zbus::Error::Failure(
                "Failed to get dbus unique name".to_owned(),
            )))?;
        let unique_identifier = unique_name.trim_start_matches(':').replace('.', "_");
        let session = OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/session/{unique_identifier}/{session_handle_token}"
        ))?;
        if session.as_str() != response.session_handle {
            log::error!(
                "Wayland RemoteDesktop: session handle mismatch, expected={}, got={}",
                session.as_str(),
                response.session_handle
            );
            return Err(DeskError::ZbusError(zbus::Error::Failure(
                "portal returned mismatched session handle".to_owned(),
            )));
        }
        log::info!(
            "Wayland RemoteDesktop: CreateSession succeeded, session={}",
            session.as_str()
        );

        let mut select_options = HashMap::new();
        let select_token = rand::random::<u32>().to_string();
        let select_request = get_zbus_portal_request(conn, &select_token)?;
        select_options.insert("handle_token", zbus::zvariant::Value::from(&select_token));
        select_options.insert("types", zbus::zvariant::Value::from(types));
        log::info!(
            "Wayland RemoteDesktop: calling SelectDevices, types={}",
            types
        );
        let select_response_stream = select_request.receive_signal("Response")?;
        proxy.call_method("SelectDevices", &(&session, select_options))?;
        let _: HashMap<String, OwnedValue> =
            wait_zbus_response(&select_request, select_response_stream)?;
        log::info!("Wayland RemoteDesktop: SelectDevices succeeded");

        let mut start_options = HashMap::new();
        let start_token = rand::random::<u32>().to_string();
        let start_request = get_zbus_portal_request(conn, &start_token)?;
        start_options.insert("handle_token", zbus::zvariant::Value::from(&start_token));
        log::info!("Wayland RemoteDesktop: calling Start");
        let start_response_stream = start_request.receive_signal("Response")?;
        proxy.call_method("Start", &(&session, "", start_options))?;
        let start_response: RemoteDesktopStartResponse =
            wait_zbus_response(&start_request, start_response_stream)?;
        let stream_id = start_response
            .streams
            .and_then(|v| v.into_iter().next().map(|x| x.0))
            .unwrap_or(0);
        log::info!(
            "Wayland RemoteDesktop: Start succeeded, stream_id={}",
            stream_id
        );

        Ok(Self {
            session,
            stream_id,
            proxy,
        })
    }

    pub fn shared() -> Result<Arc<Self>, DeskError> {
        if let Some(remote) = SHARED_REMOTE_DESKTOP.get() {
            log::debug!("Wayland RemoteDesktop: using existing shared instance");
            return Ok(remote.clone());
        }
        log::info!("Wayland RemoteDesktop: creating shared instance");
        let remote = Arc::new(Self::new(1 | 2)?);
        match SHARED_REMOTE_DESKTOP.set(remote.clone()) {
            Ok(_) => Ok(remote),
            Err(_) => Ok(SHARED_REMOTE_DESKTOP
                .get()
                .expect("shared remote desktop must exist after set failure")
                .clone()),
        }
    }

    pub fn notify_pointer_motion_absolute(&self, x: f64, y: f64) -> Result<(), DeskError> {
        let options: HashMap<String, zbus::zvariant::Value<'_>> = HashMap::new();
        self.proxy.call_method(
            "NotifyPointerMotionAbsolute",
            &(&self.session, options, self.stream_id, x, y),
        )?;
        Ok(())
    }

    pub fn notify_pointer_button(&self, button: u32, state: u32) -> Result<(), DeskError> {
        let options: HashMap<String, zbus::zvariant::Value<'_>> = HashMap::new();
        self.proxy.call_method(
            "NotifyPointerButton",
            &(&self.session, options, button, state),
        )?;
        Ok(())
    }

    pub fn notify_pointer_axis(&self, delta_x: f64, delta_y: f64) -> Result<(), DeskError> {
        let options: HashMap<String, zbus::zvariant::Value<'_>> = HashMap::new();
        self.proxy.call_method(
            "NotifyPointerAxis",
            &(&self.session, options, delta_x, delta_y),
        )?;
        Ok(())
    }

    pub fn notify_keyboard_keycode(&self, keycode: i32, state: u32) -> Result<(), DeskError> {
        let options: HashMap<String, zbus::zvariant::Value<'_>> = HashMap::new();
        self.proxy.call_method(
            "NotifyKeyboardKeycode",
            &(&self.session, options, keycode, state),
        )?;
        Ok(())
    }
}

impl Drop for WaylandRemoteDesktop {
    fn drop(&mut self) {
        log::info!(
            "Wayland RemoteDesktop: closing session={}",
            self.session.as_str()
        );
        let conn = match get_zbus_connection() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("Failed to get dbus connection for close session: {}", e);
                return;
            }
        };
        let session_proxy = Proxy::new(
            conn,
            "org.freedesktop.portal.Desktop",
            self.session.as_str(),
            "org.freedesktop.portal.Session",
        );
        if let Ok(proxy) = session_proxy {
            let _ = proxy.call_method("Close", &());
        }
    }
}
