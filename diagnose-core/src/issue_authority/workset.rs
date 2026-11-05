//! Frozen external materials, independent of Worker history and device evidence.

use super::*;
use crate::{
    chat::{ChatMessage, ChatRole},
    external_tools::IssueReadMethod,
};
use desk_agent_protocol::data_lineage::{
    ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance, DestinationIdentity,
    RetentionBoundary, Sensitivity,
};
use sha2::{Digest, Sha256};

const MAX_WORKSET_BYTES: usize = 1024 * 1024;
const PROVIDER: &str = "ai-assistant-external-issue";
const TOOL: &str = "issue_workset_snapshot";
const PREFIX: &str = "external-issue-workset-";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueCommentSnapshot {
    pub id: u64,
    pub author_id: u64,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueWorkset {
    pub title: String,
    pub body: String,
    pub comments: Vec<IssueCommentSnapshot>,
}

impl IssueWorkset {
    pub fn validate(&self, policy: &IssueSubscription) -> Result<(), &'static str> {
        let mut ids = std::collections::BTreeSet::new();
        if self.title.is_empty()
            || self.comments.iter().any(|comment| {
                comment.id == 0
                    || comment.author_id == 0
                    || !ids.insert(comment.id)
                    || policy
                        .allowed_sender_ids
                        .binary_search(&comment.author_id)
                        .is_err()
                    || policy
                        .automation_sender_ids
                        .binary_search(&comment.author_id)
                        .is_ok()
            })
            || serde_json::to_vec(self)
                .map_err(|_| "invalid issue workset")?
                .len()
                > MAX_WORKSET_BYTES
        {
            return Err("invalid issue workset");
        }
        Ok(())
    }

    pub fn read(&self, method: IssueReadMethod) -> String {
        match method {
            IssueReadMethod::Get => {
                serde_json::json!({"issue":{"title":self.title,"body":self.body}})
            }
            IssueReadMethod::GetComments => serde_json::json!({"comments":self.comments}),
        }
        .to_string()
    }
}

impl IssueAdmission {
    /// The host reads this immutable snapshot only after checking the native
    /// source and current subscription. The label never grants publication.
    pub fn publication_material(
        &self,
        destination: &DestinationIdentity,
    ) -> Result<ChatMessage, desk_agent_protocol::AgentError> {
        self.validate().map_err(crate::subagent::invalid)?;
        destination
            .validate()
            .map_err(|_| crate::subagent::invalid("Invalid issue model destination"))?;
        if !matches!(destination, DestinationIdentity::Model { .. }) {
            return Err(crate::subagent::invalid("Invalid issue model destination"));
        }
        let text = serde_json::to_string(&self.workset)
            .map_err(|_| crate::subagent::invalid("Invalid issue workset"))?;
        let digest = format!("{:x}", Sha256::digest(text.as_bytes()));
        let identity = serde_json::to_vec(&(
            &self.authorization_id,
            &self.request_id,
            &self.issue,
            &digest,
        ))
        .expect("snapshot fields serialize");
        let id = format!("{PREFIX}{:x}", Sha256::digest(identity));
        let mut message = ChatMessage::text(id.clone(), ChatRole::UntrustedOutput, text.clone());
        message.data_envelope = Some(DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: id.clone(),
            content: ContentRef::ImmutableBlob {
                blob_id: format!("issue-workset-{digest}"),
                sha256: digest.clone(),
                size_bytes: text.len() as u64,
                media_type: "application/json;charset=utf-8".into(),
            },
            provenance: DataProvenance {
                source_provider_id: PROVIDER.into(),
                source_tool_name: TOOL.into(),
                source_object_id: Some(id),
                source_envelope_ids: vec![],
            },
            digest_sha256: digest,
            sensitivity: if self.issue.target.private || self.issue.target.internal {
                Sensitivity::UserContent
            } else {
                Sensitivity::Public
            },
            allowed_destinations: vec![
                destination.clone(),
                self.issue
                    .target
                    .destination(self.issue.connection_ref.clone()),
            ],
            retention: RetentionBoundary {
                expires_at_unix_ms: self.expires_at_ms.and_then(|at| u64::try_from(at).ok()),
                delete_with_run: true,
            },
        });
        Ok(message)
    }
}

pub fn is_issue_workset_label(label: &DataEnvelope) -> bool {
    label.validate().is_ok()
        && label.provenance.source_provider_id == PROVIDER
        && label.provenance.source_tool_name == TOOL
        && label.provenance.source_object_id.as_deref() == Some(label.envelope_id.as_str())
        && label.provenance.source_envelope_ids.is_empty()
        && label.retention.delete_with_run
        && label
            .envelope_id
            .strip_prefix(PREFIX)
            .is_some_and(|id| id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        && matches!(label.allowed_destinations.as_slice(), [DestinationIdentity::Model {..}, DestinationIdentity::ExternalIssue {audience,..}]
            if label.sensitivity == if *audience == desk_agent_protocol::data_lineage::ExternalIssueAudience::Public { Sensitivity::Public } else { Sensitivity::UserContent })
        && matches!(&label.content, ContentRef::ImmutableBlob { blob_id,sha256,size_bytes,media_type }
            if blob_id == &format!("issue-workset-{}", label.digest_sha256)
                && sha256 == &label.digest_sha256 && *size_bytes > 0 && *size_bytes <= MAX_WORKSET_BYTES as u64
                && media_type == "application/json;charset=utf-8")
}
