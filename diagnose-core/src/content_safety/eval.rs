//! Frozen, provider-neutral policy evaluation fixture.
//!
//! The fixture is runtime-readable so the manager's one-off shadow-eval CLI can
//! exercise the exact production classifier seam. The OSS signal never invokes
//! it and gains no provider or enforcement path.

use std::collections::{BTreeMap, BTreeSet};

use desk_agent_protocol::content_safety::{
    ContentSafetyCategory, ContentSafetyDecision, ContentSafetyStage,
};
use serde::{Deserialize, Serialize};

use super::CONTENT_SAFETY_PROMPT_VERSION;

pub const POLICY_EVAL_FIXTURE_VERSION: &str = "policy-v1-bilingual";
pub const REQUIRED_ADVERSARIAL_VARIANTS: [&str; 7] = [
    "direct",
    "euphemistic",
    "role_play",
    "transformation",
    "multi_turn",
    "obfuscated",
    "automation",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PolicyEvalCase {
    pub id: String,
    pub locale: String,
    pub text: String,
    pub decision: ContentSafetyDecision,
    pub categories: Vec<ContentSafetyCategory>,
    pub stage: ContentSafetyStage,
    pub variant: String,
}

#[derive(Debug, Deserialize)]
struct EvalFixture {
    fixture_version: String,
    prompt_version: String,
    category_groups: Vec<CategoryEvalGroup>,
    allow_cases: Vec<AllowEvalCase>,
}

#[derive(Debug, Deserialize)]
struct CategoryEvalGroup {
    id: String,
    locale: String,
    decision: ContentSafetyDecision,
    categories: Vec<ContentSafetyCategory>,
    stage: ContentSafetyStage,
    variants: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct AllowEvalCase {
    id: String,
    locale: String,
    text: String,
    stage: ContentSafetyStage,
    boundary: String,
}

/// Parse and validate the frozen bilingual fixture, then flatten its grouped
/// adversarial cases into the exact inputs sent to the classifier.
pub fn policy_v1_bilingual_cases() -> Result<Vec<PolicyEvalCase>, String> {
    let fixture: EvalFixture =
        serde_json::from_str(include_str!("fixtures/policy_v1_bilingual.json"))
            .map_err(|error| format!("invalid policy evaluation fixture JSON: {error}"))?;
    validate_fixture(&fixture)?;

    let mut cases = Vec::new();
    for group in fixture.category_groups {
        for variant in REQUIRED_ADVERSARIAL_VARIANTS {
            let text = group
                .variants
                .get(variant)
                .expect("validated variant exists")
                .clone();
            cases.push(PolicyEvalCase {
                id: format!("{}-{variant}", group.id),
                locale: group.locale.clone(),
                text,
                decision: group.decision,
                categories: group.categories.clone(),
                stage: group.stage,
                variant: variant.to_string(),
            });
        }
    }
    cases.extend(fixture.allow_cases.into_iter().map(|case| PolicyEvalCase {
        id: case.id,
        locale: case.locale,
        text: case.text,
        decision: ContentSafetyDecision::Allow,
        categories: Vec::new(),
        stage: case.stage,
        variant: case.boundary,
    }));
    Ok(cases)
}

fn validate_fixture(fixture: &EvalFixture) -> Result<(), String> {
    if fixture.fixture_version != POLICY_EVAL_FIXTURE_VERSION {
        return Err(format!(
            "fixture_version must be {POLICY_EVAL_FIXTURE_VERSION}"
        ));
    }
    if fixture.prompt_version != CONTENT_SAFETY_PROMPT_VERSION {
        return Err(format!(
            "prompt_version must be {CONTENT_SAFETY_PROMPT_VERSION}"
        ));
    }
    if fixture.category_groups.is_empty() || fixture.allow_cases.is_empty() {
        return Err("category_groups and allow_cases must be non-empty".into());
    }
    for group in &fixture.category_groups {
        if group.id.is_empty() || group.locale.is_empty() || group.categories.is_empty() {
            return Err("category group identity and categories must be non-empty".into());
        }
        if group.stage != ContentSafetyStage::Input {
            return Err(format!("{} must use input stage", group.id));
        }
        let actual: BTreeSet<_> = group.variants.keys().map(String::as_str).collect();
        let required: BTreeSet<_> = REQUIRED_ADVERSARIAL_VARIANTS.into_iter().collect();
        if actual != required || group.variants.len() != REQUIRED_ADVERSARIAL_VARIANTS.len() {
            return Err(format!(
                "{} must contain exactly the seven frozen adversarial variants",
                group.id
            ));
        }
        if group.variants.values().any(|text| text.trim().is_empty()) {
            return Err(format!("{} contains an empty adversarial input", group.id));
        }
    }
    for case in &fixture.allow_cases {
        if case.id.is_empty()
            || case.locale.is_empty()
            || case.text.trim().is_empty()
            || case.boundary.is_empty()
            || case.stage != ContentSafetyStage::Input
        {
            return Err("allow case fields must be non-empty and use input stage".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::content_safety::policy::aggregate_decision;

    #[test]
    fn bilingual_fixture_covers_every_category_and_adversarial_variant() {
        let cases = policy_v1_bilingual_cases().expect("fixture");
        assert_eq!(cases.len(), 11 * REQUIRED_ADVERSARIAL_VARIANTS.len() + 11);
        assert!(cases.iter().any(|case| case.locale == "zh-CN"));
        assert!(cases.iter().any(|case| case.locale == "en-US"));

        let required_categories = HashSet::from([
            ContentSafetyCategory::Sexual,
            ContentSafetyCategory::SexualMinors,
            ContentSafetyCategory::Violence,
            ContentSafetyCategory::GraphicViolence,
            ContentSafetyCategory::ViolentWrongdoing,
            ContentSafetyCategory::Hate,
            ContentSafetyCategory::ThreateningHarassment,
            ContentSafetyCategory::SelfHarm,
            ContentSafetyCategory::SelfHarmInstructions,
            ContentSafetyCategory::Illicit,
            ContentSafetyCategory::Politics,
        ]);
        let covered_categories: HashSet<_> = cases
            .iter()
            .flat_map(|case| case.categories.iter().copied())
            .collect();
        assert_eq!(covered_categories, required_categories);

        for category in required_categories {
            let variants: HashSet<_> = cases
                .iter()
                .filter(|case| case.categories.contains(&category))
                .map(|case| case.variant.as_str())
                .collect();
            assert_eq!(variants, HashSet::from(REQUIRED_ADVERSARIAL_VARIANTS));
        }
        for case in &cases {
            assert_eq!(
                aggregate_decision(case.categories.iter().copied()),
                case.decision,
                "{}",
                case.id
            );
        }
    }

    #[test]
    fn allow_boundaries_cover_the_required_technical_and_contextual_cases() {
        let cases = policy_v1_bilingual_cases().expect("fixture");
        let boundaries: HashSet<_> = cases
            .iter()
            .filter(|case| case.decision == ContentSafetyDecision::Allow)
            .map(|case| case.variant.as_str())
            .collect();
        for required in [
            "process_control",
            "leader_election",
            "network_defense",
            "vulnerability_defense",
            "government_tls",
            "incidental_political_log",
            "medical_education",
        ] {
            assert!(boundaries.contains(required), "missing {required}");
        }
    }
}
