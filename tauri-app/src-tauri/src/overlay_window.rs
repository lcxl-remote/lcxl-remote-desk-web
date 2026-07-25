use tauri::WebviewWindow;

trait OverlayFullscreenWindow {
    fn set_simple_fullscreen(&self, enable: bool) -> Result<(), String>;
}

impl OverlayFullscreenWindow for WebviewWindow {
    fn set_simple_fullscreen(&self, enable: bool) -> Result<(), String> {
        WebviewWindow::set_simple_fullscreen(self, enable).map_err(|error| error.to_string())
    }
}

/// Expands an overlay over its current monitor without creating a separate
/// macOS Space. Other platforms retain Tauri's native fullscreen fallback.
pub(crate) fn enter_overlay_fullscreen(window: &WebviewWindow) -> Result<(), String> {
    set_overlay_fullscreen(window)
}

fn set_overlay_fullscreen(window: &impl OverlayFullscreenWindow) -> Result<(), String> {
    window.set_simple_fullscreen(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct RecordingWindow {
        simple_fullscreen_calls: RefCell<Vec<bool>>,
    }

    impl OverlayFullscreenWindow for RecordingWindow {
        fn set_simple_fullscreen(&self, enable: bool) -> Result<(), String> {
            self.simple_fullscreen_calls.borrow_mut().push(enable);
            Ok(())
        }
    }

    #[test]
    fn overlay_uses_simple_fullscreen() {
        let window = RecordingWindow::default();

        set_overlay_fullscreen(&window).unwrap();

        assert_eq!(*window.simple_fullscreen_calls.borrow(), vec![true]);
    }
}
