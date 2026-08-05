//! Shared classification of Linux desktop-session environment variables.

/// Presence snapshot of the two display-server environment variables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinuxDisplayEnvironment {
    pub wayland_present: bool,
    pub x11_present: bool,
}

/// Active display server selected from a [`LinuxDisplayEnvironment`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxDisplayServer {
    Wayland,
    X11,
    Headless,
}

impl LinuxDisplayEnvironment {
    pub const fn new(wayland_present: bool, x11_present: bool) -> Self {
        Self {
            wayland_present,
            x11_present,
        }
    }

    /// Wayland takes precedence because XWayland exposes both variables.
    pub const fn active_server(self) -> LinuxDisplayServer {
        if self.wayland_present {
            LinuxDisplayServer::Wayland
        } else if self.x11_present {
            LinuxDisplayServer::X11
        } else {
            LinuxDisplayServer::Headless
        }
    }
}

/// Read the process environment once and return a coherent snapshot.
#[cfg(target_os = "linux")]
pub fn detect_linux_display_environment() -> LinuxDisplayEnvironment {
    LinuxDisplayEnvironment::new(
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
        std::env::var_os("DISPLAY").is_some(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_display_classification_covers_all_environment_combinations() {
        assert_eq!(
            LinuxDisplayEnvironment::new(true, true).active_server(),
            LinuxDisplayServer::Wayland
        );
        assert_eq!(
            LinuxDisplayEnvironment::new(true, false).active_server(),
            LinuxDisplayServer::Wayland
        );
        assert_eq!(
            LinuxDisplayEnvironment::new(false, true).active_server(),
            LinuxDisplayServer::X11
        );
        assert_eq!(
            LinuxDisplayEnvironment::new(false, false).active_server(),
            LinuxDisplayServer::Headless
        );
    }
}
