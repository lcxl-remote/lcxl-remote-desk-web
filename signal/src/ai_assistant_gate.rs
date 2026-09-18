//! Process-local projection of the device-owned AI Assistant switch.
//!
//! The durable authority lives in the host settings file. The embedded OSS
//! signal reads this projection only to reject new work without an async lock;
//! it never persists or increments the revision itself.

use std::sync::{Arc, OnceLock, RwLock};

use desk_agent_protocol::ai_assistant::AiAssistantSettings;

#[derive(Debug, Default)]
pub struct AiAssistantGate {
    state: RwLock<AiAssistantSettings>,
}

impl AiAssistantGate {
    pub fn new(state: AiAssistantSettings) -> Self {
        Self {
            state: RwLock::new(state),
        }
    }

    pub fn snapshot(&self) -> AiAssistantSettings {
        *self.state.read().expect("AI assistant gate")
    }

    pub fn is_enabled(&self) -> bool {
        self.snapshot().enabled
    }

    /// Replace the runtime projection after the device has durably committed
    /// this exact snapshot.
    pub fn replace(&self, state: AiAssistantSettings) {
        *self.state.write().expect("AI assistant gate") = state;
    }
}

pub fn global_ai_assistant_gate() -> Arc<AiAssistantGate> {
    static GATE: OnceLock<Arc<AiAssistantGate>> = OnceLock::new();
    GATE.get_or_init(|| Arc::new(AiAssistantGate::default()))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_fail_closed_and_replaces_exact_snapshot() {
        let gate = AiAssistantGate::default();
        assert!(!gate.is_enabled());
        assert_eq!(gate.snapshot().revision, 0);

        gate.replace(AiAssistantSettings {
            revision: 4,
            enabled: true,
        });
        assert!(gate.is_enabled());
        assert_eq!(gate.snapshot().revision, 4);
    }
}
