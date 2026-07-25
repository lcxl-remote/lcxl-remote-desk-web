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

/// How long the caller waits for the main thread to apply the window level.
#[cfg(target_os = "macos")]
const MAIN_THREAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// `kCGScreenSaverWindowLevelKey` from `CGWindowLevel.h`.
#[cfg(target_os = "macos")]
const K_CG_SCREEN_SAVER_WINDOW_LEVEL_KEY: i32 = 13;

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// Resolves a `CGWindowLevelKey` to the numeric level AppKit expects.
    ///
    /// `core-graphics` does not re-export this, and hard-coding the numeric
    /// level would bake in a value Apple documents only through this function.
    /// Declared with an explicit framework link so it does not depend on
    /// another crate's optional `link` feature.
    unsafe fn CGWindowLevelForKey(key: i32) -> i32;
}

/// Raise the privacy overlay above the menu bar and Dock and show it without
/// activating the application.
///
/// The privacy screen must cover the whole display while the application the
/// user was working in keeps keyboard focus, so remote keystrokes still land
/// there. That rules out simple fullscreen, which hides the menu bar through
/// `NSApplication.presentationOptions` — those only take effect while this
/// application is active. Setting the native window level to the screen-saver
/// level covers the same system UI without activating anything, and
/// `orderFrontRegardless` brings the window forward without making it key.
///
/// AppKit objects may only be touched on the main thread, while the privacy
/// screen state machine runs on its own thread, so the work is dispatched and
/// awaited with a bounded wait.
#[cfg(target_os = "macos")]
pub(crate) fn show_overlay_without_activation(window: &WebviewWindow) -> Result<(), String> {
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel::<Result<i32, String>>();
    let target = window.clone();
    window
        .run_on_main_thread(move || {
            let _ = tx.send(raise_overlay_on_main_thread(&target));
        })
        .map_err(|error| format!("Failed to dispatch overlay setup to main thread: {}", error))?;

    let level = rx
        .recv_timeout(MAIN_THREAD_TIMEOUT)
        .map_err(|error| format!("Overlay main thread setup did not complete: {}", error))??;
    log::info!("Privacy overlay raised to native window level {}", level);
    Ok(())
}

/// Runs on the main thread: apply the screen-saver level and order the window
/// forward. Returns the level that was applied so the caller can log evidence.
#[cfg(target_os = "macos")]
fn raise_overlay_on_main_thread(window: &WebviewWindow) -> Result<i32, String> {
    use objc2::runtime::AnyObject;

    let ns_window = window
        .ns_window()
        .map_err(|error| format!("Failed to obtain NSWindow: {}", error))?
        as *mut AnyObject;
    if ns_window.is_null() {
        return Err("NSWindow handle is null".to_string());
    }

    let level = unsafe { CGWindowLevelForKey(K_CG_SCREEN_SAVER_WINDOW_LEVEL_KEY) };
    unsafe {
        let _: () = objc2::msg_send![ns_window, setLevel: level as isize];
        let _: () = objc2::msg_send![ns_window, orderFrontRegardless];
    }

    // The overlay must not activate this application: the application the user
    // was working in has to keep keyboard focus so remote keystrokes reach it.
    let app_active = unsafe {
        let app: *mut AnyObject = objc2::msg_send![objc2::class!(NSApplication), sharedApplication];
        if app.is_null() {
            false
        } else {
            objc2::msg_send![app, isActive]
        }
    };
    log::info!(
        "Privacy overlay ordered front without activation, NSApp.isActive={}",
        app_active
    );

    Ok(level)
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
