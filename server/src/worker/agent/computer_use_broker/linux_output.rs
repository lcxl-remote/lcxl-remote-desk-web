//! Linux output references retain the original observation and Portal owner.
use super::*;
use crate::worker::agent::{
    collectors::screen_capture::wayland::CachedFrame, linux_desktop::DesktopIdentity,
};
use desk_agent_protocol::{
    ScreenCaptureOutput, ScreenFrameFreshness,
    computer_use::{OutputFrameBinding, RawInputScreenContext, WaylandOutputInputAction},
};

impl ComputerUseBroker {
    /// Serialize worker replacement with frame publication, and stop the old
    /// native capture service before the shared broker advances its incarnation.
    pub(super) fn fence_wayland_worker_reset(
        &self,
    ) -> (
        std::sync::MutexGuard<'_, u64>,
        std::sync::MutexGuard<'_, Option<DesktopIdentity>>,
    ) {
        // Match monitor publication lock order: owner, then desktop identity.
        let owner = self.linux_monitor_owner.invalidate();
        crate::worker::agent::linux_desktop::atspi::lifetime::clear();
        let identity = self
            .linux_desktop_identity
            .lock()
            .expect("Linux desktop identity lock");
        self.revoke_linux_input_control();
        self.clear_wayland_frame();
        (owner, identity)
    }

    pub(crate) fn wayland_capture_generation(&self) -> u64 {
        self.worker_generation.load(Ordering::SeqCst)
    }

    pub(crate) fn preflight_wayland_output(
        &self,
        target: &ObjectRef,
        action: &WaylandOutputInputAction,
        ceiling: &ComputerUseSettings,
    ) -> Result<(), AgentError> {
        if !ceiling.observation_enabled() {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "Computer Use observation is disabled",
                false,
            ));
        }
        if !self.input_ownership_is_ready() {
            return Err(error(
                AgentErrorKind::SessionUnavailable,
                "Wayland input requires an active locally authorized best-effort input control period",
                false,
            ));
        }
        let frame = self.resolve_wayland_output(target, action)?;
        let observation = frame
            .output
            .frame_observation
            .as_ref()
            .ok_or_else(unavailable)?;
        // Best effort uses the original local receipt clock, never source PTS
        // or a caller-supplied wall clock. Re-encoding cannot renew this age.
        require_frame_age(frame.received_at, std::time::Instant::now())?;
        let portal = self.portal().ok_or_else(unavailable)?;
        if portal.subscribe().borrow().generation != observation.stream_generation {
            return Err(unavailable());
        }
        crate::worker::agent::linux_desktop::output_input::events(
            action,
            frame.session.stream().size.ok_or_else(unavailable)?,
        )
        .map_err(|_| unavailable())?;
        Ok(())
    }

    pub(super) fn wayland_output_ready(
        &self,
        ceiling: &ComputerUseSettings,
        allow_screen: bool,
    ) -> bool {
        if !allow_screen || !ceiling.observation_enabled() || !self.input_ownership_is_ready() {
            return false;
        }
        let Some(frame) = self
            .wayland_frame
            .lock()
            .ok()
            .and_then(|value| value.clone())
        else {
            return false;
        };
        let Some(observation) = frame.output.frame_observation.as_ref() else {
            return false;
        };
        let Some(target) = observation.output_reference.as_ref() else {
            return false;
        };
        let action = WaylandOutputInputAction {
            screen: RawInputScreenContext {
                display: frame.output.display.clone(),
                width: frame.output.width,
                height: frame.output.height,
                dpi_x: frame.output.dpi_x,
                dpi_y: frame.output.dpi_y,
            },
            frame: OutputFrameBinding {
                observation_id: observation.observation_id.clone(),
                stream_generation: observation.stream_generation,
                received_at_unix_ms: observation.received_at_unix_ms,
                freshness: observation.freshness,
            },
            step: desk_agent_protocol::computer_use::RawInputStep::KeyPress {
                key: desk_agent_protocol::computer_use::RawInputKey::Escape,
            },
        };
        self.preflight_wayland_output(target, &action, ceiling)
            .is_ok()
    }

    pub(crate) fn publish_wayland_frame(
        &self,
        identity: &DesktopIdentity,
        worker_generation: u64,
        mut frame: CachedFrame,
    ) -> Result<ScreenCaptureOutput, AgentError> {
        let current = self
            .linux_desktop_identity
            .lock()
            .map_err(|_| unavailable())?;
        // Compare under the same identity lock used by desktop revocation:
        // observing the same identity after unlock must not revive an old read.
        if self.wayland_capture_generation() != worker_generation
            || current.as_ref() != Some(identity)
            || frame.session.closure_token().is_cancelled()
            || !frame.continuity.is_current()
        {
            return Err(unavailable());
        }
        let observation = frame
            .output
            .frame_observation
            .as_ref()
            .ok_or_else(unavailable)?;
        let id = observation.observation_id.clone();
        let mut cache = self.wayland_frame.lock().map_err(|_| unavailable())?;
        // Only one live frame is retained; a new observation invalidates old
        // output references instead of accumulating lifecycle-bound objects.
        self.objects
            .lock()
            .map_err(|_| unavailable())?
            .retain(|_, entry| !matches!(entry.resolved, ResolvedObject::WaylandOutput { .. }));
        let reference = self.issue_ref(
            &self.next_snapshot_id(),
            &identity.binding(),
            ObjectKind::DesktopOutput,
            ResolvedObject::WaylandOutput { observation_id: id },
        )?;
        let observation = frame
            .output
            .frame_observation
            .as_mut()
            .ok_or_else(unavailable)?;
        observation.output_reference = Some(reference);
        crate::worker::agent::collectors::screen_capture::wayland::refresh_receipt_age(
            observation,
            frame.received_at,
        );
        let output = frame.output.clone();
        *cache = Some(frame);
        // A usable observation can make output input ready for only 30 seconds;
        // do not spend that window waiting for the periodic readiness tick.
        self.readiness_changed.notify_one();
        Ok(output)
    }

    /// The reference must still resolve to the single original retained frame.
    /// Caller-supplied frame labels, geometry, and freshness never create proof.
    pub(crate) fn resolve_wayland_output(
        &self,
        target: &ObjectRef,
        action: &WaylandOutputInputAction,
    ) -> Result<CachedFrame, AgentError> {
        action.validate().map_err(|_| unavailable())?;
        let ResolvedObject::WaylandOutput { observation_id } = self.resolve_ref(target)? else {
            return Err(unavailable());
        };
        let frame = self
            .wayland_frame
            .lock()
            .map_err(|_| unavailable())?
            .as_ref()
            .cloned()
            .ok_or_else(unavailable)?;
        let observation = frame
            .output
            .frame_observation
            .as_ref()
            .ok_or_else(unavailable)?;
        let expected = OutputFrameBinding {
            observation_id: observation.observation_id.clone(),
            stream_generation: observation.stream_generation,
            received_at_unix_ms: observation.received_at_unix_ms,
            freshness: observation.freshness,
        };
        let geometry = RawInputScreenContext {
            display: frame.output.display.clone(),
            width: frame.output.width,
            height: frame.output.height,
            dpi_x: frame.output.dpi_x,
            dpi_y: frame.output.dpi_y,
        };
        if observation_id != observation.observation_id
            || action.frame != expected
            || frame.input_epoch != self.human_input_epoch()
            || action.screen != geometry
            || observation.output_reference.as_ref() != Some(target)
            || !matches!(
                observation.freshness,
                ScreenFrameFreshness::Fresh
                    | ScreenFrameFreshness::UnchangedVerified
                    | ScreenFrameFreshness::LatestObserved
            )
            || frame.session.closure_token().is_cancelled()
            || !frame.continuity.is_current()
            || self
                .linux_desktop_identity()
                .map(|identity| identity.binding())
                .as_ref()
                != Some(&frame.identity)
        {
            return Err(unavailable());
        }
        let current = self
            .portal()
            .ok_or_else(unavailable)?
            .try_borrow_session(true)
            .map_err(|_| unavailable())?;
        if !Arc::ptr_eq(&current, &frame.session) {
            return Err(unavailable());
        }
        Ok(frame)
    }
}

const MAX_INPUT_FRAME_AGE: std::time::Duration = std::time::Duration::from_secs(30);

fn require_frame_age(
    received: std::time::Instant,
    now: std::time::Instant,
) -> Result<(), AgentError> {
    match now.checked_duration_since(received) {
        Some(age) if age <= MAX_INPUT_FRAME_AGE => Ok(()),
        _ => Err(unavailable()),
    }
}

fn unavailable() -> AgentError {
    error(
        AgentErrorKind::SessionUnavailable,
        "The original Wayland output/frame is unavailable or exceeds the best-effort age limit",
        false,
    )
}

#[cfg(test)]
mod capture_generation_tests {
    use super::*;

    #[test]
    fn original_receipt_age_is_bounded_and_future_clocks_are_rejected() {
        let received = std::time::Instant::now();
        assert!(require_frame_age(received, received).is_ok());
        assert!(require_frame_age(received, received + MAX_INPUT_FRAME_AGE).is_ok());
        assert!(
            require_frame_age(
                received,
                received + MAX_INPUT_FRAME_AGE + std::time::Duration::from_nanos(1)
            )
            .is_err()
        );
        assert!(
            require_frame_age(received + std::time::Duration::from_nanos(1), received).is_err()
        );
    }

    #[test]
    fn worker_reset_waits_for_publication_and_advances_generation() {
        let broker = Arc::new(ComputerUseBroker::new());
        let captured = broker.wayland_capture_generation();
        let old_monitor = broker.linux_monitor_owner.claim();
        let publication = broker.linux_desktop_identity.lock().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = broker.clone();
        let reset = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            worker.reset_worker_incarnation();
            done_tx.send(()).unwrap();
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let premature = done_rx.recv_timeout(std::time::Duration::from_millis(50));
        let while_locked = broker.wayland_capture_generation();
        drop(publication);
        done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        reset.join().unwrap();
        assert!(!old_monitor.current());
        assert!(matches!(
            premature,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        assert_eq!(while_locked, captured);
        assert_ne!(broker.wayland_capture_generation(), captured);
    }

    #[test]
    fn desktop_revocation_and_return_do_not_revive_capture_generation() {
        let broker = ComputerUseBroker::new();
        let identity = DesktopIdentity {
            uid: 1000,
            session_id: "fixture".into(),
            session_path: "/org/freedesktop/login1/session/fixture"
                .try_into()
                .unwrap(),
            runtime_path: "/fixture".into(),
            socket_path: "/fixture/wayland-0".into(),
            socket_device: 1,
            socket_inode: 2,
            compositor_pid: 3,
            compositor_start: 4,
            bus_id: "fixture-bus".into(),
            shell_owner: ":1.1".into(),
            logind_owner: ":1.2".into(),
        };
        broker.set_linux_desktop_identity(Some(identity.clone()));
        let captured = broker.wayland_capture_generation();
        broker.set_linux_desktop_identity(Some(identity.clone()));
        assert_eq!(broker.wayland_capture_generation(), captured);
        broker.set_linux_desktop_identity(None);
        broker.set_linux_desktop_identity(Some(identity.clone()));
        assert_eq!(broker.linux_desktop_identity(), Some(identity));
        assert_ne!(broker.wayland_capture_generation(), captured);
    }
}
