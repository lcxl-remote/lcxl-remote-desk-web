//! Stateless diagnostics for external assistant policy configuration.

use desk_agent_protocol::ai_assistant::{
    policy::{PolicyAssessment, PolicyIssue, PolicyIssueCode, PolicyIssueLevel, policy_errors},
    user_instructions::AssistantDataPolicy,
};

/// Every retrieval result is a JSON object in a model-neutral tool envelope.
/// A shortest valid call identifier and an empty object provide a necessary
/// lower bound, not a promise that any particular result will fit.
pub fn minimum_response_bytes() -> u32 {
    super::history::serialized_tool_response_bytes("a", "{}", None)
        .expect("fixed tool envelope must serialize within u32")
}

pub fn response_issue(policy: &AssistantDataPolicy) -> Option<PolicyIssue> {
    let episode = &policy.limits.retrieval.episode;
    let enabled = episode.relevance_calls > 0
        || episode.time_pages > 0
        || (episode.read_calls > 0 && episode.read_bytes > 0);
    let minimum = minimum_response_bytes();
    (enabled && episode.response_bytes < minimum).then(|| {
        let mut issue = PolicyIssue::error(
            PolicyIssueCode::ResponseTooSmall,
            &["limits.retrieval.episode.response_bytes"],
            format!("A tool response envelope requires at least {minimum} bytes"),
        );
        issue.effective_limit = Some(u64::from(minimum));
        issue
    })
}

pub fn assess(policy: &AssistantDataPolicy) -> PolicyAssessment {
    let mut issues = policy_errors(policy);

    if let Some(issue) = response_issue(policy)
        && !issues
            .iter()
            .any(|existing| existing.fields == issue.fields)
    {
        issues.push(issue);
    }
    if !issues.is_empty() {
        return PolicyAssessment { issues };
    }

    PolicyAssessment {
        issues: warnings(policy),
    }
}

/// The host has already accepted the scalar policy invariants.
pub fn warnings(policy: &AssistantDataPolicy) -> Vec<PolicyIssue> {
    let mut issues = Vec::new();
    let r = &policy.limits.retrieval;
    let p = &policy.limits.processing;
    for (code, fields, requested, effective, message) in [
        (
            PolicyIssueCode::RelevancePageLimited,
            &[
                "limits.retrieval.relevance_hits",
                "limits.retrieval.query_candidates",
            ][..],
            u64::from(r.relevance_hits),
            u64::from(r.query_candidates),
            "Relevance page size exceeds the candidate limit",
        ),
        (
            PolicyIssueCode::HistoryCandidatesLimited,
            &[
                "limits.retrieval.query_candidates",
                "limits.retrieval.relevance_roots",
                "limits.retrieval.fragments_per_root",
            ][..],
            u64::from(r.query_candidates),
            u64::from(r.relevance_roots) * u64::from(r.fragments_per_root),
            "History relevance candidates are limited by conversation diversity",
        ),
        (
            PolicyIssueCode::NativeSummaryLimited,
            &[
                "limits.processing.native_status_model_items",
                "limits.processing.native_status_items",
            ][..],
            u64::from(p.native_status_model_items),
            u64::from(p.native_status_items),
            "Model status summary exceeds the retained item limit",
        ),
        (
            PolicyIssueCode::OwnerScanLimited,
            &[
                "limits.retrieval.owner_scan_documents",
                "max_retrieval_document_visits",
            ][..],
            u64::from(r.owner_scan_documents),
            u64::from(policy.max_retrieval_document_visits),
            "Owner history scans are limited by the document visit budget",
        ),
        (
            PolicyIssueCode::CurrentMemoriesLimited,
            &[
                "limits.memory.current_facts",
                "max_memory_records",
                "max_retrieval_document_visits",
            ][..],
            u64::from(policy.limits.memory.current_facts),
            policy
                .max_memory_records
                .min(u64::from(policy.max_retrieval_document_visits)),
            "Current memory selection exceeds its storage or scan limit",
        ),
    ] {
        if requested > effective {
            issues.push(PolicyIssue {
                level: PolicyIssueLevel::Warning,
                code,
                fields: fields.iter().map(|field| (*field).into()).collect(),
                message: format!("{message}; effective upper limit is {effective}"),
                effective_limit: Some(effective),
            });
        }
    }
    issues
}

/// Update metadata is checked at the same external boundary as its policy.
pub fn assess_update(
    request: &desk_agent_protocol::ai_assistant::user_instructions::AssistantDataPolicyUpdate,
) -> PolicyAssessment {
    let mut assessment = assess(&request.policy);
    if request.expected_revision == 0 || request.policy.revision != request.expected_revision {
        assessment.issues.push(PolicyIssue::error(
            PolicyIssueCode::RevisionMismatch,
            &["revision", "expected_revision"],
            "Policy revision must match the nonzero expected revision",
        ));
    }
    assessment
}

#[cfg(test)]
mod tests;
