//! Frozen application policy and deterministic decision aggregation.

use desk_agent_protocol::content_safety::{ContentSafetyCategory, ContentSafetyDecision};

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
}
