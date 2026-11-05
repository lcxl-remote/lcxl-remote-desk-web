//! Immutable third-party comments. None of these facts are owner instructions.

use crate::{
    external_tools::IssueTarget,
    issue_authority::{IssueRepositoryScope, IssueSubscription},
    native_ids::{self, Kind},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueSourceComment {
    pub repository: IssueRepositoryScope,
    pub comment_id: u64,
    pub issue_number: Option<u64>,
    pub sender_id: Option<u64>,
    pub automation: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommentDisposition {
    Pending,
    BeforeActivation,
    PullRequest,
    OutsideScope,
    UnknownSender,
    Automation,
    SenderNotAllowed,
    OutgoingEcho,
}

impl IssueSourceComment {
    /// Neither edits nor rule revisions turn one remote comment into new work.
    pub fn event_id(&self) -> String {
        native_ids::stable(
            Kind::IssueComment,
            &(
                self.repository.platform,
                &self.repository.instance,
                self.repository.account_id,
                self.repository.repository_id,
                self.comment_id,
            ),
        )
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        let repo = &self.repository;
        IssueTarget {
            platform: repo.platform,
            instance: repo.instance.clone(),
            account_id: repo.account_id,
            repository_id: repo.repository_id,
            owner: repo.owner.clone(),
            repository: repo.repository.clone(),
            issue_number: self.issue_number.unwrap_or(1),
            issue_id: 1,
            private: repo.private,
            internal: repo.internal,
        }
        .validate()
        .map_err(|_| "invalid comment source")?;
        if self.comment_id == 0
            || self.sender_id == Some(0)
            || self.created_at_ms < 0
            || self.updated_at_ms < self.created_at_ms
            || self.body.len() > 1024 * 1024
            || repo.connection_ref.is_empty()
            || repo.connection_ref.len() > 512
            || repo.connection_ref.chars().any(char::is_control)
        {
            return Err("invalid source comment");
        }
        Ok(())
    }

    pub fn disposition(
        &self,
        policy: &IssueSubscription,
        first_enabled_at_ms: i64,
        echo: bool,
    ) -> CommentDisposition {
        if self.repository != policy.repository || !policy.enabled {
            CommentDisposition::OutsideScope
        } else if self.created_at_ms < first_enabled_at_ms {
            CommentDisposition::BeforeActivation
        } else if self.issue_number.is_none() {
            CommentDisposition::PullRequest
        } else if policy
            .issue_filter
            .is_some_and(|filter| Some(filter.issue_number) != self.issue_number)
        {
            CommentDisposition::OutsideScope
        } else if self.sender_id.is_none() {
            CommentDisposition::UnknownSender
        } else if self.automation
            || self
                .sender_id
                .is_some_and(|id| policy.automation_sender_ids.binary_search(&id).is_ok())
        {
            CommentDisposition::Automation
        } else if self
            .sender_id
            .is_some_and(|id| policy.allowed_sender_ids.binary_search(&id).is_err())
        {
            CommentDisposition::SenderNotAllowed
        } else if echo {
            CommentDisposition::OutgoingEcho
        } else {
            CommentDisposition::Pending
        }
    }

    pub fn fingerprint(&self) -> String {
        format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(self).expect("comment serializes"))
        )
    }
}

pub fn work_id(actor: &str, subscription: &str, repository: u64, issue: u64) -> String {
    native_ids::stable(Kind::IssueWork, &(actor, subscription, repository, issue))
}

pub fn request_id(work: &str, event: &str) -> String {
    native_ids::stable(Kind::AssistantMainRequest, &(work, event))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> (IssueSourceComment, IssueSubscription) {
        let repository = IssueRepositoryScope {
            platform: crate::external_tools::IssuePlatform::Gitea,
            connection_ref: "forge".into(),
            instance: "https://forge.example/".into(),
            account_id: 3,
            repository_id: 42,
            owner: "Org".into(),
            repository: "Project".into(),
            private: true,
            internal: false,
        };
        (
            IssueSourceComment {
                repository: repository.clone(),
                comment_id: 90,
                issue_number: Some(4),
                sender_id: Some(7),
                automation: false,
                created_at_ms: 1000,
                updated_at_ms: 1000,
                body: "Original comment".into(),
            },
            IssueSubscription {
                subscription_id: native_ids::stable(Kind::IssueSubscription, &"test"),
                revision: 1,
                actor_id: "owner".into(),
                conversation_id: "root".into(),
                enabled: true,
                repository,
                issue_filter: None,
                allowed_sender_ids: vec![3, 7],
                automation_sender_ids: vec![8],
                work_kind: crate::issue_authority::IssueWorkKind::Analysis,
                auto_reply: true,
            },
        )
    }

    #[test]
    fn edits_have_the_same_event_identity_but_a_different_observation() {
        let (original, mut policy) = sample();
        original.validate().unwrap();
        policy.validate().unwrap();
        let mut edited = original.clone();
        edited.body = "Changed to misleading instructions".into();
        edited.updated_at_ms += 100;
        assert_eq!(original.event_id(), edited.event_id());
        assert_ne!(original.fingerprint(), edited.fingerprint());
        policy.revision += 1;
        assert_eq!(original.event_id(), edited.event_id());
        edited.repository.platform = crate::external_tools::IssuePlatform::Github;
        assert_ne!(original.event_id(), edited.event_id());
        let work = work_id("owner", &policy.subscription_id, 42, 82);
        assert_ne!(
            request_id(&work, &original.event_id()),
            request_id(&work, "later-event")
        );
    }

    #[test]
    fn only_current_allowed_humans_trigger_and_an_outgoing_account_can_still_be_human() {
        let (mut comment, policy) = sample();
        assert_eq!(
            comment.disposition(&policy, 1000, false),
            CommentDisposition::Pending
        );
        assert_eq!(
            comment.disposition(&policy, 1001, false),
            CommentDisposition::BeforeActivation
        );
        comment.sender_id = Some(3);
        assert_eq!(
            comment.disposition(&policy, 1000, false),
            CommentDisposition::Pending
        );
        assert_eq!(
            comment.disposition(&policy, 1000, true),
            CommentDisposition::OutgoingEcho
        );
        comment.automation = true;
        assert_eq!(
            comment.disposition(&policy, 1000, false),
            CommentDisposition::Automation
        );
        comment.automation = false;
        comment.sender_id = Some(8);
        assert_eq!(
            comment.disposition(&policy, 1000, false),
            CommentDisposition::Automation
        );
        comment.sender_id = Some(9);
        assert_eq!(
            comment.disposition(&policy, 1000, false),
            CommentDisposition::SenderNotAllowed
        );
        comment.sender_id = None;
        assert_eq!(
            comment.disposition(&policy, 1000, false),
            CommentDisposition::UnknownSender
        );
        comment.issue_number = None;
        assert_eq!(
            comment.disposition(&policy, 1000, false),
            CommentDisposition::PullRequest
        );
    }
}
