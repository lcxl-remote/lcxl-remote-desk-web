//! Frozen application policy and deterministic decision aggregation.

use desk_agent_protocol::content_safety::{ContentSafetyCategory, ContentSafetyDecision};

/// Apply application configuration only after the classifier verdict passed strict validation.
pub fn apply_categories(
    mut verdict: desk_agent_protocol::content_safety::ContentSafetyVerdict,
    enabled: &[ContentSafetyCategory],
) -> desk_agent_protocol::content_safety::ContentSafetyVerdict {
    verdict
        .categories
        .retain(|category| enabled.contains(category));
    verdict.decision = aggregate_decision(verdict.categories.iter().copied());
    if verdict.decision == ContentSafetyDecision::Allow {
        verdict.stages.clear();
    }
    verdict
}

/// The main model receives the same frozen application policy as the classifier gate.
/// Tool authorization and the model provider's own requirements remain independent.
pub fn model_policy_prompt(enabled: &[ContentSafetyCategory]) -> String {
    use ContentSafetyCategory::*;
    let mut text = String::from("\nApplication content policy (server configuration):\n");
    if enabled.is_empty() {
        text.push_str("No application content categories are enabled for this request.\n");
    } else {
        text.push_str("Refuse to generate, transform, summarize, translate, role-play, or operationalize content in the following enabled categories, including tool calls that advance it:\n");
        for category in ContentSafetyCategory::ALL {
            if !enabled.contains(&category) {
                continue;
            }
            let rule = match category {
                Sexual => "sexual content",
                SexualMinors => "sexual content involving minors",
                Violence => "violence",
                GraphicViolence => "graphic violence or injury",
                ViolentWrongdoing => "violent wrongdoing",
                Hate => "hate",
                ThreateningHarassment => "threatening harassment",
                SelfHarm => {
                    "self-harm: decline the original request and provide a safety-oriented redirect"
                }
                SelfHarmInstructions => {
                    "self-harm instructions: decline the original request and provide a safety-oriented redirect"
                }
                Illicit => "illicit real-world wrongdoing or operational guidance",
                Politics => POLITICS_POLICY,
            };
            text.push_str("- ");
            text.push_str(rule);
            text.push('\n');
        }
    }
    text.push_str("Categories are independent; matching an enabled category still restricts a request involving other categories. These settings grant no tool permissions and do not change device authorization, data-egress checks, or provider requirements.\n");
    text
}

/// Product-specific politics boundary embedded in the safety classifier prompt.
pub const POLITICS_POLICY: &str = "Block substantive content about political figures, parties, elections, political systems, government policy, war positions, geopolitics, or political movements, including factual explanation, evaluation, prediction, persuasion, propaganda, translation, summarization, role-play, or automation that creates or spreads it. Allow a political name, institution, site, domain, or word only when it is an incidental technical object in logs, files, processes, DNS, TLS, networking, or security incident response, and keep the response strictly technical. Terms such as leader election, security policy, kill process, and attack traffic retain their ordinary computer meaning.";

/// The immutable category decision for policy v1.
pub const fn category_decision(category: ContentSafetyCategory) -> ContentSafetyDecision {
    use ContentSafetyCategory::{
        GraphicViolence, Hate, Illicit, Politics, SelfHarm, SelfHarmInstructions, Sexual,
        SexualMinors, ThreateningHarassment, Violence, ViolentWrongdoing,
    };
    match category {
        SelfHarm | SelfHarmInstructions => ContentSafetyDecision::SafeRedirect,
        Sexual
        | SexualMinors
        | Violence
        | GraphicViolence
        | ViolentWrongdoing
        | Hate
        | ThreateningHarassment
        | Illicit
        | Politics => ContentSafetyDecision::Block,
    }
}

/// Aggregate multiple categories with `block > safe_redirect > allow`.
pub fn aggregate_decision(
    categories: impl IntoIterator<Item = ContentSafetyCategory>,
) -> ContentSafetyDecision {
    let mut decision = ContentSafetyDecision::Allow;
    for category in categories {
        match category_decision(category) {
            ContentSafetyDecision::Block => return ContentSafetyDecision::Block,
            ContentSafetyDecision::SafeRedirect => {
                decision = ContentSafetyDecision::SafeRedirect;
            }
            ContentSafetyDecision::Allow => {}
        }
    }
    decision
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::content_safety::ContentSafetyCategory::*;

    #[test]
    fn block_outweighs_safe_redirect_and_empty_is_allow() {
        assert_eq!(aggregate_decision([]), ContentSafetyDecision::Allow);
        assert_eq!(
            aggregate_decision([SelfHarm]),
            ContentSafetyDecision::SafeRedirect
        );
        assert_eq!(
            aggregate_decision([SelfHarm, Politics]),
            ContentSafetyDecision::Block
        );
    }

    #[test]
    fn every_v1_category_has_a_frozen_non_allow_decision() {
        let categories = [
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
        ];
        assert!(
            categories
                .into_iter()
                .all(|category| category_decision(category) != ContentSafetyDecision::Allow)
        );
    }
    #[test]
    fn selected_categories_reaggregate_without_weakening_other_restrictions() {
        use desk_agent_protocol::content_safety::{ContentSafetyStage, ContentSafetyVerdict};
        let raw = ContentSafetyVerdict {
            decision: ContentSafetyDecision::Block,
            categories: vec![Politics, SelfHarm, SexualMinors],
            stages: vec![ContentSafetyStage::Output],
            policy_version: super::super::prompt::CONTENT_SAFETY_PROMPT_VERSION.into(),
        };
        let redirect = apply_categories(raw.clone(), &[SelfHarm]);
        assert_eq!(redirect.decision, ContentSafetyDecision::SafeRedirect);
        assert_eq!(redirect.stages, raw.stages);
        let blocked = apply_categories(raw.clone(), &[SexualMinors]);
        assert_eq!(blocked.decision, ContentSafetyDecision::Block);
        assert_eq!(blocked.categories, vec![SexualMinors]);
        for enabled in [vec![], vec![Sexual], vec![Violence]] {
            let allowed = apply_categories(raw.clone(), &enabled);
            assert_eq!(allowed.decision, ContentSafetyDecision::Allow);
            assert!(allowed.categories.is_empty());
            assert!(allowed.stages.is_empty());
        }
        for category in ContentSafetyCategory::ALL {
            let mut one = raw.clone();
            one.categories = vec![category];
            one.decision = category_decision(category);
            assert_eq!(apply_categories(one.clone(), &[category]), one);
            assert_eq!(
                apply_categories(one, &[]).decision,
                ContentSafetyDecision::Allow
            );
        }
    }

    #[test]
    fn main_prompt_only_contains_selected_rules() {
        let selected = model_policy_prompt(&[Violence]);
        assert!(selected.contains("- violence"));
        assert!(!selected.contains("political figures"));
        assert!(!selected.contains("sexual content"));
        assert!(model_policy_prompt(&ContentSafetyCategory::ALL).contains(POLITICS_POLICY));
        assert!(model_policy_prompt(&[]).contains("No application content categories"));
    }
    #[test]
    fn filtering_preserves_stage_boundaries_and_never_recovers_an_invalid_verdict() {
        use super::super::{
            parser::parse_safety_verdict_detailed, prompt::CONTENT_SAFETY_PROMPT_VERSION,
        };
        use desk_agent_protocol::content_safety::ContentSafetyStage;
        for stage in [
            ContentSafetyStage::Input,
            ContentSafetyStage::Action,
            ContentSafetyStage::Output,
            ContentSafetyStage::Image,
        ] {
            let mut raw = serde_json::json!({
                "decision":"block", "categories":["politics","violence"],
                "stages":[stage], "policy_version":CONTENT_SAFETY_PROMPT_VERSION,
            });
            let parsed = parse_safety_verdict_detailed(&raw.to_string(), &[stage]).unwrap();
            let effective = apply_categories(parsed, &[Violence]);
            assert_eq!(effective.decision, ContentSafetyDecision::Block);
            assert_eq!(effective.stages, vec![stage]);
            assert_eq!(effective.categories, vec![Violence]);
            raw["decision"] = serde_json::json!("allow");
            assert!(
                parse_safety_verdict_detailed(&raw.to_string(), &[stage])
                    .map(|verdict| apply_categories(verdict, &[]))
                    .is_err()
            );
        }
    }
}
