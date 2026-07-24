//! Non-interactive Wayland output geometry enumeration.
//!
//! Reads the logical position / size of every `wl_output` via the core
//! `wl_registry` + `zxdg_output_manager_v1` (xdg-output) protocols —
//! **without** going through the desktop portal, so it never raises an
//! interactive screen-share picker. This is the geometry source the
//! worker uses to refresh per-connection cursor geometry on a Wayland
//! display change (the portal `get_output_list` path would pop a picker
//! on every call).
//!
//! `logical_position` / `logical_size` (compositor-space coordinates)
//! are exactly what the portal stream reports for a captured monitor, so
//! a captured surface can be matched back to its current geometry by
//! position via [`match_output_by_anchor`].
//!
//! ## Degradation
//!
//! Everything here is best-effort: if the compositor does not expose
//! `zxdg_output_manager_v1`, or an output never reports a usable
//! `logical_size`, that output is simply absent from the result and the
//! caller keeps the previous geometry. Nothing here ever fabricates a
//! zero-sized rectangle.

use std::collections::HashMap;

use desk_signal_facade::model::image_capture::DisplayRect;
use desk_utils::error::DeskErrorCode;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};

use crate::error::CaptureError;

/// Highest versions this enumerator binds.
const MAX_WL_OUTPUT_VERSION: u32 = 4;
const MAX_XDG_OUTPUT_MANAGER_VERSION: u32 = 3;

/// One output's logical geometry in compositor space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaylandOutputGeometry {
    /// Connector-style name (e.g. "DP-1") when the compositor reports it.
    pub name: Option<String>,
    /// Logical rectangle (position + size) in the global compositor
    /// coordinate space.
    pub logical: DisplayRect,
}

/// Find the output whose logical top-left exactly equals `anchor`'s.
///
/// Position is used (not size) because a mode change keeps an output at
/// the same top-left while its size changes — which is precisely the
/// geometry we want to refresh. Matching is **exact only**: if the
/// anchored output is gone (e.g. the captured external monitor was
/// unplugged) this returns `None` so the caller keeps the old geometry,
/// rather than silently re-pointing the connection at a different
/// monitor. Mirrored clones sharing a top-left resolve to the first.
pub fn match_output_by_anchor(
    outputs: &[WaylandOutputGeometry],
    anchor: DisplayRect,
) -> Option<&WaylandOutputGeometry> {
    outputs
        .iter()
        .find(|o| o.logical.left == anchor.left && o.logical.top == anchor.top)
}

/// Build a logical rectangle from the xdg-output position / size, or
/// `None` when the size is missing or degenerate. Pure so the
/// degradation policy is unit-testable.
fn build_geometry(pos: Option<(i32, i32)>, size: Option<(i32, i32)>) -> Option<DisplayRect> {
    match (pos, size) {
        (Some((x, y)), Some((w, h))) if w > 0 && h > 0 => Some(DisplayRect {
            left: x,
            top: y,
            right: x + w,
            bottom: y + h,
        }),
        _ => None,
    }
}

struct PartialOutput {
    wl_output: WlOutput,
    name: Option<String>,
    logical_pos: Option<(i32, i32)>,
    logical_size: Option<(i32, i32)>,
}

struct EnumState {
    manager: Option<ZxdgOutputManagerV1>,
    /// Keyed by registry global name.
    outputs: HashMap<u32, PartialOutput>,
}

impl Dispatch<WlRegistry, ()> for EnumState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_output" => {
                    let v = version.min(MAX_WL_OUTPUT_VERSION);
                    let wl_output: WlOutput = registry.bind(name, v, qh, name);
                    state.outputs.insert(
                        name,
                        PartialOutput {
                            wl_output,
                            name: None,
                            logical_pos: None,
                            logical_size: None,
                        },
                    );
                }
                "zxdg_output_manager_v1" => {
                    let v = version.min(MAX_XDG_OUTPUT_MANAGER_VERSION);
                    state.manager = Some(registry.bind(name, v, qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlOutput, u32> for EnumState {
    fn event(
        _state: &mut Self,
        _output: &WlOutput,
        _event: wl_output::Event,
        _name: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Logical geometry comes from xdg-output; the core wl_output
        // events are not needed here.
    }
}

impl Dispatch<ZxdgOutputManagerV1, ()> for EnumState {
    fn event(
        _state: &mut Self,
        _manager: &ZxdgOutputManagerV1,
        _event: <ZxdgOutputManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // The manager has no events.
    }
}

impl Dispatch<ZxdgOutputV1, u32> for EnumState {
    fn event(
        state: &mut Self,
        _xdg: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        name: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(out) = state.outputs.get_mut(name) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                out.logical_pos = Some((x, y));
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                out.logical_size = Some((width, height));
            }
            zxdg_output_v1::Event::Name { name } => {
                out.name = Some(name);
            }
            _ => {}
        }
    }
}

fn map_err(context: &str, e: impl std::fmt::Display) -> CaptureError {
    CaptureError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &format!("{context}: {e}"))
}

/// Enumerate the logical geometry of every connected output, without the
/// portal. Returns an empty list (not an error) when the compositor does
/// not provide `zxdg_output_manager_v1`; outputs that never report a
/// usable `logical_size` are skipped.
pub fn enumerate_wayland_outputs() -> Result<Vec<WaylandOutputGeometry>, CaptureError> {
    let conn = Connection::connect_to_env().map_err(|e| map_err("wayland connect", e))?;
    let mut queue = conn.new_event_queue::<EnumState>();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    let mut state = EnumState {
        manager: None,
        outputs: HashMap::new(),
    };

    // First dispatch: discover the output globals and the xdg-output manager.
    queue
        .roundtrip(&mut state)
        .map_err(|e| map_err("wayland registry roundtrip", e))?;

    let Some(manager) = state.manager.take() else {
        log::debug!(
            "wayland output geometry: zxdg_output_manager_v1 unavailable; keeping prior geometry"
        );
        return Ok(Vec::new());
    };

    // Request the logical geometry for each output.
    let names: Vec<u32> = state.outputs.keys().copied().collect();
    let xdg_outputs: Vec<ZxdgOutputV1> = names
        .iter()
        .map(|&name| manager.get_xdg_output(&state.outputs[&name].wl_output, &qh, name))
        .collect();

    // Second dispatch: receive logical_position / logical_size / name.
    queue
        .roundtrip(&mut state)
        .map_err(|e| map_err("wayland xdg-output roundtrip", e))?;

    for x in xdg_outputs {
        x.destroy();
    }
    manager.destroy();

    let mut result = Vec::new();
    for out in state.outputs.values() {
        match build_geometry(out.logical_pos, out.logical_size) {
            Some(logical) => result.push(WaylandOutputGeometry {
                name: out.name.clone(),
                logical,
            }),
            None => log::debug!(
                "wayland output geometry: output {:?} has no usable logical size; skipping",
                out.name
            ),
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geo(name: &str, left: i32, top: i32, w: i32, h: i32) -> WaylandOutputGeometry {
        WaylandOutputGeometry {
            name: Some(name.to_string()),
            logical: DisplayRect {
                left,
                top,
                right: left + w,
                bottom: top + h,
            },
        }
    }

    #[test]
    fn match_anchor_hits_exact_position() {
        let outputs = vec![
            geo("DP-1", 0, 0, 1920, 1080),
            geo("DP-2", 1920, 0, 2560, 1440),
        ];
        let anchor = DisplayRect {
            left: 1920,
            top: 0,
            right: 1920 + 2560,
            bottom: 1440,
        };
        let m = match_output_by_anchor(&outputs, anchor).expect("DP-2 by position");
        assert_eq!(m.name.as_deref(), Some("DP-2"));
    }

    #[test]
    fn match_anchor_tracks_position_across_resolution_change() {
        // The captured output stayed at (1920,0) but switched to 1080p.
        // The anchor (recorded at capture start, 1440p) still resolves by
        // position, picking up the new size.
        let outputs = vec![geo("DP-2", 1920, 0, 1920, 1080)];
        let anchor_at_capture = DisplayRect {
            left: 1920,
            top: 0,
            right: 1920 + 2560,
            bottom: 1440,
        };
        let m = match_output_by_anchor(&outputs, anchor_at_capture).expect("matched by position");
        assert_eq!(m.logical.width(), 1920, "picks up the new (smaller) size");
    }

    #[test]
    fn match_anchor_returns_none_without_exact_position() {
        // No output at the anchor's top-left -> None (never a nearest
        // fallback that could re-point to another monitor).
        let outputs = vec![geo("DP-1", 0, 0, 1920, 1080)];
        let anchor = DisplayRect {
            left: 1920,
            top: 0,
            right: 1920 + 2560,
            bottom: 1440,
        };
        assert!(match_output_by_anchor(&outputs, anchor).is_none());
    }

    #[test]
    fn match_anchor_returns_none_when_captured_output_removed() {
        // The captured external monitor was unplugged; only the primary
        // remains. Must not match the primary.
        let outputs = vec![geo("DP-1", 0, 0, 1920, 1080)];
        let anchor_of_unplugged = DisplayRect {
            left: 1920,
            top: 0,
            right: 1920 + 1920,
            bottom: 1080,
        };
        assert!(match_output_by_anchor(&outputs, anchor_of_unplugged).is_none());
    }

    #[test]
    fn build_geometry_requires_position_and_nonzero_size() {
        assert_eq!(
            build_geometry(Some((10, 20)), Some((800, 600))),
            Some(DisplayRect {
                left: 10,
                top: 20,
                right: 810,
                bottom: 620
            })
        );
        assert!(
            build_geometry(Some((0, 0)), None).is_none(),
            "missing size skipped"
        );
        assert!(
            build_geometry(None, Some((800, 600))).is_none(),
            "missing position skipped"
        );
        assert!(
            build_geometry(Some((0, 0)), Some((0, 0))).is_none(),
            "degenerate zero size skipped, never fabricated"
        );
    }
}
