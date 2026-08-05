use std::os::fd::OwnedFd as StdOwnedFd;

use desk_signal_facade::model::{
    desk_settings::DeskSettings,
    image_capture::{DisplayInfo, DisplayRect},
};
use desk_utils::{
    error::DeskErrorCode,
    linux_display::{LinuxDisplayServer, detect_linux_display_environment},
};

use crate::{
    error::CaptureError,
    image_capture::{
        pipewire_capture::{PipewireImageCapture, PipewireSetup},
        portal_client::{PortalClient, probe_screencast_monitor_blocking},
        wayland_output_geometry::{WaylandOutputGeometry, enumerate_wayland_outputs},
    },
    model::image_capture::{
        CaptureRequest, CaptureResult, CursorCaptureMode, ImageCapture, ImageCaptureType,
        ImageOutputEnumerator,
    },
};

pub struct WaylandPortalImageCapture {
    inner: PipewireImageCapture,
}

impl WaylandPortalImageCapture {
    pub fn new(desk_settings: &DeskSettings) -> Result<Self, CaptureError> {
        log::info!(
            "Wayland capture: initializing, image_capture={:?}",
            desk_settings.image_capture
        );
        if detect_linux_display_environment().active_server() != LinuxDisplayServer::Wayland {
            log::error!("Wayland capture: WAYLAND_DISPLAY is not set");
            return CaptureError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "WAYLAND_DISPLAY is not set, cannot use wayland portal capture",
            );
        }

        // Verify monitor capture without creating a portal session or showing consent UI.
        probe_screencast_monitor_blocking()?;

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
/// setups. The live PipeWire format size backfills the dimensions when
/// the portal supplied no stream geometry at all, or supplied a
/// degenerate zero-area rect (size omitted) while still reporting a
/// position.
fn resolve_current_output(
    current: Option<&DisplayInfo>,
    format_size: Option<(i32, i32)>,
) -> DisplayInfo {
    if let Some(output) = current {
        // Portal reported a position but a zero/absent size: keep its
        // position and identity, fill the dimensions from the negotiated
        // PipeWire format so the anchor rect is usable.
        let coords = output.desktop_coordinates;
        if (coords.width() <= 0 || coords.height() <= 0)
            && let Some((w, h)) = format_size
            && w > 0
            && h > 0
        {
            let mut fixed = output.clone();
            fixed.desktop_coordinates.right = coords.left + w;
            fixed.desktop_coordinates.bottom = coords.top + h;
            return fixed;
        }
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

/// Stable device name advertised for the Wayland portal capture source.
///
/// The portal's own screen-cast picker is the authority over which monitor
/// gets captured, so the enumerator exposes a single backend-level entry
/// rather than per-output devices the UI could "pre-select" out of sync with
/// the picker. A constant name keeps the input side's non-interactive
/// re-enumeration matching the same entry (and thus the same geometry) across
/// calls.
const WAYLAND_PORTAL_DEVICE_NAME: &str = "wayland-portal-display";

impl ImageOutputEnumerator for WaylandPortalImageOutputEnumerator {
    fn get_output_list(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        if detect_linux_display_environment().active_server() != LinuxDisplayServer::Wayland {
            return CaptureError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "WAYLAND_DISPLAY is not set",
            );
        }
        probe_screencast_monitor_blocking()?;

        // Enumerate outputs without the portal. The screen-cast `Start`
        // handshake pops an interactive "Share Screen" picker and blocks on
        // the user's choice, which must never happen during capability
        // listing (it would surface at worker startup, before any peer
        // connects). The real portal consent is requested later, only when a
        // peer actually begins capture — see `WaylandPortalImageCapture::new`.
        Ok(portal_outputs_to_display_info(enumerate_wayland_outputs()))
    }
}

/// Collapse the non-interactive Wayland output enumeration into the single
/// backend-level [`DisplayInfo`] advertised for portal capture.
///
/// Always returns exactly one entry so the UI's capture-source list is never
/// empty. Its geometry is the "primary" output (see [`select_primary_output`]).
/// An empty result or an enumeration error yields a zero-geometry placeholder;
/// errors are logged rather than silently swallowed.
fn portal_outputs_to_display_info(
    result: Result<Vec<WaylandOutputGeometry>, CaptureError>,
) -> Vec<DisplayInfo> {
    let primary = match result {
        Ok(outputs) => select_primary_output(outputs),
        Err(e) => {
            log::warn!(
                "Wayland portal enumeration failed: {e}; advertising placeholder capture source"
            );
            None
        }
    };
    let (desktop_coordinates, display_device_name) = match primary {
        Some(geo) => (geo.logical, geo.name),
        None => (
            DisplayRect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            None,
        ),
    };
    vec![DisplayInfo {
        device_name: WAYLAND_PORTAL_DEVICE_NAME.to_string(),
        display_device_name,
        desktop_coordinates,
        attached_to_desktop: true,
        rotation: 0,
        resolutions: vec![],
    }]
}

/// Pick the primary output deterministically: prefer the output anchored at
/// the compositor origin (0,0); otherwise the smallest by `(left, top, name)`.
/// The explicit ordering matters because `enumerate_wayland_outputs` draws
/// from an unordered map, so "the first one" would be non-deterministic.
fn select_primary_output(outputs: Vec<WaylandOutputGeometry>) -> Option<WaylandOutputGeometry> {
    outputs.into_iter().min_by(|a, b| {
        // `false` sorts before `true`, so origin-anchored outputs win.
        let key = |g: &WaylandOutputGeometry| {
            let not_at_origin = !(g.logical.left == 0 && g.logical.top == 0);
            (not_at_origin, g.logical.left, g.logical.top, g.name.clone())
        };
        key(a).cmp(&key(b))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geo(
        name: Option<&str>,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    ) -> WaylandOutputGeometry {
        WaylandOutputGeometry {
            name: name.map(str::to_string),
            logical: DisplayRect {
                left,
                top,
                right,
                bottom,
            },
        }
    }

    #[test]
    fn single_output_uses_real_geometry() {
        let infos = portal_outputs_to_display_info(Ok(vec![geo(Some("DP-1"), 0, 0, 1280, 800)]));
        assert_eq!(infos.len(), 1);
        let d = &infos[0];
        assert_eq!(d.device_name, WAYLAND_PORTAL_DEVICE_NAME);
        assert_eq!(d.display_device_name.as_deref(), Some("DP-1"));
        assert_eq!(
            d.desktop_coordinates,
            DisplayRect {
                left: 0,
                top: 0,
                right: 1280,
                bottom: 800
            }
        );
    }

    #[test]
    fn multiple_outputs_collapse_to_single_primary() {
        // DP-2 is listed first, but DP-1 sits at the origin -> DP-1 wins, and
        // the result is still a single entry (no per-output devices).
        let infos = portal_outputs_to_display_info(Ok(vec![
            geo(Some("DP-2"), 1280, 0, 1280 + 1920, 1080),
            geo(Some("DP-1"), 0, 0, 1280, 800),
        ]));
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].display_device_name.as_deref(), Some("DP-1"));
        assert_eq!(infos[0].desktop_coordinates.left, 0);
    }

    #[test]
    fn non_origin_outputs_pick_deterministic_minimum() {
        // No output at the origin: the smallest (left, top, name) wins
        // regardless of enumeration order.
        let forward = portal_outputs_to_display_info(Ok(vec![
            geo(Some("B"), 100, 0, 1000, 800),
            geo(Some("A"), 100, 0, 1000, 800),
        ]));
        let reversed = portal_outputs_to_display_info(Ok(vec![
            geo(Some("A"), 100, 0, 1000, 800),
            geo(Some("B"), 100, 0, 1000, 800),
        ]));
        assert_eq!(forward[0].display_device_name.as_deref(), Some("A"));
        assert_eq!(reversed[0].display_device_name.as_deref(), Some("A"));
    }

    #[test]
    fn empty_ok_yields_zero_geometry_placeholder() {
        let infos = portal_outputs_to_display_info(Ok(vec![]));
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].device_name, WAYLAND_PORTAL_DEVICE_NAME);
        assert!(infos[0].display_device_name.is_none());
        assert_eq!(
            infos[0].desktop_coordinates,
            DisplayRect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0
            }
        );
    }

    #[test]
    fn error_yields_placeholder() {
        let err = CaptureError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, "boom");
        let infos = portal_outputs_to_display_info(Err(err));
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].device_name, WAYLAND_PORTAL_DEVICE_NAME);
        assert_eq!(
            infos[0].desktop_coordinates,
            DisplayRect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0
            }
        );
    }

    #[test]
    fn device_name_is_stable_constant() {
        let cases = [
            portal_outputs_to_display_info(Ok(vec![geo(Some("DP-1"), 0, 0, 1280, 800)])),
            portal_outputs_to_display_info(Ok(vec![])),
            portal_outputs_to_display_info(Err(CaptureError::new_custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "x",
            ))),
        ];
        for infos in cases {
            assert_eq!(infos[0].device_name, WAYLAND_PORTAL_DEVICE_NAME);
        }
    }

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

    #[test]
    fn current_output_fills_zero_portal_size_from_format() {
        // Portal reported a position (100,50) but omitted the size,
        // leaving a degenerate rect. The format size backfills the
        // dimensions while position and identity are preserved.
        let zero_size = DisplayInfo {
            device_name: "7".to_string(),
            display_device_name: Some("DP-1".to_string()),
            desktop_coordinates: DisplayRect {
                left: 100,
                top: 50,
                right: 100,
                bottom: 50,
            },
            attached_to_desktop: true,
            rotation: 0,
            resolutions: vec![],
        };
        let out = resolve_current_output(Some(&zero_size), Some((1920, 1080)));
        assert_eq!(
            out.desktop_coordinates,
            DisplayRect {
                left: 100,
                top: 50,
                right: 100 + 1920,
                bottom: 50 + 1080,
            },
            "size filled from format, position preserved"
        );
        assert_eq!(out.device_name, "7", "portal identity preserved");
    }
}
