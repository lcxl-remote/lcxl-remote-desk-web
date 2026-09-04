//! Process-local projection of the device-owned Device Assistant switch.
//!
//! The durable authority lives in the host settings file. The embedded OSS
//! signal reads this projection only to reject new work without an async lock;
//! it never persists or increments the revision itself.

use std::sync::{Arc, OnceLock, RwLock};

use desk_agent_protocol::device_assistant::DeviceAssistantSettings;

#[derive(Debug, Default)]
pub struct DeviceAssistantGate {
    state: RwLock<DeviceAssistantSettings>,
}

impl DeviceAssistantGate {
    pub fn new(state: DeviceAssistantSettings) -> Self {
        Self {
            state: RwLock::new(state),
        }
    }

    pub fn snapshot(&self) -> DeviceAssistantSettings {
        *self.state.read().expect("device assistant gate")
    }

    pub fn is_enabled(&self) -> bool {
        self.snapshot().enabled
    }

    /// Replace the runtime projection after the device has durably committed
    /// this exact snapshot.
    pub fn replace(&self, state: DeviceAssistantSettings) {
        *self.state.write().expect("device assistant gate") = state;
    }
}

pub fn global_device_assistant_gate() -> Arc<DeviceAssistantGate> {
    static GATE: OnceLock<Arc<DeviceAssistantGate>> = OnceLock::new();
    GATE.get_or_init(|| Arc::new(DeviceAssistantGate::default()))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_fail_closed_and_replaces_exact_snapshot() {
        let gate = DeviceAssistantGate::default();
        assert!(!gate.is_enabled());
        assert_eq!(gate.snapshot().revision, 0);

        gate.replace(DeviceAssistantSettings {
            revision: 4,
            enabled: true,
        });
        assert!(gate.is_enabled());
        assert_eq!(gate.snapshot().revision, 4);
    }
}
