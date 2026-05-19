use windows::Win32::{
    Foundation::GetLastError,
    UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_MOUSE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
            MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
            MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, SendInput,
        },
        WindowsAndMessaging::SetCursorPos,
    },
};

use crate::{
    error::InputError,
    model::data_channel::{MouseEventData, MouseEventHandler},
};

pub struct WindowsMouseEventHandler {
    /// Virtual-desktop-space left edge of the captured monitor.
    /// Required because `SetCursorPos` uses the virtual desktop
    /// coordinate system whose origin is the primary monitor's top-left
    /// — non-primary monitors live at a non-zero offset.
    pub left: i32,
    /// Virtual-desktop-space top edge of the captured monitor.
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

impl WindowsMouseEventHandler {
    pub fn new(left: i32, top: i32, width: i32, height: i32) -> Self {
        Self {
            left,
            top,
            width,
            height,
        }
    }
}

/// Map a normalised `(x, y)` in `[0.0, 1.0]` to absolute virtual desktop
/// coordinates, given the captured monitor's rect. Pulled out of
/// `handle_mouse_move` so the arithmetic can be unit-tested without a
/// real display attached — `SetCursorPos` is unsafe Win32 and cannot be
/// exercised on CI.
fn compute_absolute(left: i32, top: i32, width: i32, height: i32, x: f64, y: f64) -> (i32, i32) {
    let abs_x = left + (x * width as f64) as i32;
    let abs_y = top + (y * height as f64) as i32;
    (abs_x, abs_y)
}

impl MouseEventHandler for WindowsMouseEventHandler {
    fn handle_mouse_move(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        let (x, y) =
            compute_absolute(self.left, self.top, self.width, self.height, event.x, event.y);
        let result = unsafe { SetCursorPos(x, y) };
        if let Err(error) = result {
            log::error!(
                "Failed to set cursor position to ({}, {}), error: {}",
                x,
                y,
                error
            );
        }
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
        let mut input = INPUT::default();
        input.r#type = INPUT_MOUSE;
        input.Anonymous.mi.dwFlags = mouse_event_flags;
        let inputs = [input];
        unsafe {
            let result = SendInput(&inputs, size_of::<[INPUT; 1]>() as i32);
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
        let mut input = INPUT::default();
        input.r#type = INPUT_MOUSE;
        input.Anonymous.mi.dwFlags = mouse_event_flags;
        let inputs = [input];
        unsafe {
            let result = SendInput(&inputs, size_of::<[INPUT; 1]>() as i32);
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
        assert_eq!(
            compute_absolute(1280, 0, 1500, 900, 1.0, 1.0),
            (2780, 900)
        );
    }
}
