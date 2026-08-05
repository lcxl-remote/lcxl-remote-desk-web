//! Stable, non-model-generated refusal reason keys.

use desk_agent_protocol::content_safety::ContentSafetyDecision;

/// Stable key selected by server policy and localized by the control end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalReasonKey {
    Blocked,
    SafeRedirect,
}

impl RefusalReasonKey {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blocked => "content_safety.blocked",
            Self::SafeRedirect => "content_safety.safe_redirect",
        }
    }
}

/// Return a fixed reason key for policy decisions. `Allow` has no refusal.
pub const fn refusal_reason_for(decision: ContentSafetyDecision) -> Option<RefusalReasonKey> {
    match decision {
        ContentSafetyDecision::Allow => None,
        ContentSafetyDecision::Block => Some(RefusalReasonKey::Blocked),
        ContentSafetyDecision::SafeRedirect => Some(RefusalReasonKey::SafeRedirect),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_stable_and_allow_has_no_refusal() {
        assert_eq!(
            refusal_reason_for(ContentSafetyDecision::Block)
                .unwrap()
                .as_str(),
            "content_safety.blocked"
        );
        assert_eq!(
            refusal_reason_for(ContentSafetyDecision::SafeRedirect)
                .unwrap()
                .as_str(),
            "content_safety.safe_redirect"
        );
        assert!(refusal_reason_for(ContentSafetyDecision::Allow).is_none());
    }
}
