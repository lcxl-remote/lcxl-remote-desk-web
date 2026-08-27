//! Provider-neutral content-safety wire types shared by manager AI surfaces.
//!
//! These are closed enums on purpose: a provider response can never introduce a
//! new category, stage, or decision that a runtime silently treats as allowed.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

/// AI product surface whose content is being reviewed.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ContentSafetySurface {
    Diagnosis,
    TerminalCopilot,
    DeviceAssistant,
    TerminalCompletion,
    FleetDiagnosis,
    FleetExecution,
    Automation,
    ProviderProbe,
    DocumentationSupport,
}

/// Trust boundary at which a safety verdict applies.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ContentSafetyStage {
    Input,
    Action,
    Output,
    Image,
}

/// Application policy category. This is intentionally stricter than any one
/// provider's taxonomy and includes the product-specific politics category.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ContentSafetyCategory {
    Sexual,
    SexualMinors,
    Violence,
    GraphicViolence,
    ViolentWrongdoing,
    Hate,
    ThreateningHarassment,
    SelfHarm,
    SelfHarmInstructions,
    Illicit,
    Politics,
}

/// Closed business decision returned by the safety layer.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ContentSafetyDecision {
    Allow,
    Block,
    SafeRedirect,
}

/// Validated verdict. Categories and stages are empty only for `Allow`; a
/// parser must reject inconsistent combinations before constructing this value.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ContentSafetyVerdict {
    pub decision: ContentSafetyDecision,
    pub categories: Vec<ContentSafetyCategory>,
    pub stages: Vec<ContentSafetyStage>,
    pub policy_version: String,
}

/// Why provisional text was retracted. The browser selects a local, fixed
/// message from this enum and never renders a provider explanation.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum StreamRetractionReason {
    PolicyBlocked,
    SafeRedirect,
    SafetyUnavailable,
    Incomplete,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_tokens_are_stable_and_unknown_values_fail_closed() {
        assert_eq!(
            serde_json::to_string(&ContentSafetyCategory::ViolentWrongdoing).unwrap(),
            "\"violent_wrongdoing\""
        );
        assert_eq!(
            serde_json::to_string(&StreamRetractionReason::SafetyUnavailable).unwrap(),
            "\"safety_unavailable\""
        );
        assert_eq!(
            serde_json::to_string(&ContentSafetySurface::DocumentationSupport).unwrap(),
            "\"documentation_support\""
        );
        assert!(
            serde_json::from_str::<ContentSafetyDecision>("\"review\"").is_err(),
            "unknown decisions must not become allow"
        );
        assert!(
            serde_json::from_str::<ContentSafetyCategory>("\"out_of_scope\"").is_err(),
            "future categories require an explicit protocol change"
        );
    }

    #[test]
    fn verdict_rejects_unknown_fields() {
        let json = r#"{
            "decision":"allow",
            "categories":[],
            "stages":[],
            "policy_version":"content-safety-v1",
            "explanation":"provider-controlled text"
        }"#;
        assert!(serde_json::from_str::<ContentSafetyVerdict>(json).is_err());
    }
}
