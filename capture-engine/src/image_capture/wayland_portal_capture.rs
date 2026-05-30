use std::os::fd::OwnedFd as StdOwnedFd;

use desk_signal_facade::model::{
    desk_settings::DeskSettings,
    image_capture::{DisplayInfo, DisplayRect},
};
use desk_utils::error::DeskErrorCode;

use crate::{
    error::CaptureError,
    image_capture::{
        pipewire_capture::{PipewireImageCapture, PipewireSetup},
        pipewire_utils::get_zbus_connection,
        portal_client::PortalClient,
    },
    model::image_capture::{
        CaptureRequest, CaptureResult, CursorCaptureMode, ImageCapture, ImageCaptureType,
        ImageOutputEnumerator,
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
    pub fn new(desk_settings: &DeskSettings) -> Result<Self, CaptureError> {
        log::info!(
            "Wayland capture: initializing, image_capture={:?}",
            desk_settings.image_capture
        );
        if std::env::var("WAYLAND_DISPLAY").is_err() {
            log::error!("Wayland capture: WAYLAND_DISPLAY is not set");
            return CaptureError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
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
            .ok_or(CaptureError::ZbusError(zbus::Error::Failure(
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
    fn capture(&mut self, request: CaptureRequest) -> Result<CaptureResult, CaptureError> {
        let show_mouse = matches!(request.cursor_mode, CursorCaptureMode::RenderInFrame);
        let image = self.inner.capture(show_mouse)?;
        Ok(CaptureResult {
            image,
            cursor_update: None,
            content_changed: true,
            dirty_rects: None,
        })
    }

    fn get_capture_type(&self) -> ImageCaptureType {
        ImageCaptureType::WAYLANDPORTAL
    }

    fn get_current_output(&self) -> Result<DisplayInfo, CaptureError> {
        let format_size = self
            .inner
            .format
            .as_ref()
            .map(|f| (f.size().width as i32, f.size().height as i32));
        Ok(resolve_current_output(
            self.inner.current_output.as_ref(),
            format_size,
        ))
    }
}

/// Resolve the captured surface's `DisplayInfo`, preferring the real
/// geometry the portal reported for the stream (position + size). The
/// worker anchors per-connection cursor geometry on this surface's
/// **position**, so returning the true coordinates — not a hardcoded
/// `0,0` — is required to address the right monitor on multi-output
/// setups. Falls back to synthesising from the live PipeWire format size
/// only when the portal supplied no stream geometry.
fn resolve_current_output(
    current: Option<&DisplayInfo>,
    format_size: Option<(i32, i32)>,
) -> DisplayInfo {
    if let Some(output) = current {
        return output.clone();
    }
    let (width, height) = format_size.unwrap_or((0, 0));
    DisplayInfo {
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
    }
}

pub struct WaylandPortalImageOutputEnumerator;

impl WaylandPortalImageOutputEnumerator {
    pub fn new() -> Self {
        Self
    }
}

impl ImageOutputEnumerator for WaylandPortalImageOutputEnumerator {
    fn get_output_list(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        if std::env::var("WAYLAND_DISPLAY").is_err() {
            return CaptureError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "WAYLAND_DISPLAY is not set",
            );
        }

        let portal = PortalClient::new()?;
        let session = portal.create_screencast_session()?;
        let session_path = session.handle.as_str().to_string();
        let result = (|| -> Result<Vec<DisplayInfo>, CaptureError> {
            portal.select_sources(&session)?;
            let response = portal.start(&session)?;
            let selected_stream = response
                .streams
                .and_then(|mut streams| streams.drain(..).next())
                .ok_or(CaptureError::ZbusError(zbus::Error::Failure(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_output_preserves_real_portal_position() {
        // A captured second monitor at (1920,0). The anchor must carry
        // the real position, not 0,0.
        let real = DisplayInfo {
            device_name: "42".to_string(),
            display_device_name: Some("DP-2".to_string()),
            desktop_coordinates: DisplayRect {
                left: 1920,
                top: 0,
                right: 1920 + 2560,
                bottom: 1440,
            },
            attached_to_desktop: true,
            rotation: 0,
            resolutions: vec![],
        };
        let out = resolve_current_output(Some(&real), Some((2560, 1440)));
        assert_eq!(out.desktop_coordinates.left, 1920);
        assert_eq!(out.desktop_coordinates.top, 0);
    }

    #[test]
    fn current_output_falls_back_to_format_size_at_origin() {
        let out = resolve_current_output(None, Some((1280, 720)));
        assert_eq!(
            out.desktop_coordinates,
            DisplayRect {
                left: 0,
                top: 0,
                right: 1280,
                bottom: 720
            }
        );
    }
}
