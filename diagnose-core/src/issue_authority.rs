//! Canonical subscription policy and immutable admitted scope. Hosts own intake and locks.

use crate::{
    external_tools::IssueAccess,
    native_ids::{self, Kind},
};
use serde::{Deserialize, Serialize};

mod work_source;
pub use work_source::IssueWorkSource;
mod workset;
pub use workset::{
    IssueCommentSnapshot, IssueWorkset, is_issue_supplement_label, is_issue_workset_label,
};

pub use desk_agent_protocol::ai_assistant::issue_subscription::IssueWorkKind;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueRepositoryScope {
    pub platform: desk_agent_protocol::external_tools::IssuePlatform,
    pub connection_ref: String,
    pub instance: String,
    pub account_id: u64,
    pub repository_id: u64,
    pub owner: String,
    pub repository: String,
    pub private: bool,
    pub internal: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueFilter {
    pub issue_id: u64,
    pub issue_number: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueSubscription {
    pub subscription_id: String,
    pub revision: u64,
    pub actor_id: String,
    pub conversation_id: String,
    pub enabled: bool,
    pub repository: IssueRepositoryScope,
    pub issue_filter: Option<IssueFilter>,
    pub allowed_sender_ids: Vec<u64>,
    pub automation_sender_ids: Vec<u64>,
    pub work_kind: IssueWorkKind,
    pub auto_reply: bool,
}

impl IssueSubscription {
    pub fn canonicalize(&mut self) {
        self.allowed_sender_ids.sort_unstable();
        self.allowed_sender_ids.dedup();
        self.automation_sender_ids.sort_unstable();
        self.automation_sender_ids.dedup();
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        let repo = &self.repository;
        if !native_ids::is(Kind::IssueSubscription, &self.subscription_id)
            || self.revision == 0
            || self.revision > i64::MAX as u64
            || !valid_id(&self.actor_id)
            || !valid_id(&self.conversation_id)
            || !valid_id(&repo.connection_ref)
            || !repo.instance.starts_with("https://")
            || repo.instance.len() > 2048
            || repo.account_id == 0
            || repo.repository_id == 0
            || !valid_component(&repo.owner)
            || !valid_component(&repo.repository)
            || self
                .issue_filter
                .is_some_and(|filter| filter.issue_id == 0 || filter.issue_number == 0)
            || self.enabled && self.allowed_sender_ids.is_empty()
            || !valid_ids(&self.allowed_sender_ids)
            || !valid_ids(&self.automation_sender_ids)
            || self
                .allowed_sender_ids
                .iter()
                .any(|id| self.automation_sender_ids.binary_search(id).is_ok())
        {
            return Err("invalid issue subscription");
        }
        Ok(())
    }
    pub fn allows_issue(&self, issue: &IssueAccess) -> bool {
        let repo = &self.repository;
        let target = &issue.target;
        self.enabled
            && repo.platform == target.platform
            && repo.connection_ref == issue.connection_ref
            && repo.instance == target.instance
            && repo.account_id == target.account_id
            && repo.repository_id == target.repository_id
            && repo.owner == target.owner
            && repo.repository == target.repository
            && repo.private == target.private
            && repo.internal == target.internal
            && self.issue_filter.is_none_or(|filter| {
                filter.issue_id == target.issue_id && filter.issue_number == target.issue_number
            })
    }
    pub fn allows_event(
        &self,
        issue: &IssueAccess,
        sender: u64,
        automation: bool,
        echo: bool,
    ) -> bool {
        self.allows_issue(issue)
            && !automation
            && !echo
            && sender > 0
            && self.allowed_sender_ids.binary_search(&sender).is_ok()
            && self.automation_sender_ids.binary_search(&sender).is_err()
    }
    pub fn overlaps(&self, other: &Self) -> bool {
        self.enabled
            && other.enabled
            && self.actor_id == other.actor_id
            && self.conversation_id == other.conversation_id
            && self.repository.platform == other.repository.platform
            && self.repository.instance == other.repository.instance
            && self.repository.repository_id == other.repository.repository_id
            && match (self.issue_filter, other.issue_filter) {
                (Some(a), Some(b)) => a.issue_id == b.issue_id,
                _ => true,
            }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueAdmission {
    pub authorization_id: String,
    pub subscription: IssueSubscription,
    pub event_id: String,
    pub sender_id: u64,
    pub sender_automation: bool,
    pub echo: bool,
    pub issue: IssueAccess,
    pub workset: IssueWorkset,
    pub request_id: String,
    pub execution_root_id: String,
    pub admitted_at_ms: i64,
    pub expires_at_ms: Option<i64>,
}

impl IssueAdmission {
    pub fn id(subscription: &IssueSubscription, event_id: &str) -> String {
        native_ids::stable(
            Kind::IssueAdmission,
            &(
                &subscription.actor_id,
                &subscription.subscription_id,
                subscription.revision,
                event_id,
            ),
        )
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        self.subscription.validate()?;
        self.issue
            .validate()
            .map_err(|_| "invalid admitted issue")?;
        self.workset.validate(&self.subscription)?;
        if self.authorization_id != Self::id(&self.subscription, &self.event_id)
            || !valid_id(&self.event_id)
            || !native_ids::is(Kind::AssistantMainRequest, &self.request_id)
            || !valid_id(&self.execution_root_id)
            || self.admitted_at_ms < 0
            || self
                .expires_at_ms
                .is_some_and(|at| at <= self.admitted_at_ms)
            || !self.subscription.allows_event(
                &self.issue,
                self.sender_id,
                self.sender_automation,
                self.echo,
            )
        {
            return Err("invalid issue admission");
        }
        Ok(())
    }
    pub fn usable(&self, current: &IssueSubscription, now: i64) -> bool {
        now >= self.admitted_at_ms
            && self.expires_at_ms.is_none_or(|at| now < at)
            && self.subscription.subscription_id == current.subscription_id
            && self.subscription.actor_id == current.actor_id
            && self.subscription.conversation_id == current.conversation_id
            && current.revision >= self.subscription.revision
            && current.allows_event(
                &self.issue,
                self.sender_id,
                self.sender_automation,
                self.echo,
            )
    }
    pub fn can_publish(&self, current: &IssueSubscription, now: i64) -> bool {
        self.subscription.auto_reply && current.auto_reply && self.usable(current, now)
    }
    pub fn can_use_devices(&self, current: &IssueSubscription, now: i64) -> bool {
        self.subscription.work_kind == IssueWorkKind::DeviceInvestigation
            && current.work_kind == IssueWorkKind::DeviceInvestigation
            && self.usable(current, now)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueSourceRef {
    pub authorization_id: String,
    pub subscription_revision: u64,
    pub tool_ref: String,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 512 && !id.chars().any(char::is_control)
}
fn valid_component(value: &str) -> bool {
    valid_id(value) && !value.contains(['/', '\\']) && value != "." && value != ".."
}
fn valid_ids(ids: &[u64]) -> bool {
    ids.iter().all(|id| *id > 0) && ids.windows(2).all(|pair| pair[0] < pair[1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{external_tools::IssueTarget, input_read_context::ReadContextSelection};

    fn selection() -> ReadContextSelection {
        let issue = IssueAccess {
            connection_ref: "forge".into(),
            target: IssueTarget {
                platform: desk_agent_protocol::external_tools::IssuePlatform::Gitea,
                instance: "https://forge.test/".into(),
                account_id: 3,
                repository_id: 42,
                owner: "org".into(),
                repository: "project".into(),
                issue_number: 4,
                issue_id: 82,
                private: true,
                internal: false,
            },
        };
        let mut selected = ReadContextSelection::capture(
            &[],
            &desk_agent_protocol::AgentScope {
                granted: vec![],
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
        )
        .unwrap();
        selected.external_issue_sources.push(IssueSourceRef {
            authorization_id: native_ids::stable(Kind::IssueAdmission, &"admission"),
            subscription_revision: 1,
            tool_ref: issue.tool_ref(),
        });
        selected.external_issues.push(issue);
        selected
    }

    #[test]
    fn source_refs_round_trip_with_the_original_read_context_and_reject_mismatched_targets() {
        let selected = selection();
        selected.validate().unwrap();
        let encoded = serde_json::to_string(&selected).unwrap();
        let decoded: ReadContextSelection = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, selected);
        for case in 0..5 {
            let mut changed = selected.clone();
            match case {
                0 => changed.external_issue_sources[0].tool_ref = "other-issue".into(),
                1 => changed.external_issue_sources[0].authorization_id = "unverified".into(),
                2 => changed.external_issue_sources[0].subscription_revision = 0,
                3 => changed.external_issues.clear(),
                _ => changed
                    .external_issue_sources
                    .push(changed.external_issue_sources[0].clone()),
            }
            assert!(changed.validate().is_err());
        }
    }

    #[test]
    fn child_creation_keeps_source_refs_instead_of_promoting_read_access_into_publication() {
        let selected = selection();
        let destination = desk_agent_protocol::data_lineage::DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        };
        let owner = crate::model_message_labels::model_bound_user_message(
            "owner-input".into(),
            "Investigate the selected issue".into(),
            destination.clone(),
        )
        .unwrap();
        let creation = crate::subagent::creation::CreationEnvelope {
            root_conversation_id: "root".into(),
            actor_id: "owner".into(),
            device_id: String::new(),
            targets: vec![],
            main_request_id: None,
            source: crate::subagent::DelegationSource::UserInput { input_revision: 1 },
            parent_input_revision: 1,
            parent_control_revision: 1,
            owner_requirement: owner,
            scheduled_source: None,
            original_read_context: Some(selected.clone()),
            model_destination: destination,
            assistant_seed: None,
        };
        creation.validate().unwrap();
        assert_eq!(creation.child_read_context().unwrap(), selected);
        let mut read_only = creation;
        read_only
            .original_read_context
            .as_mut()
            .unwrap()
            .external_issue_sources
            .clear();
        let child = read_only.child_read_context().unwrap();
        assert_eq!(child.external_issues, selected.external_issues);
        assert!(child.external_issue_sources.is_empty());
    }
}
