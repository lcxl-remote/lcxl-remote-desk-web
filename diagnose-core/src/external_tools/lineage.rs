//! Whole-result lineage saved with the native tool message, before model reuse.

use super::*;
use crate::{input_read_context::ReadContextSelection, session::PersistedAgentSession};
use desk_agent_protocol::data_lineage::{
    ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance, RetentionBoundary,
    Sensitivity,
};

pub const READ_PROVIDER_ID: &str = "ai-assistant-external-issue";
const PREFIX: &str = "external-issue-read-";
const CONTENT_PREFIX: &str = "external-issue-content-";
const MEDIA_TYPE: &str = "application/json;charset=utf-8";
pub const MAX_READ_RESULT_BYTES: usize = 1024 * 1024;

/// This label is not dispatch authority. The host checks native input/lease and
/// current connection access on both sides of the HTTP read. The ordinary tool
/// writer then persists the exact bytes and label in the original conversation.
pub fn label_issue_read(
    session: &PersistedAgentSession,
    original: &ReadContextSelection,
    destination: &DestinationIdentity,
    call: &ToolCall,
    tool_signature: &str,
    content: &str,
) -> Result<DataEnvelope, AgentError> {
    let (access, _) = resolve(&original.external_issues, call)?;
    if session.input_revision == 0 {
        return Err(invalid("External read lineage is unavailable"));
    }
    let origin = (
        "active_input",
        &session.conversation_id,
        &session.actor_id,
        session.input_revision,
        session.control_revision,
    );
    label_bound_issue_read(access, destination, call, tool_signature, content, &origin)
}

/// A queued read belongs to its frozen request and coordination lease, not to
/// the active user input or that input's automation chain.
#[derive(Serialize)]
pub struct QueuedIssueReadOrigin<'a> {
    pub actor_id: &'a str,
    pub conversation_id: &'a str,
    pub request_id: &'a str,
    pub frozen_sha256: &'a str,
    pub batch_id: &'a str,
}

pub fn label_queued_issue_read(
    origin: &QueuedIssueReadOrigin<'_>,
    access: &IssueAccess,
    destination: &DestinationIdentity,
    call: &ToolCall,
    result: &IssueReadOutput,
) -> Result<DataEnvelope, AgentError> {
    if [
        origin.actor_id,
        origin.conversation_id,
        origin.request_id,
        origin.batch_id,
    ]
    .iter()
    .any(|value| value.is_empty() || value.len() > 512)
        || origin.frozen_sha256.len() != 64
        || !origin
            .frozen_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(invalid("Invalid queued external read origin"));
    }
    access.validate()?;
    label_bound_issue_read(
        access,
        destination,
        call,
        &result.tool_signature,
        &result.content,
        &("queued_input", origin),
    )
}

fn label_bound_issue_read(
    access: &IssueAccess,
    destination: &DestinationIdentity,
    call: &ToolCall,
    tool_signature: &str,
    content: &str,
    origin: &impl Serialize,
) -> Result<DataEnvelope, AgentError> {
    destination
        .validate()
        .map_err(|_| invalid("Invalid external read model destination"))?;
    if !matches!(destination, DestinationIdentity::Model { .. })
        || tool_signature.len() != 64
        || !tool_signature.bytes().all(|byte| byte.is_ascii_hexdigit())
        || content.is_empty()
        || content.len() > MAX_READ_RESULT_BYTES
        || call.id.is_empty()
        || call.id.len() > 512
    {
        return Err(invalid("External read lineage is unavailable"));
    }
    let digest = format!("{:x}", Sha256::digest(content));
    let identity = serde_json::to_vec(&(
        origin,
        &call.id,
        access,
        &call.arguments_json,
        tool_signature,
        &digest,
    ))
    .map_err(|_| invalid("External read lineage could not be encoded"))?;
    let id = format!("{PREFIX}{:x}", Sha256::digest(identity));
    let label = DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: id.clone(),
        content: ContentRef::ImmutableBlob {
            blob_id: format!("{CONTENT_PREFIX}{}", &digest[..32]),
            sha256: digest.clone(),
            size_bytes: content.len() as u64,
            media_type: MEDIA_TYPE.into(),
        },
        provenance: DataProvenance {
            source_provider_id: READ_PROVIDER_ID.into(),
            source_tool_name: TOOL_NAME.into(),
            source_object_id: Some(id),
            source_envelope_ids: Vec::new(),
        },
        digest_sha256: digest,
        sensitivity: if access.target.private || access.target.internal {
            Sensitivity::UserContent
        } else {
            Sensitivity::Public
        },
        allowed_destinations: vec![
            destination.clone(),
            access.target.destination(access.connection_ref.clone()),
        ],
        retention: RetentionBoundary {
            expires_at_unix_ms: None,
            delete_with_run: true,
        },
    };
    label
        .validate()
        .map_err(|_| invalid("Invalid external read result label"))?;
    Ok(label)
}

/// Recognizes a compiled whole-result contract, not an external tool's claimed
/// annotations. Native production is established by the saved tool message.
pub fn is_issue_read_label(label: &DataEnvelope) -> bool {
    if label.validate().is_err()
        || label.provenance.source_provider_id != READ_PROVIDER_ID
        || label.provenance.source_tool_name != TOOL_NAME
        || !label.provenance.source_envelope_ids.is_empty()
        || label.provenance.source_object_id.as_deref() != Some(label.envelope_id.as_str())
        || !label.envelope_id.strip_prefix(PREFIX).is_some_and(|hash| {
            hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        || label.retention.expires_at_unix_ms.is_some()
        || !label.retention.delete_with_run
    {
        return false;
    }
    let [
        DestinationIdentity::Model { .. },
        DestinationIdentity::ExternalIssue { audience, .. },
    ] = label.allowed_destinations.as_slice()
    else {
        return false;
    };
    let sensitivity = if *audience == ExternalIssueAudience::Public {
        Sensitivity::Public
    } else {
        Sensitivity::UserContent
    };
    label.sensitivity == sensitivity
        && matches!(&label.content,
        ContentRef::ImmutableBlob { blob_id,sha256,size_bytes,media_type }
        if blob_id == &format!("{CONTENT_PREFIX}{}", &label.digest_sha256[..32])
            && sha256 == &label.digest_sha256 && *size_bytes > 0 && *size_bytes <= MAX_READ_RESULT_BYTES as u64
            && media_type == MEDIA_TYPE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_label_binds_the_original_request_issue_tool_and_bytes() {
        let scope = desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        };
        let mut session = PersistedAgentSession::new(
            "work",
            "owner",
            "",
            1,
            scope.clone(),
            "2026-11-04T00:00:00Z",
        );
        session.input_revision = 1;
        let mut original = ReadContextSelection::capture(&[], &scope).unwrap();
        original.external_issues.push(IssueAccess {
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
        });
        let call=ToolCall { id:"read-1".into(),name:TOOL_NAME.into(),arguments_json:serde_json::json!({"tool_ref":original.external_issues[0].tool_ref(),"args":{"method":"get"}}).to_string() };
        let destination = DestinationIdentity::Model {
            connection_id: "model".into(),
            connection_revision: 1,
            model_id: "analysis".into(),
            profile_revision: 1,
        };
        let signature = "a".repeat(64);
        let first = label_issue_read(
            &session,
            &original,
            &destination,
            &call,
            &signature,
            "{\"title\":\"Issue\"}",
        )
        .unwrap();
        assert!(is_issue_read_label(&first));
        let policy = crate::ai_assistant::source_registry::registered_leaf(
            &first,
            1,
            crate::ai_assistant::source_registry::SOURCE_CONTRACT_REVISION,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            policy.contract,
            crate::ai_assistant::source_registry::RegisteredSourceContract::ExternalIssueResult
        );
        assert!(policy.policy.derived_visible_text_allowed);
        assert!(!policy.policy.independent_fact_allowed);
        assert_eq!(
            first.allowed_destinations[1],
            original.external_issues[0]
                .target
                .destination("forge".into())
        );
        let changed = label_issue_read(
            &session,
            &original,
            &destination,
            &call,
            &signature,
            "{\"title\":\"Edited\"}",
        )
        .unwrap();
        assert_ne!(first.envelope_id, changed.envelope_id);
        session.input_revision += 1;
        assert_ne!(
            first.envelope_id,
            label_issue_read(
                &session,
                &original,
                &destination,
                &call,
                &signature,
                "{\"title\":\"Issue\"}"
            )
            .unwrap()
            .envelope_id
        );
        assert!(
            label_issue_read(
                &session,
                &ReadContextSelection::capture(&[], &scope).unwrap(),
                &destination,
                &call,
                &signature,
                "{}"
            )
            .is_err()
        );
        let mut altered = first.clone();
        altered.sensitivity = Sensitivity::Public;
        assert!(!is_issue_read_label(&altered));
        altered = first;
        altered.allowed_destinations.pop();
        assert!(!is_issue_read_label(&altered));
    }
}
