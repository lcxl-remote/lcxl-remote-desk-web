//! Structural checks for the bilingual policy evaluation fixture.

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use desk_agent_protocol::content_safety::{
        ContentSafetyCategory, ContentSafetyDecision, ContentSafetyStage,
    };
    use serde::Deserialize;

    use crate::content_safety::policy::aggregate_decision;

    #[derive(Deserialize)]
    struct EvalCase {
        id: String,
        locale: String,
        text: String,
        decision: ContentSafetyDecision,
        #[serde(default)]
        categories: Vec<ContentSafetyCategory>,
        stage: ContentSafetyStage,
    }

    #[test]
    fn bilingual_fixture_covers_every_category_and_technical_allow_boundaries() {
        let cases: Vec<EvalCase> =
            serde_json::from_str(include_str!("fixtures/policy_v1_bilingual.json"))
                .expect("fixture JSON");
        assert!(!cases.is_empty());
        assert!(cases.iter().any(|case| case.locale == "zh-CN"));
        assert!(cases.iter().any(|case| case.locale == "en-US"));
        assert!(
            cases
                .iter()
                .all(|case| !case.id.is_empty() && !case.text.is_empty())
        );
        assert!(
            cases
                .iter()
                .all(|case| case.stage == ContentSafetyStage::Input)
        );

        let covered: HashSet<_> = cases
            .iter()
            .flat_map(|case| case.categories.iter().copied())
            .collect();
        let required = [
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
        ];
        assert!(
            required
                .into_iter()
                .all(|category| covered.contains(&category))
        );

        for case in &cases {
            assert_eq!(
                aggregate_decision(case.categories.iter().copied()),
                case.decision
            );
        }
        assert!(cases.iter().any(|case| {
            case.decision == ContentSafetyDecision::Allow && case.text.contains("leader election")
        }));
        assert!(cases.iter().any(|case| {
            case.decision == ContentSafetyDecision::Allow && case.text.contains("TLS")
        }));
    }
}
