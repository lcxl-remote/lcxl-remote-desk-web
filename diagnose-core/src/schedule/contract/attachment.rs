//! Attachment quotas do not establish artifact ownership or authorize an external sink.
use super::TaskContractError;
use desk_agent_protocol::{
    communication::{
        ImmutableAttachmentSnapshot, MAX_ATTACHMENT_BYTES, MAX_ATTACHMENTS,
        MAX_TOTAL_ATTACHMENT_BYTES,
    },
    schedule::contract::{TaskAttachmentLimits, TaskAttachmentPolicy},
};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskAttachmentDecision {
    Automatic,
    RequiresApproval,
    Denied,
}

impl TaskAttachmentDecision {
    /// Intersect attachment quotas with the ordinary task decision. An attachment
    /// allowance cannot override a source, destination, identity or budget denial.
    pub fn intersect(self, decision: super::TaskDecision) -> super::TaskDecision {
        use super::{TaskDecision, TaskDenyReason};
        match (self, decision) {
            (_, denied @ TaskDecision::Denied(_)) => denied,
            (Self::Denied, _) => TaskDecision::Denied(TaskDenyReason::HardBoundary),
            (Self::RequiresApproval, TaskDecision::Allowed { rule_id }) => {
                TaskDecision::ApprovalRequired { rule_id }
            }
            (_, decision) => decision,
        }
    }
}

fn valid_limits(limits: &TaskAttachmentLimits) -> bool {
    if limits.max_count == 0 {
        return limits.max_bytes_per_attachment == 0
            && limits.max_total_bytes == 0
            && limits.media_types.is_empty();
    }
    limits.max_count as usize <= MAX_ATTACHMENTS
        && limits.max_bytes_per_attachment > 0
        && limits.max_bytes_per_attachment <= MAX_ATTACHMENT_BYTES
        && limits.max_total_bytes >= limits.max_bytes_per_attachment
        && limits.max_total_bytes <= MAX_TOTAL_ATTACHMENT_BYTES
        && !limits.media_types.is_empty()
        && limits.media_types.len() <= 32
        && limits.media_types.iter().collect::<BTreeSet<_>>().len() == limits.media_types.len()
        && limits.media_types.iter().all(|media| {
            let mut parts = media.split('/');
            let valid = |part: Option<&str>| {
                part.is_some_and(|part| {
                    !part.is_empty()
                        && part.bytes().all(|b| {
                            b.is_ascii_lowercase()
                                || b.is_ascii_digit()
                                || b"!#$&^_.+-".contains(&b)
                        })
                })
            };
            (media == crate::provider_preflight::TEXT_ARTIFACT_MEDIA_TYPE)
                || (media.len() <= 128
                    && valid(parts.next())
                    && valid(parts.next())
                    && parts.next().is_none())
        })
}

pub fn validate_attachment_policy(policy: &TaskAttachmentPolicy) -> Result<(), TaskContractError> {
    let automatic = &policy.automatic;
    let ceiling = &policy.approval_ceiling;
    if !valid_limits(automatic)
        || !valid_limits(ceiling)
        || automatic.max_count > ceiling.max_count
        || automatic.max_bytes_per_attachment > ceiling.max_bytes_per_attachment
        || automatic.max_total_bytes > ceiling.max_total_bytes
        || !automatic
            .media_types
            .iter()
            .all(|media| ceiling.media_types.contains(media))
    {
        return Err(TaskContractError::InvalidLimits);
    }
    Ok(())
}

/// Seed an unapproved contract from the exact verified attachment set. Both
/// ceilings start equal; observing a send never silently creates extra capacity.
pub fn observed_attachment_policy(
    attachments: &[ImmutableAttachmentSnapshot],
) -> Result<Option<TaskAttachmentPolicy>, TaskContractError> {
    if attachments.is_empty() {
        return Ok(None);
    }
    let mut total = 0u64;
    let mut largest = 0u64;
    let mut media_types = BTreeSet::new();
    for item in attachments {
        item.validate()
            .map_err(|_| TaskContractError::InvalidInput)?;
        total = total
            .checked_add(item.size_bytes)
            .ok_or(TaskContractError::InvalidLimits)?;
        largest = largest.max(item.size_bytes);
        media_types.insert(item.media_type.clone());
    }
    let limits = TaskAttachmentLimits {
        max_count: u32::try_from(attachments.len())
            .map_err(|_| TaskContractError::InvalidLimits)?,
        max_bytes_per_attachment: largest,
        max_total_bytes: total,
        media_types: media_types.into_iter().collect(),
    };
    let policy = TaskAttachmentPolicy {
        automatic: limits.clone(),
        approval_ceiling: limits,
    };
    if evaluate_attachment_limits(&policy, attachments)? != TaskAttachmentDecision::Automatic {
        return Err(TaskContractError::InvalidLimits);
    }
    Ok(Some(policy))
}

fn fits(
    attachments: &[ImmutableAttachmentSnapshot],
    total: u64,
    limits: &TaskAttachmentLimits,
) -> bool {
    attachments.len() <= limits.max_count as usize
        && total <= limits.max_total_bytes
        && attachments.iter().all(|item| {
            item.size_bytes <= limits.max_bytes_per_attachment
                && limits.media_types.contains(&item.media_type)
        })
}

/// Every snapshot must already be resolved from an authentic current-run receipt.
/// Invalid inputs and duplicates never become approval candidates.
pub fn evaluate_attachment_limits(
    policy: &TaskAttachmentPolicy,
    attachments: &[ImmutableAttachmentSnapshot],
) -> Result<TaskAttachmentDecision, TaskContractError> {
    validate_attachment_policy(policy)?;
    let mut digests = BTreeSet::new();
    let mut total = 0u64;
    for attachment in attachments {
        attachment
            .validate()
            .map_err(|_| TaskContractError::InvalidInput)?;
        if !digests.insert(&attachment.digest_sha256) {
            return Err(TaskContractError::InvalidInput);
        }
        total = total
            .checked_add(attachment.size_bytes)
            .ok_or(TaskContractError::InvalidLimits)?;
    }
    Ok(if !fits(attachments, total, &policy.approval_ceiling) {
        TaskAttachmentDecision::Denied
    } else if fits(attachments, total, &policy.automatic) {
        TaskAttachmentDecision::Automatic
    } else {
        TaskAttachmentDecision::RequiresApproval
    })
}

/// A bounded evaluation result, not a dispatch grant. Callers must still verify
/// destination identity, the sealed payload and the ordinary action authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskAttachmentEvaluation {
    pub decision: TaskAttachmentDecision,
    pub sources: crate::schedule::source_graph::ResolvedTaskSources,
    contract_digest: String,
    run_id: String,
    step_id: String,
    attachments: Vec<ImmutableAttachmentSnapshot>,
    verified_decision: TaskAttachmentDecision,
}

impl TaskAttachmentEvaluation {
    pub(super) fn matches(
        &self,
        contract: &super::ValidatedTaskContract,
        run_id: &str,
        step_id: &str,
        attachments: &[ImmutableAttachmentSnapshot],
    ) -> bool {
        self.contract_digest == contract.digest
            && self.run_id == run_id
            && self.step_id == step_id
            && self.attachments == attachments
            && self.verified_decision != TaskAttachmentDecision::Denied
            && self.decision == self.verified_decision
    }
}

impl super::ValidatedTaskContract {
    pub(super) fn attachment_approval_scope(
        &self,
        step_id: &str,
        attachments: &[ImmutableAttachmentSnapshot],
    ) -> bool {
        use desk_agent_protocol::schedule::contract::{TaskExceptionMode, TaskInputConstraint};
        if self.contract.exception_mode != TaskExceptionMode::RequestApproval {
            return false;
        }
        let Some(step) = self
            .contract
            .steps
            .iter()
            .find(|step| step.step_id == step_id)
        else {
            return false;
        };
        let Some(rule) = self
            .contract
            .permissions
            .iter()
            .find(|rule| rule.rule_id == step.rule_id)
        else {
            return false;
        };
        let TaskInputConstraint::GeneratedMessage {
            attachment_policy: Some(policy),
            ..
        } = &rule.input
        else {
            return false;
        };
        matches!(
            evaluate_attachment_limits(policy, attachments),
            Ok(TaskAttachmentDecision::Automatic | TaskAttachmentDecision::RequiresApproval)
        )
    }

    pub fn evaluate_step_attachments(
        &self,
        run_id: &str,
        step_id: &str,
        attachments: &[ImmutableAttachmentSnapshot],
        receipts: &[crate::schedule::source_graph::attachment::TaskAttachmentReceipt<'_>],
        nodes: &[crate::model_egress::ModelInputLineage],
        bindings: &[crate::schedule::source_graph::TaskSourceBinding],
    ) -> Result<TaskAttachmentEvaluation, TaskContractError> {
        use crate::schedule::source_graph::{
            ResolvedTaskSources, attachment::resolve_task_attachment_sources,
        };
        use desk_agent_protocol::schedule::contract::{
            TaskExceptionMode, TaskInputConstraint, TaskStepBinding,
        };
        super::id(run_id)?;
        let step = self
            .contract
            .steps
            .iter()
            .find(|step| step.step_id == step_id)
            .ok_or(TaskContractError::InvalidSteps)?;
        let TaskStepBinding::SendMessage {
            allowed_source_scopes,
            ..
        } = &step.binding
        else {
            return Err(TaskContractError::InvalidSteps);
        };
        let rule = self
            .contract
            .permissions
            .iter()
            .find(|rule| rule.rule_id == step.rule_id)
            .ok_or(TaskContractError::InvalidSteps)?;
        let TaskInputConstraint::GeneratedMessage {
            attachment_policy, ..
        } = &rule.input
        else {
            return Err(TaskContractError::InvalidInput);
        };
        if attachments.len() != receipts.len() || attachments.len() > MAX_ATTACHMENTS {
            return Err(TaskContractError::InvalidInput);
        }
        let decision = match attachment_policy {
            Some(policy) => evaluate_attachment_limits(policy, attachments)?,
            None if attachments.is_empty() => TaskAttachmentDecision::Automatic,
            None => TaskAttachmentDecision::Denied,
        };
        let mut scopes = BTreeSet::new();
        let mut roots = BTreeSet::new();
        let mut envelopes = BTreeSet::new();
        for (attachment, receipt) in attachments.iter().zip(receipts) {
            if !envelopes.insert(receipt.envelope_id) {
                return Err(TaskContractError::InvalidInput);
            }
            let source =
                resolve_task_attachment_sources(run_id, attachment, receipt, nodes, bindings)
                    .map_err(|_| TaskContractError::InvalidScope)?;
            if source.scopes.is_empty()
                || !source
                    .scopes
                    .iter()
                    .all(|scope| allowed_source_scopes.contains(scope))
            {
                return Err(TaskContractError::InvalidScope);
            }
            scopes.extend(source.scopes);
            roots.extend(source.root_envelope_ids);
        }
        let decision = if decision == TaskAttachmentDecision::RequiresApproval
            && self.contract.exception_mode == TaskExceptionMode::Deny
        {
            TaskAttachmentDecision::Denied
        } else {
            decision
        };
        Ok(TaskAttachmentEvaluation {
            contract_digest: self.digest.clone(),
            run_id: run_id.into(),
            step_id: step_id.into(),
            attachments: attachments.to_vec(),
            verified_decision: decision,
            decision,
            sources: ResolvedTaskSources {
                scopes: scopes.into_iter().collect(),
                root_envelope_ids: roots.into_iter().collect(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::data_lineage::ContentRef;
    fn limits(bytes: u64) -> TaskAttachmentLimits {
        TaskAttachmentLimits {
            max_count: 1,
            max_bytes_per_attachment: bytes,
            max_total_bytes: bytes,
            media_types: vec!["text/plain".into()],
        }
    }
    fn file(bytes: u64) -> ImmutableAttachmentSnapshot {
        ImmutableAttachmentSnapshot {
            content: ContentRef::Artifact {
                artifact_id: "artifact".into(),
                sha256: "a".repeat(64),
                size_bytes: bytes,
                media_type: "text/plain".into(),
            },
            file_name: "report.txt".into(),
            media_type: "text/plain".into(),
            size_bytes: bytes,
            digest_sha256: "a".repeat(64),
        }
    }
    #[test]
    fn observed_policy_starts_at_exact_recorded_capacity() {
        let policy = observed_attachment_policy(&[file(7)]).unwrap().unwrap();
        assert_eq!(policy.automatic, policy.approval_ceiling);
        assert_eq!(policy.automatic.max_count, 1);
        assert_eq!(policy.automatic.max_total_bytes, 7);
        assert_eq!(policy.automatic.max_bytes_per_attachment, 7);
        assert_eq!(policy.automatic.media_types, vec!["text/plain"]);
        assert_eq!(observed_attachment_policy(&[]), Ok(None));
        assert_eq!(
            observed_attachment_policy(&[file(7), file(7)]),
            Err(TaskContractError::InvalidInput)
        );
    }

    #[test]
    fn attachment_decision_never_weakens_ordinary_authority() {
        use crate::schedule::contract::{TaskDecision, TaskDenyReason};
        let allowed = || TaskDecision::Allowed {
            rule_id: "rule".into(),
        };
        let approval = || TaskDecision::ApprovalRequired {
            rule_id: "rule".into(),
        };
        assert_eq!(
            TaskAttachmentDecision::Automatic.intersect(allowed()),
            allowed()
        );
        assert_eq!(
            TaskAttachmentDecision::RequiresApproval.intersect(allowed()),
            approval()
        );
        assert_eq!(
            TaskAttachmentDecision::Automatic.intersect(approval()),
            approval()
        );
        assert_eq!(
            TaskAttachmentDecision::Denied.intersect(approval()),
            TaskDecision::Denied(TaskDenyReason::HardBoundary)
        );
        for attachment in [
            TaskAttachmentDecision::Automatic,
            TaskAttachmentDecision::RequiresApproval,
            TaskAttachmentDecision::Denied,
        ] {
            for reason in [
                TaskDenyReason::Identity,
                TaskDenyReason::Destination,
                TaskDenyReason::SourceScope,
                TaskDenyReason::Budget,
            ] {
                assert_eq!(
                    attachment.intersect(TaskDecision::Denied(reason)),
                    TaskDecision::Denied(reason)
                );
            }
        }
    }

    #[test]
    fn distinguishes_automatic_exception_and_hard_limit() {
        let policy = TaskAttachmentPolicy {
            automatic: limits(10),
            approval_ceiling: limits(20),
        };
        assert_eq!(
            evaluate_attachment_limits(&policy, &[file(10)]),
            Ok(TaskAttachmentDecision::Automatic)
        );
        assert_eq!(
            evaluate_attachment_limits(&policy, &[file(11)]),
            Ok(TaskAttachmentDecision::RequiresApproval)
        );
        assert_eq!(
            evaluate_attachment_limits(&policy, &[file(21)]),
            Ok(TaskAttachmentDecision::Denied)
        );
        assert_eq!(
            evaluate_attachment_limits(&policy, &[file(1), file(1)]),
            Err(TaskContractError::InvalidInput)
        );
    }
    #[test]
    fn accepts_exact_edge_text_media_type_without_general_parameter_wildcards() {
        let mut policy = TaskAttachmentPolicy {
            automatic: limits(10),
            approval_ceiling: limits(10),
        };
        policy.automatic.media_types =
            vec![crate::provider_preflight::TEXT_ARTIFACT_MEDIA_TYPE.into()];
        policy.approval_ceiling.media_types = policy.automatic.media_types.clone();
        assert!(validate_attachment_policy(&policy).is_ok());
        policy.approval_ceiling.media_types = vec!["text/plain;charset=*".into()];
        assert!(validate_attachment_policy(&policy).is_err());
    }

    #[test]
    fn rejects_wildcards_and_inverted_authority() {
        let mut policy = TaskAttachmentPolicy {
            automatic: limits(20),
            approval_ceiling: limits(10),
        };
        assert!(validate_attachment_policy(&policy).is_err());
        policy.automatic = limits(10);
        policy.approval_ceiling.media_types = vec!["text/*".into()];
        assert!(validate_attachment_policy(&policy).is_err());
    }
}
