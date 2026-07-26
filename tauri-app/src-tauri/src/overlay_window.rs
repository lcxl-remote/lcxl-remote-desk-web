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
        // Paint the window itself opaque black. What the privacy screen
        // promises is that the desktop underneath cannot be seen, and that
        // promise must not depend on a web page rendering successfully: a
        // failed load, a blank render or a webview that never got sized would
        // otherwise leave the real desktop on display. The page draws its own
        // matching background on top of this.
        let _: () = objc2::msg_send![ns_window, setOpaque: true];
        let black: *mut AnyObject = objc2::msg_send![objc2::class!(NSColor), blackColor];
        if !black.is_null() {
            let _: () = objc2::msg_send![ns_window, setBackgroundColor: black];
        }

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
    unsafe { log_overlay_compositing_state(ns_window) };

    Ok(level)
}

/// Log the AppKit state that decides whether the overlay actually paints.
///
/// Frame, visibility and page load can all look correct while the physical
/// display still shows the desktop — the window can be fully transparent, sit
/// on another Space, be occluded, or host a webview that was never sized. Only
/// scalar and pointer-returning messages are used so no Objective-C struct
/// return is involved.
///
/// # Safety
///
/// `ns_window` must be a live `NSWindow` and this must run on the main thread.
#[cfg(target_os = "macos")]
unsafe fn log_overlay_compositing_state(ns_window: *mut objc2::runtime::AnyObject) {
    use objc2::runtime::AnyObject;

    unsafe {
        let level: isize = objc2::msg_send![ns_window, level];
        let is_visible: bool = objc2::msg_send![ns_window, isVisible];
        let is_opaque: bool = objc2::msg_send![ns_window, isOpaque];
        let alpha: f64 = objc2::msg_send![ns_window, alphaValue];
        // NSWindowOcclusionStateVisible is bit 1; zero means fully occluded.
        let occlusion: usize = objc2::msg_send![ns_window, occlusionState];
        let style_mask: usize = objc2::msg_send![ns_window, styleMask];
        let collection_behavior: usize = objc2::msg_send![ns_window, collectionBehavior];
        // NSWindowSharingNone is 0, which is what content protection sets.
        let sharing_type: usize = objc2::msg_send![ns_window, sharingType];
        let on_active_space: bool = objc2::msg_send![ns_window, isOnActiveSpace];
        let screen: *mut AnyObject = objc2::msg_send![ns_window, screen];

        log::info!(
            "Privacy overlay window state level={level} visible={is_visible} opaque={is_opaque} \
             alpha={alpha} occlusion={occlusion:#x} style_mask={style_mask:#x} \
             collection_behavior={collection_behavior:#x} sharing_type={sharing_type} \
             on_active_space={on_active_space} has_screen={}",
            !screen.is_null()
        );

        let content_view: *mut AnyObject = objc2::msg_send![ns_window, contentView];
        if content_view.is_null() {
            log::warn!("Privacy overlay has no content view");
            return;
        }
        let content_hidden: bool = objc2::msg_send![content_view, isHidden];
        let content_alpha: f64 = objc2::msg_send![content_view, alphaValue];
        let subviews: *mut AnyObject = objc2::msg_send![content_view, subviews];
        let subview_count: usize = if subviews.is_null() {
            0
        } else {
            objc2::msg_send![subviews, count]
        };

        log::info!(
            "Privacy overlay content view hidden={content_hidden} alpha={content_alpha} \
             subviews={subview_count}"
        );

        if subview_count > 0 {
            let webview: *mut AnyObject = objc2::msg_send![subviews, objectAtIndex: 0usize];
            if !webview.is_null() {
                let hidden: bool = objc2::msg_send![webview, isHidden];
                let webview_alpha: f64 = objc2::msg_send![webview, alphaValue];
                let opaque: bool = objc2::msg_send![webview, isOpaque];
                log::info!(
                    "Privacy overlay webview hidden={hidden} alpha={webview_alpha} opaque={opaque}"
                );
            }
        }
    }
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
