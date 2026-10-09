//! Bounded, content-free conclusions for observed correction chains.

use super::{
    aggregate::{Contribution, Count},
    *,
};

pub const MAX_GROUP_LINKS: u32 = 32;

closed_enum!(CorrectionCategory {
    ToolInput,
    Protocol,
    Schedule,
    Approval
});
closed_enum!(CorrectionGroupOutcome {
    AwaitingResponse,
    AwaitingValidation,
    AwaitingOutputCheck,
    InputAccepted,
    InputRejected,
    OutputAccepted,
    OutputRejected,
    Ambiguous,
    NotComparable,
    Switched,
    NoResponse,
    Unavailable
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorrectionGroup {
    pub root_id: String,
    pub last_input_id: String,
    pub category: CorrectionCategory,
    pub reason: Option<super::protocol_correction::ProtocolCorrectionReason>,
    pub outcome: CorrectionGroupOutcome,
    pub linked_attempts: u32,
    pub updated_at_ms: i64,
}

pub fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= 192 && !value.chars().any(char::is_control)
}

impl CorrectionGroup {
    pub fn is_bounded(&self) -> bool {
        valid_identity(&self.root_id)
            && valid_identity(&self.last_input_id)
            && self.linked_attempts <= MAX_GROUP_LINKS
            && self.updated_at_ms >= 0
    }

    /// Only the root contributes a group count. This conclusion does not
    /// certify permissions, execution, or the original business intention.
    pub fn contribute(&self, value: &mut Contribution) {
        value.counts.insert(Count::CorrectionGroups, 1);
        value.counts.insert(
            match self.category {
                CorrectionCategory::ToolInput => Count::CorrectionGroupToolInput,
                CorrectionCategory::Protocol => Count::CorrectionGroupProtocol,
                CorrectionCategory::Schedule => Count::CorrectionGroupSchedule,
                CorrectionCategory::Approval => Count::CorrectionGroupApproval,
            },
            1,
        );
        value.counts.insert(
            match self.outcome {
                CorrectionGroupOutcome::AwaitingResponse => Count::CorrectionGroupAwaitingResponse,
                CorrectionGroupOutcome::AwaitingValidation => {
                    Count::CorrectionGroupAwaitingValidation
                }
                CorrectionGroupOutcome::AwaitingOutputCheck => {
                    Count::CorrectionGroupAwaitingOutputCheck
                }
                CorrectionGroupOutcome::InputAccepted => Count::CorrectionGroupInputAccepted,
                CorrectionGroupOutcome::InputRejected => Count::CorrectionGroupInputRejected,
                CorrectionGroupOutcome::OutputAccepted => Count::CorrectionGroupOutputAccepted,
                CorrectionGroupOutcome::OutputRejected => Count::CorrectionGroupOutputRejected,
                CorrectionGroupOutcome::Ambiguous => Count::CorrectionGroupAmbiguous,
                CorrectionGroupOutcome::NotComparable => Count::CorrectionGroupNotComparable,
                CorrectionGroupOutcome::Switched => Count::CorrectionGroupSwitched,
                CorrectionGroupOutcome::NoResponse => Count::CorrectionGroupNoResponse,
                CorrectionGroupOutcome::Unavailable => Count::CorrectionGroupUnavailable,
            },
            1,
        );
    }
}

/// Categories follow registered tool contracts, never prefixes or model text.
pub fn tool_category(key: &str) -> CorrectionCategory {
    if [
        crate::schedule::proposal::REQUEST_SCHEDULE,
        crate::schedule::management_tools::LIST,
        crate::schedule::management_tools::CANCEL,
    ]
    .contains(&key)
    {
        CorrectionCategory::Schedule
    } else if [
        crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME,
        crate::directory_tools::REQUEST_DIRECTORY,
        crate::goal_tools::REQUEST_GOAL_TOOL_NAME,
    ]
    .contains(&key)
    {
        CorrectionCategory::Approval
    } else {
        CorrectionCategory::ToolInput
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_conclusion_does_not_create_input_permission_or_execution_denominators() {
        let group = CorrectionGroup {
            root_id: "root.tool.0".into(),
            last_input_id: "next.tool.0".into(),
            category: CorrectionCategory::Approval,
            reason: None,
            outcome: CorrectionGroupOutcome::InputAccepted,
            linked_attempts: 1,
            updated_at_ms: 1,
        };
        let mut contribution = Contribution::default();
        group.contribute(&mut contribution);
        assert!(group.is_bounded());
        assert_eq!(contribution.get(Count::CorrectionGroups), 1);
        assert_eq!(contribution.get(Count::CorrectionGroupInputAccepted), 1);
        for key in [
            Count::InputAccepted,
            Count::PermissionApproved,
            Count::OperationsDispatched,
            Count::OperationsVerified,
        ] {
            assert_eq!(contribution.get(key), 0);
        }
        assert_eq!(contribution.counts.len(), 3);
    }

    #[test]
    fn identities_chain_size_and_categories_are_closed_and_bounded() {
        let mut group = CorrectionGroup {
            root_id: "root".into(),
            last_input_id: "leaf".into(),
            category: CorrectionCategory::ToolInput,
            reason: None,
            outcome: CorrectionGroupOutcome::Unavailable,
            linked_attempts: MAX_GROUP_LINKS,
            updated_at_ms: 0,
        };
        assert!(group.is_bounded());
        group.linked_attempts += 1;
        assert!(!group.is_bounded());
        group.linked_attempts = 0;
        group.root_id = "x".repeat(193);
        assert!(!group.is_bounded());
        assert!(!valid_identity("a\nb"));
        assert_eq!(
            tool_category(crate::schedule::proposal::REQUEST_SCHEDULE),
            CorrectionCategory::Schedule
        );
        assert_eq!(
            tool_category(crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME),
            CorrectionCategory::Approval
        );
        assert_eq!(
            tool_category("request_permissions_extra"),
            CorrectionCategory::ToolInput
        );
        assert!(serde_json::from_str::<CorrectionCategory>("\"untrusted\"").is_err());
    }
}
