//! Closed, server-owned Capability Provider risk classifier.

use desk_agent_protocol::{
    capability_grant::CapabilityRiskTier, capability_provider::CapabilityEffect,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CapabilityRiskSignals {
    /// Read scope contains private/sensitive user content rather than a bounded,
    /// already attached non-sensitive projection.
    pub sensitive_content: bool,
    /// Call sends any derived/local bytes to a model, web or connector sink.
    pub external_egress: bool,
    /// Call overwrites/deletes an existing object or otherwise lacks safe create-new
    /// semantics.
    pub destructive_or_overwrite: bool,
    /// Raw UI/input fallback can take actions not represented by a semantic schema.
    pub unpredictable_input: bool,
}

/// Return the immutable minimum risk tier. Platform policy may only elevate this
/// result; Provider/model hints are deliberately absent from the API.
pub const fn classify_capability_risk(
    effect: CapabilityEffect,
    signals: CapabilityRiskSignals,
) -> CapabilityRiskTier {
    use CapabilityEffect as Effect;
    use CapabilityRiskTier as Risk;

    if signals.destructive_or_overwrite || signals.unpredictable_input {
        return Risk::R3;
    }
    match effect {
        Effect::ReadDevice => {
            if signals.sensitive_content || signals.external_egress {
                Risk::R1
            } else {
                Risk::R0
            }
        }
        Effect::ReadFile | Effect::ReadExternal | Effect::ExportData | Effect::CaptureScreen => {
            Risk::R1
        }
        Effect::WriteArtifact | Effect::MutateApplication | Effect::WriteExternalDraft => {
            if signals.external_egress {
                Risk::R3
            } else {
                Risk::R2
            }
        }
        Effect::SendExternal | Effect::InputFallback => Risk::R3,
    }
}

pub const fn elevate_risk(
    classified: CapabilityRiskTier,
    policy_floor: CapabilityRiskTier,
) -> CapabilityRiskTier {
    if (policy_floor as u8) > (classified as u8) {
        policy_floor
    } else {
        classified
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_table_is_closed_and_conservative() {
        let plain = CapabilityRiskSignals::default();
        let cases = [
            (CapabilityEffect::ReadDevice, CapabilityRiskTier::R0),
            (CapabilityEffect::ReadFile, CapabilityRiskTier::R1),
            (CapabilityEffect::ReadExternal, CapabilityRiskTier::R1),
            (CapabilityEffect::ExportData, CapabilityRiskTier::R1),
            (CapabilityEffect::WriteArtifact, CapabilityRiskTier::R2),
            (CapabilityEffect::MutateApplication, CapabilityRiskTier::R2),
            (CapabilityEffect::WriteExternalDraft, CapabilityRiskTier::R2),
            (CapabilityEffect::SendExternal, CapabilityRiskTier::R3),
            (CapabilityEffect::CaptureScreen, CapabilityRiskTier::R1),
            (CapabilityEffect::InputFallback, CapabilityRiskTier::R3),
        ];
        for (effect, expected) in cases {
            assert_eq!(classify_capability_risk(effect, plain), expected);
        }
    }

    #[test]
    fn sensitive_read_and_destructive_write_are_elevated() {
        assert_eq!(
            classify_capability_risk(
                CapabilityEffect::ReadDevice,
                CapabilityRiskSignals {
                    sensitive_content: true,
                    ..Default::default()
                },
            ),
            CapabilityRiskTier::R1
        );
        assert_eq!(
            classify_capability_risk(
                CapabilityEffect::WriteArtifact,
                CapabilityRiskSignals {
                    destructive_or_overwrite: true,
                    ..Default::default()
                },
            ),
            CapabilityRiskTier::R3
        );
    }

    #[test]
    fn policy_can_only_elevate() {
        assert_eq!(
            elevate_risk(CapabilityRiskTier::R2, CapabilityRiskTier::R0),
            CapabilityRiskTier::R2
        );
        assert_eq!(
            elevate_risk(CapabilityRiskTier::R1, CapabilityRiskTier::R3),
            CapabilityRiskTier::R3
        );
    }
}
