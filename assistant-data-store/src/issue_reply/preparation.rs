//! Publication input is rebuilt from the original admitted snapshot, not a report.

use super::*;
use desk_diagnose_core::{
    chat::{ChatMessage, ToolCall},
    external_tools::IssueAccess,
    input_read_context::ReadContextSelection,
    issue_reply::{
        ReplyGeneration, ReplyGenerationInput, ReplyOrigin, ReplyPublication, ReplyToolReceipt,
    },
    session::PersistedAgentSession,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueReplyWork {
    pub origin: ReplyOrigin,
    pub access: IssueAccess,
    pub material: ChatMessage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preparation {
    Existing(ReplyToolReceipt),
    Generate(Box<IssueReplyWork>),
}

impl Preparation {
    pub fn into_generation(self, signature: String) -> ReplyGeneration {
        match self {
            Self::Existing(receipt) => ReplyGeneration::Existing(receipt),
            Self::Generate(work) => ReplyGeneration::Generate(Box::new(ReplyGenerationInput {
                publication: ReplyPublication {
                    origin: work.origin,
                    access: work.access,
                    comment_tool_signature: signature,
                },
                materials: vec![work.material],
            })),
        }
    }
}

pub fn tool_receipt(row: &reply::Model) -> ReplyToolReceipt {
    ReplyToolReceipt {
        artifact_id: row.artifact_id.clone(),
        state: row.state.clone(),
        reason: row.reason.clone(),
    }
}

/// The host checks the exact native input and pending tool call before and after
/// transport preflight; final model admission uses the same current snapshot.
pub async fn prepare_on(
    db: &impl ConnectionTrait,
    held: &PersistedAgentSession,
    original: &ReadContextSelection,
    call: &ToolCall,
    destination: &desk_agent_protocol::data_lineage::DestinationIdentity,
    now: i64,
) -> Result<Preparation, AssistantStoreError> {
    let access = desk_diagnose_core::issue_reply::resolve(&original.external_issues, call)
        .map_err(|_| invalid())?;
    let (mut snapshot, policy) =
        crate::issue_authority::resolve_input(db, held, original, access, now)
            .await?
            .ok_or(AssistantStoreError::Inaccessible)?;
    if !snapshot.can_publish(&policy, now)
        || !crate::issue_events::supplements::publication_content_current(db, &snapshot, &policy)
            .await?
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let origin = ReplyOrigin {
        actor_id: held.actor_id.clone(),
        root_conversation_id: snapshot.subscription.conversation_id.clone(),
        request_id: snapshot.request_id.clone(),
        execution_conversation_id: held.conversation_id.clone(),
        input_revision: held.input_revision,
        control_revision: held.control_revision,
        tool_call_id: call.id.clone(),
        authorization_id: snapshot.authorization_id.clone(),
        authorization_revision: snapshot.subscription.revision,
    };
    let id = desk_diagnose_core::issue_reply::artifact_id(
        &held.actor_id,
        &held.conversation_id,
        held.input_revision,
        &call.id,
    );
    if let Some(row) = load(db, &held.actor_id, &id).await? {
        if row.request_id != origin.request_id
            || row.root_conversation_id != origin.root_conversation_id
            || row.execution_conversation_id != origin.execution_conversation_id
            || row.input_revision != i64::try_from(origin.input_revision).map_err(|_| invalid())?
            || receipt_key(&row)?.access != *access
        {
            return Err(invalid());
        }
        return Ok(Preparation::Existing(tool_receipt(&row)));
    }
    crate::issue_events::supplements::include_on(db, &mut snapshot, &policy, true).await?;
    Ok(Preparation::Generate(Box::new(IssueReplyWork {
        origin,
        access: access.clone(),
        material: snapshot
            .publication_material(destination)
            .map_err(|_| invalid())?,
    })))
}
