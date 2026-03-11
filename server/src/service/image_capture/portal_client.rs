use std::collections::HashMap;

use serde::Deserialize;
use zbus::{
    blocking::Proxy,
    zvariant::{DeserializeDict, OwnedFd, OwnedObjectPath, Type},
};

use crate::{
    error::DeskError,
    service::image_capture::pipewire_utils::{
        get_zbus_connection, get_zbus_portal_request, wait_zbus_response,
    },
};

#[derive(Debug, Clone)]
pub struct PortalSession {
    pub handle: OwnedObjectPath,
}

#[derive(DeserializeDict, Type, Debug)]
#[zvariant(signature = "dict")]
pub struct ScreenCastCreateSessionResponse {
    pub session_handle: String,
}

#[derive(DeserializeDict, Type, Debug, Clone)]
#[zvariant(signature = "dict")]
pub struct PortalStreamInfo {
    pub id: Option<String>,
    pub position: Option<(i32, i32)>,
    pub size: Option<(i32, i32)>,
    pub source_type: Option<u32>,
    pub mapping_id: Option<String>,
}

#[derive(DeserializeDict, Type, Debug)]
#[zvariant(signature = "dict")]
pub struct ScreenCastStartResponse {
    pub streams: Option<Vec<(u32, PortalStreamInfo)>>,
    pub restore_token: Option<String>,
}

pub struct PortalClient<'a> {
    proxy: Proxy<'a>,
}

impl PortalClient<'_> {
    pub fn new() -> Result<Self, DeskError> {
        let conn = get_zbus_connection()?;
        log::info!("Wayland portal: creating ScreenCast proxy");
        let proxy = Proxy::new(
            conn,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.ScreenCast",
        )?;
        log::info!("Wayland portal: ScreenCast proxy is ready");
        Ok(Self { proxy })
    }

    pub fn create_screencast_session(&self) -> Result<PortalSession, DeskError> {
        let conn = get_zbus_connection()?;
        let mut options = HashMap::new();

        let handle_token = rand::random::<u32>().to_string();
        let session_handle_token = rand::random::<u32>().to_string();

        let request = get_zbus_portal_request(conn, &handle_token)?;
        options.insert("handle_token", zbus::zvariant::Value::from(&handle_token));
        options.insert(
            "session_handle_token",
            zbus::zvariant::Value::from(&session_handle_token),
        );

        log::info!(
            "Wayland portal: calling CreateSession, handle_token={}, session_handle_token={}",
            handle_token,
            session_handle_token
        );
        let response_stream = request.receive_signal("Response")?;
        self.proxy.call_method("CreateSession", &(options))?;
        let response: ScreenCastCreateSessionResponse = wait_zbus_response(&request, response_stream)?;

        let unique_name = conn
            .unique_name()
            .ok_or(DeskError::ZbusError(zbus::Error::Failure(
                "Failed to get dbus unique name".to_owned(),
            )))?;
        let unique_identifier = unique_name.trim_start_matches(':').replace('.', "_");
        let expected = OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/session/{unique_identifier}/{session_handle_token}"
        ))?;

        if expected.as_str() != response.session_handle {
            log::error!(
                "Wayland portal: session handle mismatch, expected={}, got={}",
                expected.as_str(),
                response.session_handle
            );
            return Err(DeskError::ZbusError(zbus::Error::Failure(
                "portal returned mismatched session handle".to_owned(),
            )));
        }

        log::info!(
            "Wayland portal: CreateSession succeeded, session={}",
            expected.as_str()
        );
        Ok(PortalSession { handle: expected })
    }

    pub fn select_sources(&self, session: &PortalSession) -> Result<(), DeskError> {
        let conn = get_zbus_connection()?;
        let mut options = HashMap::new();
        let handle_token = rand::random::<u32>().to_string();
        let request = get_zbus_portal_request(conn, &handle_token)?;

        options.insert("handle_token", zbus::zvariant::Value::from(handle_token));
        options.insert("types", zbus::zvariant::Value::from(1_u32));
        options.insert("multiple", zbus::zvariant::Value::from(false));

        log::info!(
            "Wayland portal: calling SelectSources, session={}",
            session.handle.as_str()
        );
        let response_stream = request.receive_signal("Response")?;
        self.proxy
            .call_method("SelectSources", &(&session.handle, options))?;

        let _: HashMap<String, zbus::zvariant::OwnedValue> =
            wait_zbus_response(&request, response_stream)?;
        log::info!("Wayland portal: SelectSources succeeded");
        Ok(())
    }

    pub fn start(&self, session: &PortalSession) -> Result<ScreenCastStartResponse, DeskError> {
        let conn = get_zbus_connection()?;
        let mut options = HashMap::new();
        let handle_token = rand::random::<u32>().to_string();
        let request = get_zbus_portal_request(conn, &handle_token)?;
        options.insert("handle_token", zbus::zvariant::Value::from(&handle_token));

        log::info!(
            "Wayland portal: calling Start, session={}",
            session.handle.as_str()
        );
        let response_stream = request.receive_signal("Response")?;
        self.proxy
            .call_method("Start", &(&session.handle, "", options))?;
        let response = wait_zbus_response(&request, response_stream)?;
        log::info!("Wayland portal: Start succeeded");
        Ok(response)
    }

    pub fn open_pipewire_remote(&self, session: &PortalSession) -> Result<OwnedFd, DeskError> {
        let options: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
        log::info!(
            "Wayland portal: calling OpenPipeWireRemote, session={}",
            session.handle.as_str()
        );
        let fd: OwnedFd = self
            .proxy
            .call("OpenPipeWireRemote", &(&session.handle, options))?;
        log::info!("Wayland portal: OpenPipeWireRemote succeeded");
        Ok(fd)
    }
}
