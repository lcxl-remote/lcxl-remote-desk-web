//! Privacy screen lifecycle.
//!
//! The overlay can be dismissed from several directions — a remote `Hide`, the
//! local escape chord consumed by the platform input interceptor, the Tauri
//! global shortcut, or a session teardown — so all of them are funnelled into
//! one event loop. That loop is the only owner of
//! `controlled_by_connection_id`, which keeps the reported `visible` state and
//! the real window from drifting apart.

use crate::platform::{self, LocalEscapeCallback};
use desk_input_injection::model::host_control::{HostControlEventType, PrivateScreenCommand};
use std::sync::Arc;
use std::sync::mpsc;
use tauri::AppHandle;

pub struct PrivateScreenManager {
    app_handle: AppHandle,
    frontend_url: String,
}

#[cfg(not(target_os = "linux"))]
const PRIVATE_SCREEN_WINDOW_LABEL: &str = "private-screen";
const HOTKEY: &str = "ctrl+alt+l";

/// Everything that can change the privacy screen's state.
enum PrivateScreenEvent {
    /// A command from a remote controller, forwarded unchanged from the shared
    /// host-control channel.
    Remote(PrivateScreenCommand),
    /// The local user asked to leave the privacy screen, either through the
    /// platform input interceptor's escape chord or the global shortcut.
    LocalDismiss,
}

/// Whether the event loop keeps running after handling an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopControl {
    Continue,
    Stop,
}

/// The platform side of the privacy screen lifecycle.
///
/// Behind a trait so the show transaction and its rollback can be exercised
/// without a window server or Accessibility approval.
trait PrivateScreenOps {
    /// Put the process into whatever state the overlay window has to be born
    /// into. On macOS the overlay is evicted from a full-screen Space unless
    /// the process is an accessory application at the moment it is created.
    fn adopt_overlay_activation_policy(&self) -> Result<(), String>;
    /// Undo `adopt_overlay_activation_policy`. Safe to call when it never ran.
    fn restore_activation_policy(&self);
    /// Create and position the overlay window without making it visible.
    fn prepare_overlay(&self) -> Result<(), String>;
    /// Start intercepting local input. `on_local_escape` fires once the local
    /// escape chord has been fully consumed.
    fn start_input_interception(&self, on_local_escape: LocalEscapeCallback) -> Result<(), String>;
    fn stop_input_interception(&self);
    fn register_escape_hotkey(&self) -> Result<(), String>;
    fn unregister_escape_hotkey(&self);
    /// Make the prepared overlay visible.
    fn present_overlay(&self) -> Result<(), String>;
    fn close_overlay(&self);
}

/// Bring the privacy screen up as a transaction.
///
/// Every step is rolled back if a later one fails, because a partially entered
/// privacy screen is the worst outcome available: an interception that started
/// without a visible overlay locks the keyboard behind an invisible wall, and a
/// visible overlay without interception claims a protection that is not there.
fn enter_private_screen(
    ops: &impl PrivateScreenOps,
    on_local_escape: LocalEscapeCallback,
) -> Result<(), String> {
    let outcome = (|| {
        // The window has to be created after the policy is adopted: a window
        // that already exists keeps the behaviour of the policy it was born
        // under.
        ops.adopt_overlay_activation_policy()?;
        ops.prepare_overlay()?;
        ops.start_input_interception(on_local_escape)?;
        ops.register_escape_hotkey()?;
        ops.present_overlay()
    })();

    if let Err(error) = outcome {
        // Roll back through the ordinary teardown rather than a bespoke
        // unwind, so the two can never drift apart. Every step tolerates
        // having nothing to undo.
        leave_private_screen(ops);
        return Err(error);
    }

    Ok(())
}

/// Tear the privacy screen down. Every step is best effort and idempotent so
/// repeated or racing dismissals converge on the same state.
fn leave_private_screen(ops: &impl PrivateScreenOps) {
    ops.unregister_escape_hotkey();
    ops.stop_input_interception();
    ops.close_overlay();
    // Last, so the process only returns to its normal presence once the
    // overlay it was adopted for is gone.
    ops.restore_activation_policy();
}

/// The single owner of the privacy screen's state.
struct PrivateScreenCore<O: PrivateScreenOps> {
    ops: O,
    controlled_by_connection_id: Option<String>,
}

impl<O: PrivateScreenOps> PrivateScreenCore<O> {
    fn new(ops: O) -> Self {
        Self {
            ops,
            controlled_by_connection_id: None,
        }
    }

    /// Whether a command from `connection_id` may act on the current session.
    fn is_authorized(&self, connection_id: &str) -> bool {
        match &self.controlled_by_connection_id {
            Some(owner) => owner == connection_id,
            None => true,
        }
    }

    fn handle_event(
        &mut self,
        event: PrivateScreenEvent,
        on_local_escape: &LocalEscapeCallback,
        state_sender: &tokio::sync::mpsc::UnboundedSender<HostControlEventType>,
    ) -> LoopControl {
        match event {
            PrivateScreenEvent::Remote(PrivateScreenCommand::Show(from_connection_id)) => {
                if !self.is_authorized(&from_connection_id) {
                    log::warn!("Private screen is already controlled by another connection");
                    return LoopControl::Continue;
                }

                match enter_private_screen(&self.ops, on_local_escape.clone()) {
                    Ok(()) => {
                        let _ =
                            state_sender.send(HostControlEventType::PrivateScreenVisibleChanged(
                                from_connection_id.clone(),
                                true,
                            ));
                        self.controlled_by_connection_id = Some(from_connection_id);
                    }
                    Err(error) => {
                        // The transaction already rolled itself back, so the
                        // machine is left unprotected but usable. Report the
                        // failure instead of claiming the screen is up.
                        log::error!("Failed to show private screen: {}", error);
                        let _ = state_sender.send(HostControlEventType::PrivateScreenUnknownError(
                            Some(from_connection_id),
                            error,
                        ));
                    }
                }
                LoopControl::Continue
            }
            PrivateScreenEvent::Remote(PrivateScreenCommand::Hide(from_connection_id)) => {
                if !self.is_authorized(&from_connection_id) {
                    log::warn!("Private screen is already controlled by another connection");
                    return LoopControl::Continue;
                }

                leave_private_screen(&self.ops);
                self.controlled_by_connection_id = None;
                let _ = state_sender.send(HostControlEventType::PrivateScreenVisibleChanged(
                    from_connection_id,
                    false,
                ));
                LoopControl::Continue
            }
            PrivateScreenEvent::Remote(PrivateScreenCommand::Quit) => {
                leave_private_screen(&self.ops);
                if let Some(owner) = self.controlled_by_connection_id.take() {
                    let _ = state_sender.send(HostControlEventType::PrivateScreenVisibleChanged(
                        owner, false,
                    ));
                }
                log::info!("Private screen quit");
                LoopControl::Stop
            }
            PrivateScreenEvent::LocalDismiss => {
                let Some(owner) = self.controlled_by_connection_id.take() else {
                    // The overlay is already down; reporting `visible=false`
                    // for no connection would invent a state change.
                    log::debug!("Local private screen dismissal with no active session, ignoring");
                    return LoopControl::Continue;
                };

                leave_private_screen(&self.ops);
                let _ = state_sender.send(HostControlEventType::PrivateScreenVisibleChanged(
                    owner, false,
                ));
                LoopControl::Continue
            }
        }
    }
}

impl PrivateScreenManager {
    pub fn new(app_handle: AppHandle, frontend_url: String) -> Self {
        Self {
            app_handle,
            frontend_url,
        }
    }

    pub fn start(
        self,
        cmd_receiver: mpsc::Receiver<PrivateScreenCommand>,
        state_sender: tokio::sync::mpsc::UnboundedSender<HostControlEventType>,
    ) {
        let (event_sender, event_receiver) = mpsc::channel::<PrivateScreenEvent>();

        // Adapt the shared host-control channel into the internal event
        // channel so the loop below has a single input. The shared
        // `PrivateScreenCommand` wire enum is untouched.
        let remote_sender = event_sender.clone();
        std::thread::spawn(move || {
            while let Ok(command) = cmd_receiver.recv() {
                if remote_sender
                    .send(PrivateScreenEvent::Remote(command))
                    .is_err()
                {
                    break;
                }
            }
            log::info!("Private screen command channel closed");
        });

        let ops = TauriPrivateScreenOps {
            app_handle: self.app_handle,
            frontend_url: self.frontend_url,
            event_sender: event_sender.clone(),
        };

        // The interceptor calls this from its own thread and must not wait for
        // the loop below, which tears that very thread down while handling the
        // dismissal.
        let escape_sender = event_sender;
        let on_local_escape: LocalEscapeCallback = Arc::new(move || {
            let _ = escape_sender.send(PrivateScreenEvent::LocalDismiss);
        });

        std::thread::spawn(move || {
            let mut core = PrivateScreenCore::new(ops);
            while let Ok(event) = event_receiver.recv() {
                if core.handle_event(event, &on_local_escape, &state_sender) == LoopControl::Stop {
                    break;
                }
            }
            log::info!("Private screen loop exited");
        });
    }
}

/// The production platform implementation.
struct TauriPrivateScreenOps {
    app_handle: AppHandle,
    frontend_url: String,
    event_sender: mpsc::Sender<PrivateScreenEvent>,
}

impl PrivateScreenOps for TauriPrivateScreenOps {
    fn adopt_overlay_activation_policy(&self) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        {
            crate::overlay_window::set_overlay_activation_policy(&self.app_handle, true)
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(())
        }
    }

    fn restore_activation_policy(&self) {
        #[cfg(target_os = "macos")]
        if let Err(error) =
            crate::overlay_window::set_overlay_activation_policy(&self.app_handle, false)
        {
            log::warn!("Failed to restore the application activation policy: {error}");
        }
    }

    fn prepare_overlay(&self) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        {
            // Linux has no overlay window: the screen is darkened through
            // xrandr while the input devices are grabbed.
            Ok(())
        }

        #[cfg(not(target_os = "linux"))]
        {
            use tauri::Manager as _;

            let window = match self
                .app_handle
                .get_webview_window(PRIVATE_SCREEN_WINDOW_LABEL)
            {
                Some(window) => window,
                None => self.build_overlay_window()?,
            };

            let monitors = window.available_monitors().unwrap_or_default();
            log::info!(
                "Private screen sees {} available monitor(s)",
                monitors.len()
            );
            if monitors.len() > 1 {
                log::warn!(
                    "Private screen covers the primary monitor only, {} monitors are attached; \
                     content on the other monitors stays visible while local input is blocked",
                    monitors.len()
                );
            }

            match window.primary_monitor() {
                Ok(Some(monitor)) => {
                    log::info!(
                        "Private screen target monitor name={:?} position={:?} size={:?}",
                        monitor.name(),
                        monitor.position(),
                        monitor.size()
                    );
                    if let Err(error) = window.set_size(*monitor.size()) {
                        log::warn!("Failed to size private screen to the primary monitor: {error}");
                    }
                    if let Err(error) = window.set_position(*monitor.position()) {
                        log::warn!(
                            "Failed to position private screen on the primary monitor: {error}"
                        );
                    }
                }
                Ok(None) => log::warn!(
                    "No primary monitor reported, private screen keeps its default frame"
                ),
                Err(error) => log::warn!("Failed to resolve the primary monitor: {error}"),
            }

            Ok(())
        }
    }

    fn start_input_interception(&self, on_local_escape: LocalEscapeCallback) -> Result<(), String> {
        platform::block_input(true, Some(on_local_escape))
    }

    fn stop_input_interception(&self) {
        if let Err(error) = platform::block_input(false, None) {
            log::warn!("Failed to stop local input interception: {}", error);
        }
    }

    fn register_escape_hotkey(&self) -> Result<(), String> {
        use tauri_plugin_global_shortcut::GlobalShortcutExt;

        // Re-registering the same shortcut fails, so drop any stale
        // registration from an earlier session first.
        let _ = self.app_handle.global_shortcut().unregister(HOTKEY);

        let event_sender = self.event_sender.clone();
        self.app_handle
            .global_shortcut()
            .on_shortcut(HOTKEY, move |_app, _shortcut, event| {
                if event.state == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                    log::info!("Private screen hotkey pressed");
                    // The state machine owns the teardown; this path only
                    // reports the intent so the reported `visible` state and
                    // the real window cannot drift apart.
                    let _ = event_sender.send(PrivateScreenEvent::LocalDismiss);
                }
            })
            .map_err(|e| e.to_string())
    }

    fn unregister_escape_hotkey(&self) {
        use tauri_plugin_global_shortcut::GlobalShortcutExt;

        if let Err(error) = self.app_handle.global_shortcut().unregister(HOTKEY) {
            log::warn!("Failed to unregister the private screen hotkey: {}", error);
        }
    }

    fn present_overlay(&self) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        {
            Ok(())
        }

        #[cfg(not(target_os = "linux"))]
        {
            use tauri::Manager as _;

            let window = self
                .app_handle
                .get_webview_window(PRIVATE_SCREEN_WINDOW_LABEL)
                .ok_or_else(|| "Private screen window is missing".to_string())?;

            #[cfg(target_os = "macos")]
            {
                // macOS deliberately skips `show()`, simple fullscreen and
                // always-on-top: see `overlay_window` for why each of them
                // would either activate this application or drop the window
                // back below the menu bar.
                crate::overlay_window::show_overlay_without_activation(&window)?;
            }

            #[cfg(not(target_os = "macos"))]
            {
                window.show().map_err(|e| e.to_string())?;
                crate::overlay_window::enter_overlay_fullscreen(&window)?;
                window.set_always_on_top(true).map_err(|e| e.to_string())?;
                window.set_focus().map_err(|e| e.to_string())?;
            }

            // Remote clicks are posted in screen coordinates and have to reach
            // the application underneath, so the overlay never takes the mouse.
            let _ = window.set_ignore_cursor_events(true);

            log::info!(
                "Private screen presented position={:?} size={:?} visible={:?} focused={:?}",
                window.outer_position(),
                window.outer_size(),
                window.is_visible(),
                window.is_focused()
            );

            Ok(())
        }
    }

    fn close_overlay(&self) {
        #[cfg(not(target_os = "linux"))]
        {
            use tauri::Manager as _;

            if let Some(window) = self
                .app_handle
                .get_webview_window(PRIVATE_SCREEN_WINDOW_LABEL)
                && let Err(error) = window.close()
            {
                log::warn!("Failed to close the private screen window: {}", error);
            }
        }
    }
}

impl TauriPrivateScreenOps {
    #[cfg(not(target_os = "linux"))]
    fn build_overlay_window(&self) -> Result<tauri::WebviewWindow, String> {
        use tauri::{WebviewUrl, WebviewWindowBuilder};

        let url = format!("{}/private-screen?tauri=1", self.frontend_url);
        let builder = WebviewWindowBuilder::new(
            &self.app_handle,
            PRIVATE_SCREEN_WINDOW_LABEL,
            WebviewUrl::External(url.parse().map_err(|e| format!("{e}"))?),
        )
        .title(rust_i18n::t!("private_screen_title"))
        .always_on_top(true)
        .decorations(false)
        .skip_taskbar(true)
        .resizable(false)
        .content_protected(true) // Keep the overlay out of screen capture
        .minimizable(false)
        .on_page_load(|window, event| {
            log::info!(
                "Private screen page load {:?} for {}",
                event.event(),
                window.label()
            );
            if let tauri::webview::PageLoadEvent::Finished = event.event() {
                crate::inject_native_bridge_state(&window);
            }
        });

        // The overlay must never steal keyboard focus: remote keystrokes have
        // to keep reaching the application the user was working in. It is
        // created hidden and ordered front explicitly once it is positioned.
        #[cfg(target_os = "macos")]
        let builder = builder
            .visible(false)
            .focused(false)
            .focusable(false)
            .visible_on_all_workspaces(true);

        builder.build().map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A step of the platform lifecycle, recorded in call order.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Step {
        AdoptPolicy,
        RestorePolicy,
        Prepare,
        StartInterception,
        StopInterception,
        RegisterHotkey,
        UnregisterHotkey,
        Present,
        Close,
    }

    #[derive(Default)]
    struct RecordingOps {
        steps: RefCell<Vec<Step>>,
        fail_at: Option<Step>,
    }

    impl RecordingOps {
        fn failing(step: Step) -> Self {
            Self {
                steps: RefCell::new(Vec::new()),
                fail_at: Some(step),
            }
        }

        fn record(&self, step: Step) -> Result<(), String> {
            self.steps.borrow_mut().push(step);
            if self.fail_at == Some(step) {
                return Err(format!("{step:?} failed"));
            }
            Ok(())
        }

        fn steps(&self) -> Vec<Step> {
            self.steps.borrow().clone()
        }
    }

    impl PrivateScreenOps for RecordingOps {
        fn adopt_overlay_activation_policy(&self) -> Result<(), String> {
            self.record(Step::AdoptPolicy)
        }

        fn restore_activation_policy(&self) {
            let _ = self.record(Step::RestorePolicy);
        }

        fn prepare_overlay(&self) -> Result<(), String> {
            self.record(Step::Prepare)
        }

        fn start_input_interception(
            &self,
            _on_local_escape: LocalEscapeCallback,
        ) -> Result<(), String> {
            self.record(Step::StartInterception)
        }

        fn stop_input_interception(&self) {
            let _ = self.record(Step::StopInterception);
        }

        fn register_escape_hotkey(&self) -> Result<(), String> {
            self.record(Step::RegisterHotkey)
        }

        fn unregister_escape_hotkey(&self) {
            let _ = self.record(Step::UnregisterHotkey);
        }

        fn present_overlay(&self) -> Result<(), String> {
            self.record(Step::Present)
        }

        fn close_overlay(&self) {
            let _ = self.record(Step::Close);
        }
    }

    fn noop_escape() -> LocalEscapeCallback {
        Arc::new(|| {})
    }

    fn channel() -> (
        tokio::sync::mpsc::UnboundedSender<HostControlEventType>,
        tokio::sync::mpsc::UnboundedReceiver<HostControlEventType>,
    ) {
        tokio::sync::mpsc::unbounded_channel()
    }

    fn drain(
        receiver: &mut tokio::sync::mpsc::UnboundedReceiver<HostControlEventType>,
    ) -> Vec<HostControlEventType> {
        let mut events = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            events.push(event);
        }
        events
    }

    fn visible_changes(events: &[HostControlEventType]) -> Vec<(String, bool)> {
        events
            .iter()
            .filter_map(|event| match event {
                HostControlEventType::PrivateScreenVisibleChanged(id, visible) => {
                    Some((id.clone(), *visible))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn show_runs_the_lifecycle_in_order() {
        let ops = RecordingOps::default();

        enter_private_screen(&ops, noop_escape()).unwrap();

        assert_eq!(
            ops.steps(),
            vec![
                Step::AdoptPolicy,
                Step::Prepare,
                Step::StartInterception,
                Step::RegisterHotkey,
                Step::Present,
            ]
        );
    }

    /// The interception is what makes the privacy screen a privacy screen. If
    /// it cannot start, no overlay may be left behind.
    #[test]
    fn interception_failure_rolls_back_the_window() {
        let ops = RecordingOps::failing(Step::StartInterception);

        let error = enter_private_screen(&ops, noop_escape()).unwrap_err();

        assert!(error.contains("StartInterception"));
        assert_eq!(
            ops.steps(),
            vec![
                Step::AdoptPolicy,
                Step::Prepare,
                Step::StartInterception,
                Step::UnregisterHotkey,
                Step::StopInterception,
                Step::Close,
                Step::RestorePolicy,
            ]
        );
    }

    /// Without the escape hotkey the local user has no way back, so a failure
    /// there must release the input interception too.
    #[test]
    fn hotkey_failure_rolls_back_interception_and_window() {
        let ops = RecordingOps::failing(Step::RegisterHotkey);

        enter_private_screen(&ops, noop_escape()).unwrap_err();

        assert_eq!(
            ops.steps(),
            vec![
                Step::AdoptPolicy,
                Step::Prepare,
                Step::StartInterception,
                Step::RegisterHotkey,
                Step::UnregisterHotkey,
                Step::StopInterception,
                Step::Close,
                Step::RestorePolicy,
            ]
        );
    }

    /// An interception without a visible overlay would lock the keyboard behind
    /// an invisible wall.
    #[test]
    fn present_failure_rolls_back_everything() {
        let ops = RecordingOps::failing(Step::Present);

        enter_private_screen(&ops, noop_escape()).unwrap_err();

        assert_eq!(
            ops.steps(),
            vec![
                Step::AdoptPolicy,
                Step::Prepare,
                Step::StartInterception,
                Step::RegisterHotkey,
                Step::Present,
                Step::UnregisterHotkey,
                Step::StopInterception,
                Step::Close,
                Step::RestorePolicy,
            ]
        );
    }

    /// The activation policy is process-wide, so a show that cannot even adopt
    /// it must not go on to build anything.
    #[test]
    fn activation_policy_failure_stops_before_the_window_exists() {
        let ops = RecordingOps::failing(Step::AdoptPolicy);

        enter_private_screen(&ops, noop_escape()).unwrap_err();

        assert!(!ops.steps().contains(&Step::Prepare));
        assert!(!ops.steps().contains(&Step::StartInterception));
        assert_eq!(*ops.steps().last().unwrap(), Step::RestorePolicy);
    }

    #[test]
    fn failed_show_reports_an_error_and_leaves_no_owner() {
        let (sender, mut receiver) = channel();
        let mut core = PrivateScreenCore::new(RecordingOps::failing(Step::StartInterception));

        core.handle_event(
            PrivateScreenEvent::Remote(PrivateScreenCommand::Show("conn-1".to_string())),
            &noop_escape(),
            &sender,
        );

        assert!(core.controlled_by_connection_id.is_none());
        let events = drain(&mut receiver);
        assert!(
            visible_changes(&events).is_empty(),
            "a failed show must not claim the privacy screen is up"
        );
        assert!(matches!(
            events.as_slice(),
            [HostControlEventType::PrivateScreenUnknownError(Some(id), _)] if id == "conn-1"
        ));
    }

    #[test]
    fn local_dismiss_closes_the_active_session() {
        let (sender, mut receiver) = channel();
        let mut core = PrivateScreenCore::new(RecordingOps::default());

        core.handle_event(
            PrivateScreenEvent::Remote(PrivateScreenCommand::Show("conn-1".to_string())),
            &noop_escape(),
            &sender,
        );
        core.handle_event(PrivateScreenEvent::LocalDismiss, &noop_escape(), &sender);

        assert!(core.controlled_by_connection_id.is_none());
        assert_eq!(
            visible_changes(&drain(&mut receiver)),
            vec![("conn-1".to_string(), true), ("conn-1".to_string(), false)]
        );
    }

    /// A dismissal with nothing to dismiss must not invent a state change.
    #[test]
    fn local_dismiss_without_an_owner_reports_nothing() {
        let (sender, mut receiver) = channel();
        let mut core = PrivateScreenCore::new(RecordingOps::default());

        core.handle_event(PrivateScreenEvent::LocalDismiss, &noop_escape(), &sender);

        assert!(core.controlled_by_connection_id.is_none());
        assert!(drain(&mut receiver).is_empty());
        assert!(core.ops.steps().is_empty());
    }

    /// The hotkey and a remote hide can arrive back to back; only the first one
    /// has a session to close.
    #[test]
    fn local_dismiss_then_remote_hide_closes_once() {
        let (sender, mut receiver) = channel();
        let mut core = PrivateScreenCore::new(RecordingOps::default());

        core.handle_event(
            PrivateScreenEvent::Remote(PrivateScreenCommand::Show("conn-1".to_string())),
            &noop_escape(),
            &sender,
        );
        core.handle_event(PrivateScreenEvent::LocalDismiss, &noop_escape(), &sender);
        core.handle_event(
            PrivateScreenEvent::Remote(PrivateScreenCommand::Hide("conn-1".to_string())),
            &noop_escape(),
            &sender,
        );

        assert!(core.controlled_by_connection_id.is_none());
        // The trailing remote hide still answers its own request, but the
        // session was already closed and is not closed twice.
        assert_eq!(
            visible_changes(&drain(&mut receiver)),
            vec![
                ("conn-1".to_string(), true),
                ("conn-1".to_string(), false),
                ("conn-1".to_string(), false),
            ]
        );
    }

    #[test]
    fn a_foreign_connection_cannot_hide_someone_elses_private_screen() {
        let (sender, mut receiver) = channel();
        let mut core = PrivateScreenCore::new(RecordingOps::default());

        core.handle_event(
            PrivateScreenEvent::Remote(PrivateScreenCommand::Show("conn-1".to_string())),
            &noop_escape(),
            &sender,
        );
        core.handle_event(
            PrivateScreenEvent::Remote(PrivateScreenCommand::Hide("conn-2".to_string())),
            &noop_escape(),
            &sender,
        );

        assert_eq!(
            core.controlled_by_connection_id.as_deref(),
            Some("conn-1"),
            "the owner keeps the private screen"
        );
        assert_eq!(
            visible_changes(&drain(&mut receiver)),
            vec![("conn-1".to_string(), true)]
        );
    }

    #[test]
    fn a_foreign_connection_cannot_take_over_the_private_screen() {
        let (sender, _receiver) = channel();
        let mut core = PrivateScreenCore::new(RecordingOps::default());

        core.handle_event(
            PrivateScreenEvent::Remote(PrivateScreenCommand::Show("conn-1".to_string())),
            &noop_escape(),
            &sender,
        );
        let steps_after_owner = core.ops.steps().len();
        core.handle_event(
            PrivateScreenEvent::Remote(PrivateScreenCommand::Show("conn-2".to_string())),
            &noop_escape(),
            &sender,
        );

        assert_eq!(core.controlled_by_connection_id.as_deref(), Some("conn-1"));
        assert_eq!(core.ops.steps().len(), steps_after_owner);
    }

    #[test]
    fn quit_closes_the_active_session_and_stops_the_loop() {
        let (sender, mut receiver) = channel();
        let mut core = PrivateScreenCore::new(RecordingOps::default());

        core.handle_event(
            PrivateScreenEvent::Remote(PrivateScreenCommand::Show("conn-1".to_string())),
            &noop_escape(),
            &sender,
        );
        let control = core.handle_event(
            PrivateScreenEvent::Remote(PrivateScreenCommand::Quit),
            &noop_escape(),
            &sender,
        );

        assert_eq!(control, LoopControl::Stop);
        assert!(core.controlled_by_connection_id.is_none());
        assert_eq!(
            visible_changes(&drain(&mut receiver)),
            vec![("conn-1".to_string(), true), ("conn-1".to_string(), false)]
        );
    }

    /// Teardown order matters: the hotkey and the interception go away before
    /// the window, so no input is left blocked after the overlay is gone.
    #[test]
    fn hide_tears_down_in_reverse_order() {
        let (sender, _receiver) = channel();
        let mut core = PrivateScreenCore::new(RecordingOps::default());

        core.handle_event(
            PrivateScreenEvent::Remote(PrivateScreenCommand::Show("conn-1".to_string())),
            &noop_escape(),
            &sender,
        );
        core.handle_event(
            PrivateScreenEvent::Remote(PrivateScreenCommand::Hide("conn-1".to_string())),
            &noop_escape(),
            &sender,
        );

        assert_eq!(
            core.ops.steps(),
            vec![
                Step::AdoptPolicy,
                Step::Prepare,
                Step::StartInterception,
                Step::RegisterHotkey,
                Step::Present,
                Step::UnregisterHotkey,
                Step::StopInterception,
                Step::Close,
                Step::RestorePolicy,
            ]
        );
    }

    /// The escape callback must only hand the intent to the loop: it runs on
    /// the interception thread that the loop tears down.
    #[test]
    fn escape_callback_only_sends_and_survives_a_closed_loop() {
        let (event_sender, event_receiver) = mpsc::channel::<PrivateScreenEvent>();
        let callback: LocalEscapeCallback = Arc::new(move || {
            let _ = event_sender.send(PrivateScreenEvent::LocalDismiss);
        });

        callback();
        assert!(matches!(
            event_receiver.try_recv(),
            Ok(PrivateScreenEvent::LocalDismiss)
        ));

        drop(event_receiver);
        // A dismissal arriving after the loop is gone must not panic the
        // interception thread.
        callback();
    }
}
