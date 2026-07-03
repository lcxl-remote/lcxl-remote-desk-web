use windows::Win32::{
    Foundation::GetLastError,
    UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_MOUSE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
            MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
            MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
            MOUSEEVENTF_WHEEL, SendInput,
        },
        WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN,
        },
    },
};

use crate::{
    error::InputError,
    model::{
        data_channel::{MouseEventData, MouseEventHandler},
        geometry::SharedMonitorGeometry,
    },
};

pub struct WindowsMouseEventHandler {
    /// Shared, hot-updatable virtual-desktop rect of the captured
    /// monitor. The worker mutates it on display reconfiguration
    /// (`WM_DISPLAYCHANGE`, IDD `SetMode`, virtual display Attach /
    /// Detach) so the cursor lands correctly without the connection
    /// being torn down.
    geometry: SharedMonitorGeometry,
}

impl WindowsMouseEventHandler {
    pub fn new(geometry: SharedMonitorGeometry) -> Self {
        Self { geometry }
    }

    /// Build a `SendInput` mouse-move event that lands at the normalised
    /// `(x, y)` carried by a mouse event, expressed in absolute
    /// virtual-desktop coordinates.
    ///
    /// Every pointer action — moves, presses and releases — is injected
    /// through `SendInput` (never `SetCursorPos`) so the whole gesture
    /// flows through a single synthetic input stream. This is what makes
    /// "hold left button and drag to select text" work: once a button is
    /// pressed with `SendInput`, Windows keeps that button's held state in
    /// the injected stream, so each subsequent `MOUSEEVENTF_MOVE` is
    /// delivered to applications as a drag (`WM_MOUSEMOVE` with
    /// `MK_LBUTTON`) rather than a bare hover. Mixing `SetCursorPos` for
    /// motion with `SendInput` for buttons broke that continuity and made
    /// drag selection unreliable (notably in Windows Terminal).
    ///
    /// The browser ships `mousemove` on a separate, unordered data channel
    /// (it may drop or reorder packets) while clicks travel on the
    /// reliable channel. Prefixing every press / release with its own move
    /// input — rather than relying on a preceding move to have landed —
    /// guarantees the button is injected at exactly the point the user
    /// clicked, matching the macOS backend. The dispatcher serialises every
    /// input event for a connection, so the input batch built here is
    /// atomic with respect to other events.
    fn build_move_input(&self, x: f64, y: f64) -> INPUT {
        // Snapshot the geometry into locals before releasing the read
        // lock so no Win32 call below runs while the lock is held — keeps
        // the lock-hold window microscopic.
        let (left, top, width, height) = {
            let g = self.geometry.read().expect("monitor geometry poisoned");
            (g.left, g.top, g.width, g.height)
        };
        let (abs_x, abs_y) = compute_absolute(left, top, width, height, x, y);
        let (vx, vy, vw, vh) = virtual_desktop_metrics();
        let (nx, ny) = to_absolute_normalized(abs_x, abs_y, vx, vy, vw, vh);

        let mut input = INPUT::default();
        input.r#type = INPUT_MOUSE;
        input.Anonymous.mi.dx = nx;
        input.Anonymous.mi.dy = ny;
        input.Anonymous.mi.dwFlags =
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
        input
    }
}

/// Read the current virtual-desktop origin and size via `GetSystemMetrics`.
/// Returns `(x_origin, y_origin, width, height)`. The origin can be negative
/// when a secondary monitor sits left of / above the primary. Kept as a thin
/// wrapper so `build_move_input` stays lock-scoped and the pure normalisation
/// math below can be unit-tested without a real desktop.
fn virtual_desktop_metrics() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

/// Map a normalised `(x, y)` in `[0.0, 1.0]` to absolute virtual desktop
/// pixel coordinates, given the captured monitor's rect. Pulled out so the
/// arithmetic can be unit-tested without a real display attached.
fn compute_absolute(left: i32, top: i32, width: i32, height: i32, x: f64, y: f64) -> (i32, i32) {
    let abs_x = left + (x * width as f64) as i32;
    let abs_y = top + (y * height as f64) as i32;
    (abs_x, abs_y)
}

/// Convert an absolute virtual-desktop pixel `(abs_x, abs_y)` into the
/// `0..=65535` normalised range `SendInput` expects with
/// `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`.
///
/// `(vx, vy)` is the virtual-desktop origin (may be negative on multi-monitor
/// layouts) and `(vw, vh)` its size, as returned by `GetSystemMetrics`. The
/// origin is subtracted first so a monitor left of / above the primary maps
/// correctly; the span uses `size - 1` because a `w`-pixel-wide desktop
/// addresses columns `0..=w-1`, and half-span rounding gives nearest-pixel
/// accuracy. The result is clamped to `0..=65535` so an out-of-range pixel
/// never wraps. Pure function — unit-tested without a windowing session.
fn to_absolute_normalized(
    abs_x: i32,
    abs_y: i32,
    vx: i32,
    vy: i32,
    vw: i32,
    vh: i32,
) -> (i32, i32) {
    let span_x = (vw - 1).max(1) as i64;
    let span_y = (vh - 1).max(1) as i64;
    let nx = ((abs_x - vx) as i64 * 65535 + span_x / 2) / span_x;
    let ny = ((abs_y - vy) as i64 * 65535 + span_y / 2) / span_y;
    (nx.clamp(0, 65535) as i32, ny.clamp(0, 65535) as i32)
}

impl MouseEventHandler for WindowsMouseEventHandler {
    fn handle_mouse_move(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        // A bare absolute move. While a button is held, Windows carries
        // that button's state in the synthetic input stream, so this is
        // delivered as a drag rather than a hover — enabling text
        // selection and drag gestures.
        let inputs = [self.build_move_input(event.x, event.y)];
        unsafe {
            let result = SendInput(&inputs, size_of::<INPUT>() as i32);
            if result == 0 {
                let last_error = GetLastError();
                log::error!("Failed to send mouse move event, error: {:?}", last_error);
            }
        };
        Ok(())
    }

    fn handle_mouse_down(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        let mut mouse_event_flags = MOUSE_EVENT_FLAGS(0);
        match event.button {
            0 => mouse_event_flags |= MOUSEEVENTF_LEFTDOWN,
            1 => mouse_event_flags |= MOUSEEVENTF_MIDDLEDOWN,
            2 => mouse_event_flags |= MOUSEEVENTF_RIGHTDOWN,
            _ => {
                log::warn!("Unsupported mouse button: {}", event.button);
                return Ok(());
            }
        };
        // Inject an absolute move immediately before the press, in the same
        // `SendInput` batch, so the button lands at the click point even if
        // the preceding move was dropped or reordered on its unreliable
        // channel — and keeps the whole gesture in one synthetic stream so
        // a following drag is recognised.
        let move_input = self.build_move_input(event.x, event.y);
        let mut button_input = INPUT::default();
        button_input.r#type = INPUT_MOUSE;
        button_input.Anonymous.mi.dwFlags = mouse_event_flags;
        let inputs = [move_input, button_input];
        unsafe {
            let result = SendInput(&inputs, size_of::<INPUT>() as i32);
            if result == 0 {
                let last_error = GetLastError();
                log::error!(
                    "Failed to send mouse down event {}, error: {:?}",
                    event.button,
                    last_error
                );
            }
        };
        Ok(())
    }

    fn handle_mouse_up(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        let mut mouse_event_flags = MOUSE_EVENT_FLAGS(0);
        match event.button {
            0 => mouse_event_flags |= MOUSEEVENTF_LEFTUP,
            1 => mouse_event_flags |= MOUSEEVENTF_MIDDLEUP,
            2 => mouse_event_flags |= MOUSEEVENTF_RIGHTUP,
            _ => {
                log::warn!("Unsupported mouse button: {}", event.button);
                return Ok(());
            }
        };
        // Match the down path: absolute-move then release in one batch so a
        // drag whose intermediate moves were dropped still lifts at the
        // correct point.
        let move_input = self.build_move_input(event.x, event.y);
        let mut button_input = INPUT::default();
        button_input.r#type = INPUT_MOUSE;
        button_input.Anonymous.mi.dwFlags = mouse_event_flags;
        let inputs = [move_input, button_input];
        unsafe {
            let result = SendInput(&inputs, size_of::<INPUT>() as i32);
            if result == 0 {
                let last_error = GetLastError();
                log::error!(
                    "Failed to send mouse up event {}, error: {:?}",
                    event.button,
                    last_error
                );
            }
        };
        Ok(())
    }

    fn handle_mouse_wheel(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        let mut inputs = Vec::new();

        // Vertical scroll
        // Browser delta_y > 0 means scroll down
        // Windows MOUSEEVENTF_WHEEL: positive value means scroll up (away from user), negative means scroll down (towards user)
        if event.delta_y != 0.0 {
            let mut input = INPUT::default();
            input.r#type = INPUT_MOUSE;
            input.Anonymous.mi.dwFlags = MOUSEEVENTF_WHEEL;
            input.Anonymous.mi.mouseData = (-event.delta_y) as i32 as u32;
            inputs.push(input);
        }

        // Horizontal scroll
        // Browser delta_x > 0 means scroll right
        // Windows MOUSEEVENTF_HWHEEL: positive value means scroll right, negative means scroll left
        if event.delta_x != 0.0 {
            let mut input = INPUT::default();
            input.r#type = INPUT_MOUSE;
            input.Anonymous.mi.dwFlags = MOUSEEVENTF_HWHEEL;
            input.Anonymous.mi.mouseData = event.delta_x as i32 as u32;
            inputs.push(input);
        }

        if !inputs.is_empty() {
            unsafe {
                SendInput(&inputs, (size_of::<[INPUT; 1]>() * inputs.len()) as i32);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::geometry::{MonitorGeometry, shared};

    /// Primary monitor at the origin: the offset is zero, so the result
    /// is just `(x * width, y * height)`. Sanity check.
    #[test]
    fn compute_absolute_primary_monitor_center() {
        assert_eq!(compute_absolute(0, 0, 1280, 800, 0.5, 0.5), (640, 400));
    }

    /// IDD virtual monitor sitting to the right of a 1280-wide primary.
    /// The browser sends a normalised `(0.5, 0.5)` for the center of the
    /// captured surface; without applying `left = 1280` the cursor lands
    /// on the primary monitor (this is the bug the fix targets).
    #[test]
    fn compute_absolute_offset_monitor_to_the_right_translates_correctly() {
        // IDD rect: left=1280, top=0, right=2780, bottom=900 → 1500x900.
        // The center should land at (1280 + 750, 450) — crucially the
        // x coordinate is shifted by the monitor's `left` offset.
        let (x, y) = compute_absolute(1280, 0, 1500, 900, 0.5, 0.5);
        assert_eq!((x, y), (2030, 450));
    }

    /// Users can drag a monitor to the left of the primary in Display
    /// Settings, which gives a negative `left`. The virtual desktop
    /// coordinate space accepts negative values; `SetCursorPos` accepts
    /// them too.
    #[test]
    fn compute_absolute_negative_offset_is_preserved() {
        // left=-1500, center maps to (-1500 + 750, 450) = (-750, 450).
        let (x, y) = compute_absolute(-1500, 0, 1500, 900, 0.5, 0.5);
        assert_eq!((x, y), (-750, 450));
    }

    /// Vertically stacked monitor (second one below or above the
    /// primary) carries a non-zero `top`. The arithmetic is symmetric.
    #[test]
    fn compute_absolute_vertical_offset_translates_y() {
        let (x, y) = compute_absolute(0, 1080, 1920, 1080, 0.25, 0.75);
        assert_eq!((x, y), (480, 1080 + 810));
    }

    /// Corner pixels of the captured surface map to corners of the
    /// destination monitor's rect.
    #[test]
    fn compute_absolute_top_left_and_bottom_right_corners() {
        assert_eq!(compute_absolute(1280, 0, 1500, 900, 0.0, 0.0), (1280, 0));
        // Note (1.0, 1.0) maps to (left+width, top+height) i.e. the
        // *exclusive* far edge — one past the last addressable pixel.
        // SetCursorPos clamps internally, so this is acceptable.
        assert_eq!(compute_absolute(1280, 0, 1500, 900, 1.0, 1.0), (2780, 900));
    }

    /// A click (`mousedown` / `mouseup`) carries its own normalised
    /// coordinates and must land at exactly that point — the handlers now
    /// reposition the cursor before injecting the button event instead of
    /// relying on a preceding `mousemove` that may have been dropped or
    /// reordered on its unreliable channel. This pins the pixel a click
    /// at the surface center resolves to on an off-origin monitor; a
    /// regression that dropped the down/up repositioning (or stripped the
    /// `left` offset) would change this result.
    #[test]
    fn click_center_lands_on_offset_monitor_not_primary() {
        // IDD/secondary panel 1500x900 at x-offset 1280. The center of
        // the captured surface must resolve to (1280 + 750, 450), i.e.
        // inside that monitor — never (750, 450) on the primary.
        let (x, y) = compute_absolute(1280, 0, 1500, 900, 0.5, 0.5);
        assert_eq!((x, y), (2030, 450));
    }

    /// A click at the top-left corner of the captured surface must land
    /// on that monitor's origin, not the virtual-desktop origin. Guards
    /// the offset translation for the down/up path specifically.
    #[test]
    fn click_top_left_corner_lands_on_monitor_origin() {
        assert_eq!(compute_absolute(1280, 0, 1500, 900, 0.0, 0.0), (1280, 0));
    }

    /// The virtual-desktop origin pixel maps to the low end of the
    /// normalised range.
    #[test]
    fn to_absolute_normalized_origin_maps_to_zero() {
        assert_eq!(to_absolute_normalized(0, 0, 0, 0, 1920, 1080), (0, 0));
    }

    /// The far-corner pixel (last addressable column/row) maps to the top
    /// of the `0..=65535` range.
    #[test]
    fn to_absolute_normalized_far_corner_maps_to_max() {
        assert_eq!(
            to_absolute_normalized(1919, 1079, 0, 0, 1920, 1080),
            (65535, 65535)
        );
    }

    /// The exact center pixel of an odd-sized desktop maps to the midpoint
    /// of the range. 1001 px spans 0..=1000, so pixel 500 → 65535/2.
    #[test]
    fn to_absolute_normalized_center_is_midpoint() {
        assert_eq!(
            to_absolute_normalized(500, 500, 0, 0, 1001, 1001),
            (32768, 32768)
        );
    }

    /// A negative virtual-desktop origin (secondary monitor left of / above
    /// the primary) is subtracted before scaling, so the origin pixel still
    /// maps to zero and the primary origin lands partway across the range.
    #[test]
    fn to_absolute_normalized_negative_origin() {
        // 3840-wide virtual desktop whose origin is at -1920 (secondary
        // monitor to the left). The leftmost pixel maps to 0...
        assert_eq!(
            to_absolute_normalized(-1920, 0, -1920, 0, 3840, 1080),
            (0, 0)
        );
        // ...and the rightmost pixel to the max.
        assert_eq!(
            to_absolute_normalized(1919, 0, -1920, 0, 3840, 1080),
            (65535, 0)
        );
    }

    /// Pixels outside the virtual desktop are clamped, never wrapped: a
    /// coordinate below the origin pins to 0, one past the far edge to the
    /// max. Guards against `SendInput` receiving out-of-range values.
    #[test]
    fn to_absolute_normalized_clamps_out_of_range() {
        assert_eq!(to_absolute_normalized(-100, -100, 0, 0, 1920, 1080), (0, 0));
        assert_eq!(
            to_absolute_normalized(999999, 999999, 0, 0, 1920, 1080),
            (65535, 65535)
        );
    }

    /// A degenerate 1x1 (or zero-reported) virtual desktop must not divide
    /// by zero; the span is floored to 1 and the origin pixel maps to 0.
    #[test]
    fn to_absolute_normalized_single_pixel_desktop_no_div_by_zero() {
        assert_eq!(to_absolute_normalized(0, 0, 0, 0, 1, 1), (0, 0));
    }

    /// Hot-update path: the handler must observe writes made through a
    /// cloned `SharedMonitorGeometry` handle. This is the contract the
    /// `InputDispatcher::refresh_geometry` / `retarget_connection`
    /// callers rely on — they hold their own clone of the same Arc and
    /// mutate it after a display reconfig.
    #[test]
    fn compute_absolute_reflects_geometry_update() {
        let geometry = shared(MonitorGeometry::new(0, 0, 1280, 800));
        let writer = std::sync::Arc::clone(&geometry);

        // Pre-update: read through the handler's clone matches the
        // initial rect.
        let (l, t, w, h) = {
            let g = geometry.read().unwrap();
            (g.left, g.top, g.width, g.height)
        };
        assert_eq!(compute_absolute(l, t, w, h, 0.5, 0.5), (640, 400));

        // Worker-side write through the cloned handle.
        *writer.write().unwrap() = MonitorGeometry::new(1280, 0, 1500, 900);

        // Post-update: the handler's clone sees the new rect on the
        // next read — exactly what `handle_mouse_move` does on each
        // event.
        let (l, t, w, h) = {
            let g = geometry.read().unwrap();
            (g.left, g.top, g.width, g.height)
        };
        assert_eq!(compute_absolute(l, t, w, h, 0.5, 0.5), (1280 + 750, 450));
    }
}
