//! Source labels for Windows input synthesized by this product.
//!
//! These values are classification markers, not credentials. A low-level hook
//! can use them to avoid treating the product's own browser or AI injection as
//! external user input. Every missing, unknown, or stripped marker remains
//! external and must conservatively preempt an AI writer lease.

use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT, INPUT_KEYBOARD, INPUT_MOUSE};

pub const AI_INPUT_MARKER: usize = 0x4c58_4101;
pub const BROWSER_INPUT_MARKER: usize = 0x4c58_4201;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputSource {
    Ai,
    Browser,
    External,
}

#[must_use]
pub const fn classify_input(extra_info: usize, injected: bool) -> InputSource {
    match (extra_info, injected) {
        (AI_INPUT_MARKER, true) => InputSource::Ai,
        (BROWSER_INPUT_MARKER, true) => InputSource::Browser,
        _ => InputSource::External,
    }
}

/// Mark an input produced by the existing remote-controller/browser path.
///
/// `INPUT` is a tagged union. Callers must set `r#type` before invoking this
/// helper; unsupported input kinds are left unmarked and therefore fail closed
/// as external input in the ownership monitor.
pub fn mark_browser_input(input: &mut INPUT) {
    match input.r#type {
        INPUT_KEYBOARD => input.Anonymous.ki.dwExtraInfo = BROWSER_INPUT_MARKER,
        INPUT_MOUSE => input.Anonymous.mi.dwExtraInfo = BROWSER_INPUT_MARKER,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_product_markers_are_internal() {
        assert_eq!(classify_input(AI_INPUT_MARKER, true), InputSource::Ai);
        assert_eq!(
            classify_input(BROWSER_INPUT_MARKER, true),
            InputSource::Browser
        );
        assert_eq!(classify_input(0, false), InputSource::External);
        assert_eq!(classify_input(0, true), InputSource::External);
        assert_eq!(
            classify_input(AI_INPUT_MARKER, false),
            InputSource::External
        );
        assert_eq!(
            classify_input(BROWSER_INPUT_MARKER, false),
            InputSource::External
        );
        assert_eq!(
            classify_input(AI_INPUT_MARKER + 1, true),
            InputSource::External
        );
        assert_eq!(
            classify_input(BROWSER_INPUT_MARKER + 1, true),
            InputSource::External
        );
    }

    #[test]
    fn browser_marker_is_written_to_keyboard_and_mouse_inputs() {
        let mut keyboard = INPUT::default();
        keyboard.r#type = INPUT_KEYBOARD;
        mark_browser_input(&mut keyboard);
        assert_eq!(
            unsafe { keyboard.Anonymous.ki.dwExtraInfo },
            BROWSER_INPUT_MARKER
        );

        let mut mouse = INPUT::default();
        mouse.r#type = INPUT_MOUSE;
        mark_browser_input(&mut mouse);
        assert_eq!(
            unsafe { mouse.Anonymous.mi.dwExtraInfo },
            BROWSER_INPUT_MARKER
        );
    }
}
