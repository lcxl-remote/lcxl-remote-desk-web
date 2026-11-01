use super::*;

#[test]
fn both_host_defaults_have_no_issues() {
    assert!(assess(&AssistantDataPolicy::default()).issues.is_empty());
    assert!(assess(&AssistantDataPolicy::default()).issues.is_empty());
}

#[test]
fn warnings_are_conditional_ordered_and_clear_after_correction() {
    let mut policy = AssistantDataPolicy::default();
    policy.limits.retrieval.relevance_hits = 100;
    policy.limits.retrieval.query_candidates = 80;
    policy.limits.processing.native_status_model_items = 25;
    policy.limits.retrieval.owner_scan_documents = 10_001;
    policy.limits.memory.current_facts = 1001;
    let result = assess(&policy);
    assert!(!result.has_errors());
    assert_eq!(
        result
            .issues
            .iter()
            .map(|issue| issue.effective_limit.unwrap())
            .collect::<Vec<_>>(),
        [80, 60, 24, 10_000, 1000]
    );
    assert!(
        result
            .issues
            .iter()
            .all(|issue| issue.level == PolicyIssueLevel::Warning && !issue.fields.is_empty())
    );
    assert_eq!(result, assess(&policy));
    assert!(assess(&AssistantDataPolicy::default()).issues.is_empty());
}

#[test]
fn equal_caps_are_not_warned_and_diversity_products_do_not_overflow() {
    let mut policy = AssistantDataPolicy::default();
    policy.limits.retrieval.query_candidates = 60;
    policy.limits.retrieval.relevance_hits = 60;
    policy.limits.processing.native_status_model_items = 24;
    policy.limits.retrieval.owner_scan_documents = 10_000;
    policy.limits.memory.current_facts = 1000;
    assert!(assess(&policy).issues.is_empty());
    policy.limits.retrieval.relevance_roots = u32::MAX;
    policy.limits.retrieval.fragments_per_root = u32::MAX;
    assert!(assess(&policy).issues.is_empty());
}

#[test]
fn scalar_errors_identify_fields_and_suppress_derived_warnings() {
    let mut policy = AssistantDataPolicy::default();
    policy.limits.retrieval.query_candidates = 0;
    policy.history_retention_days = 0;
    let result = assess(&policy);
    assert!(policy.validate().is_err());
    assert!(result.has_errors());
    assert_eq!(result.issues.len(), 2);
    assert_eq!(
        result.issues[0].fields,
        ["limits.retrieval.query_candidates"]
    );
    assert!(
        result
            .issues
            .iter()
            .all(|issue| issue.level == PolicyIssueLevel::Error)
    );
}

#[test]
fn response_floor_is_a_serialized_necessary_bound_not_a_content_quota() {
    let minimum = minimum_response_bytes();
    for id in ["a", "longer-call", "引用调用"] {
        for content in ["{}", r#"{"content":"text"}"#, r#"{"content":"\"\\"}"#] {
            assert!(
                super::super::history::serialized_tool_response_bytes(id, content, None).unwrap()
                    >= minimum
            );
        }
    }
    let mut policy = AssistantDataPolicy::default();
    policy.limits.retrieval.episode.response_bytes = minimum - 1;
    assert_eq!(
        assess(&policy).issues[0].code,
        PolicyIssueCode::ResponseTooSmall
    );
    policy.limits.retrieval.episode.response_bytes = minimum;
    assert!(assess(&policy).issues.is_empty());
    policy.limits.retrieval.episode.response_bytes = 4;
    assert!(assess(&policy).has_errors());
    policy.limits.retrieval.episode.relevance_calls = 0;
    policy.limits.retrieval.episode.time_pages = 0;
    policy.limits.retrieval.episode.read_bytes = 0;
    assert!(assess(&policy).issues.is_empty());
}

#[test]
fn positive_read_budgets_are_not_subject_to_an_empirical_floor() {
    let mut policy = AssistantDataPolicy::default();
    for bytes in [0, 1, 511, 512, 513] {
        policy.limits.retrieval.episode.read_bytes = bytes;
        assert!(assess(&policy).issues.is_empty(), "{bytes}");
    }
}

#[test]
fn unrelated_capacity_and_time_limits_are_not_guessed() {
    let mut policy = AssistantDataPolicy::default();
    policy.limits.retrieval.search_window_days = policy.history_retention_days + 1;
    policy.limits.retrieval.episode.query_state_bytes = 1;
    policy.max_memory_body_bytes = 1;
    assert!(assess(&policy).issues.is_empty());
}
