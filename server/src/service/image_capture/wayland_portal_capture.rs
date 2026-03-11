use std::os::fd::OwnedFd as StdOwnedFd;

use desk_signal_facade::model::{
    desk_settings::DeskSettings,
    image_capture::{DisplayInfo, DisplayRect},
};

use crate::{
    error::DeskError,
    model::image_capture::{ImageCapture, ImageCaptureType, ImageInfo, ImageOutputEnumerator},
    service::image_capture::{
        pipewire_capture::{PipewireImageCapture, PipewireSetup},
        pipewire_utils::get_zbus_connection,
        portal_client::PortalClient,
    },
};
use zbus::blocking::Proxy;

fn close_portal_session(session_path: &str) {
    let Ok(conn) = get_zbus_connection() else {
        return;
    };
    let proxy = Proxy::new(
        conn,
        "org.freedesktop.portal.Desktop",
        session_path,
        "org.freedesktop.portal.Session",
    );
    if let Ok(proxy) = proxy {
        let _ = proxy.call_method("Close", &());
    }
}

pub struct WaylandPortalImageCapture {
    inner: PipewireImageCapture,
}

impl WaylandPortalImageCapture {
    pub fn new(desk_settings: &DeskSettings) -> Result<Self, DeskError> {
        log::info!(
            "Wayland capture: initializing, image_capture={:?}",
            desk_settings.image_capture
        );
        if std::env::var("WAYLAND_DISPLAY").is_err() {
            log::error!("Wayland capture: WAYLAND_DISPLAY is not set");
            return DeskError::custom_error(
                crate::error::DeskErrorCode::SYSTEM_ERROR,
                "WAYLAND_DISPLAY is not set, cannot use wayland portal capture",
            );
        }

        // Fast fail if portal service is unavailable in current user session.
        let portal = PortalClient::new()?;
        let session = portal.create_screencast_session()?;
        portal.select_sources(&session)?;
        let response = portal.start(&session)?;
        let remote_fd: StdOwnedFd = portal.open_pipewire_remote(&session)?.into();
        log::info!(
            "Wayland capture: portal flow completed, session={}",
            session.handle.as_str()
        );
        let selected_stream = response
            .streams
            .and_then(|mut streams| streams.drain(..).next())
            .ok_or(DeskError::ZbusError(zbus::Error::Failure(
                "portal did not return stream".to_owned(),
            )))?;
        log::info!(
            "Wayland capture: selected stream id={}, stream_info={:?}",
            selected_stream.0,
            selected_stream.1
        );

        let (left, top) = selected_stream.1.position.unwrap_or((0, 0));
        let (width, height) = selected_stream.1.size.unwrap_or((0, 0));
        let output = DisplayInfo {
            device_name: selected_stream
                .1
                .id
                .clone()
                .unwrap_or_else(|| "wayland-portal-display".to_string()),
            display_device_name: selected_stream.1.mapping_id.clone(),
            desktop_coordinates: DisplayRect {
                left,
                top,
                right: left + width,
                bottom: top + height,
            },
            attached_to_desktop: true,
            rotation: 0,
            resolutions: vec![],
        };
        let setup = PipewireSetup {
            stream_id: selected_stream.0,
            current_output: Some(output),
            portal_session: Some(session.handle),
            remote_fd: Some(remote_fd),
        };
        let inner = PipewireImageCapture::new_with_setup(desk_settings, setup)?;
        log::info!("Wayland capture: PipeWire capture created successfully");
        Ok(Self { inner })
    }
}

impl ImageCapture for WaylandPortalImageCapture {
    fn capture(&mut self, show_mouse: bool) -> Result<Box<dyn ImageInfo + Send + Sync>, DeskError> {
        self.inner.capture(show_mouse)
    }

    fn get_capture_type(&self) -> ImageCaptureType {
        ImageCaptureType::WAYLANDPORTAL
    }

    fn get_current_output(&self) -> Result<DisplayInfo, DeskError> {
        let size = self.inner.format.as_ref().map(|f| f.size());
        let width = size.map(|s| s.width).unwrap_or(0) as i32;
        let height = size.map(|s| s.height).unwrap_or(0) as i32;
        Ok(DisplayInfo {
            device_name: "wayland-portal-display".to_string(),
            display_device_name: Some("wayland-portal-display".to_string()),
            desktop_coordinates: DisplayRect {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            },
            attached_to_desktop: true,
            rotation: 0,
            resolutions: vec![],
        })
    }
}

pub struct WaylandPortalImageOutputEnumerator;

impl WaylandPortalImageOutputEnumerator {
    pub fn new() -> Self {
        Self
    }
}

impl ImageOutputEnumerator for WaylandPortalImageOutputEnumerator {
    fn get_output_list(&self) -> Result<Vec<DisplayInfo>, DeskError> {
        if std::env::var("WAYLAND_DISPLAY").is_err() {
            return DeskError::custom_error(
                crate::error::DeskErrorCode::SYSTEM_ERROR,
                "WAYLAND_DISPLAY is not set",
            );
        }

        let portal = PortalClient::new()?;
        let session = portal.create_screencast_session()?;
        let session_path = session.handle.as_str().to_string();
        let result = (|| -> Result<Vec<DisplayInfo>, DeskError> {
            portal.select_sources(&session)?;
            let response = portal.start(&session)?;
            let selected_stream = response
                .streams
                .and_then(|mut streams| streams.drain(..).next())
                .ok_or(DeskError::ZbusError(zbus::Error::Failure(
                    "portal did not return stream".to_owned(),
                )))?;

            let (left, top) = selected_stream.1.position.unwrap_or((0, 0));
            let (width, height) = selected_stream.1.size.unwrap_or((0, 0));
            Ok(vec![DisplayInfo {
                device_name: selected_stream
                    .1
                    .id
                    .clone()
                    .unwrap_or_else(|| "wayland-portal-display".to_string()),
                display_device_name: selected_stream.1.mapping_id.clone(),
                desktop_coordinates: DisplayRect {
                    left,
                    top,
                    right: left + width,
                    bottom: top + height,
                },
                attached_to_desktop: true,
                rotation: 0,
                resolutions: vec![],
            }])
        })();
        close_portal_session(&session_path);
        result
    }
}
