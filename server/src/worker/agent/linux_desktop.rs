//! GNOME Wayland identity and desktop integration live outside shared dispatch.
pub(crate) mod atspi;
mod identity;
pub(crate) mod local_control;
mod monitor;
pub(crate) mod monitor_owner;
pub(crate) mod output_input;
pub(crate) mod portal_startup;
pub(crate) mod turn_monitor;

pub(crate) use identity::{DesktopIdentity, resolve};
pub(crate) use monitor::DesktopMonitor;

pub(super) fn desktop_observation(
    identity: &DesktopIdentity,
) -> super::computer_use_broker::ObservedDesktop {
    super::computer_use_broker::ObservedDesktop {
        session_id: identity.session_id.clone(),
        foreground_application: None,
    }
}
