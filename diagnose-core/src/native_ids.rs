//! Native business identities: compact values, never aliases for longer IDs.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Task,
    ChildConversation,
    Worker,
    BackgroundAction,
    ExecRequest,
    InteractiveExecRequest,
    Message,
    Schedule,
    Preview,
    Attachment,
    PermissionRequest,
    DirectoryRequest,
    GoalOpenRequest,
    Goal,
    StatusStep,
    Lineage,
    AssistantOrigin,
    AssistantSearchDocument,
    Memory,
    MemoryJob,
    RetrievalEpisode,
    AssistantDataControl,
    AssistantReadAuthorization,
    AssistantReadProjection,
    AssistantDeviceBinding,
    AssistantRetrievalQuery,
    AssistantPageCursor,
    AssistantMainRequest,
    WorkbenchWorkConversation,
    IssueReply,
    IssueSubscription,
    IssueAdmission,
    IssueComment,
    IssueWork,
    SubagentEvent,
    WorkerReport,
    DelegationGroup,
    TaskBudgetReservation,
    DelegationReservation,
}

impl Kind {
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Task => "t",
            Self::ChildConversation => "sa",
            Self::Worker => "worker",
            Self::BackgroundAction => "j",
            Self::ExecRequest => "ex",
            Self::InteractiveExecRequest => "xp",
            Self::Message => "m",
            Self::Schedule => "sc",
            Self::Preview => "v",
            Self::Attachment => "att",
            Self::PermissionRequest => "rq",
            Self::DirectoryRequest => "dir",
            Self::GoalOpenRequest => "gr",
            Self::Goal => "g",
            Self::StatusStep => "step",
            Self::Lineage => "id",
            Self::AssistantSearchDocument => "sdoc",
            Self::AssistantOrigin => "origin",
            Self::Memory => "mem",
            Self::MemoryJob => "mjob",
            Self::RetrievalEpisode => "lookup",
            Self::AssistantDataControl => "dc",
            Self::AssistantReadAuthorization => "read",
            Self::AssistantReadProjection => "proj",
            Self::AssistantDeviceBinding => "dev",
            Self::AssistantRetrievalQuery => "query",
            Self::AssistantPageCursor => "page",
            Self::AssistantMainRequest => "request",
            Self::WorkbenchWorkConversation => "work",
            Self::IssueReply => "reply",
            Self::IssueSubscription => "subscription",
            Self::IssueAdmission => "admission",
            Self::IssueComment => "comment",
            Self::IssueWork => "issue-work",
            Self::SubagentEvent => "sev",
            Self::WorkerReport => "wr",
            Self::DelegationGroup => "dg",
            Self::TaskBudgetReservation => "tb",
            Self::DelegationReservation => "dr",
        }
    }
}

/// Encode all bytes supplied by the owning generator. No UUID string is stored.
pub fn from_bytes(kind: Kind, bytes: &[u8; 16]) -> String {
    format!("{}_{}", kind.prefix(), URL_SAFE_NO_PAD.encode(bytes))
}

/// Derive an independent business ID; full authority and idempotency keys remain
/// in the original records. Callers must reject mismatching complete bindings.
pub fn stable(kind: Kind, identity: &impl Serialize) -> String {
    let input = serde_json::to_vec(&("native-id:v1", kind.prefix(), identity))
        .expect("server-owned native identity serialization");
    let digest = Sha256::digest(input);
    let bytes: [u8; 16] = digest[..16].try_into().expect("128-bit business identity");
    from_bytes(kind, &bytes)
}

/// Deterministic history source id for tests and fixtures. Production source
/// ids are database primary keys formatted by `SourceRef`.
#[doc(hidden)]
pub fn fixture_source_id(seed: &impl Serialize) -> String {
    let input = serde_json::to_vec(&("fixture-source", seed)).expect("fixture seed serialization");
    let digest = Sha256::digest(input);
    let key = u64::from_be_bytes(digest[..8].try_into().expect("eight bytes")) % 1_000_000_000;
    desk_agent_protocol::ai_assistant::source_ref::SourceRef::history(key as i64 + 1).to_string()
}

/// Deterministic memory evidence source id for tests and fixtures.
#[doc(hidden)]
pub fn fixture_memory_source_id(seed: &impl Serialize) -> String {
    let history = fixture_source_id(seed);
    let parsed = desk_agent_protocol::ai_assistant::source_ref::SourceRef::parse(&history)
        .expect("fixture history source id");
    desk_agent_protocol::ai_assistant::source_ref::SourceRef::memory(parsed.id).to_string()
}

pub fn is(kind: Kind, id: &str) -> bool {
    let Some(body) = id
        .strip_prefix(kind.prefix())
        .and_then(|v| v.strip_prefix('_'))
    else {
        return false;
    };
    body.len() == 22
        && URL_SAFE_NO_PAD
            .decode(body)
            .is_ok_and(|bytes| bytes.len() == 16 && URL_SAFE_NO_PAD.encode(bytes) == body)
}

pub fn background_action(conversation: &str, turn: &str, call: &str) -> String {
    stable(Kind::BackgroundAction, &(conversation, turn, call))
}

pub fn workbench_work_conversation(actor: &str, workbench: &str, request: &str) -> String {
    stable(
        Kind::WorkbenchWorkConversation,
        &(actor, workbench, request),
    )
}

pub fn capability_completion(call: &str) -> String {
    stable(Kind::Message, &("capability-completion", call))
}

pub fn scheduled_message(run: &str) -> String {
    stable(Kind::Message, &("scheduled-task-input", run))
}

pub fn rehearsal_message(rehearsal: &str) -> String {
    stable(Kind::Message, &("rehearsal-input", rehearsal))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_representation_matches_web_android_and_ios() {
        let bytes = std::array::from_fn(|index| index as u8);
        assert_eq!(
            from_bytes(Kind::Message, &bytes),
            "m_AAECAwQFBgcICQoLDA0ODw"
        );
        assert!(is(Kind::Message, &scheduled_message("run")));
        assert_ne!(scheduled_message("run"), rehearsal_message("run"));
        assert!(is(Kind::Message, &capability_completion("call")));
        assert_eq!(capability_completion("call"), capability_completion("call"));
        assert_ne!(
            capability_completion("call"),
            capability_completion("other-call")
        );
    }

    #[test]
    fn random_material_is_encoded_without_losing_any_bits() {
        let bytes = std::array::from_fn(|index| (index as u8).wrapping_mul(17));
        let id = from_bytes(Kind::Message, &bytes);
        assert!(is(Kind::Message, &id));
        assert_eq!(
            URL_SAFE_NO_PAD
                .decode(id.split_once('_').unwrap().1)
                .unwrap(),
            bytes
        );
        assert_eq!(id.len(), 24);
        assert!(!is(Kind::Task, &id));
        assert!(!is(Kind::Message, &(id + "=")));
    }

    #[test]
    fn stable_identity_preserves_complete_scope_and_separates_business_types() {
        let source = ("root", "group", "actor", "device", "call");
        let task = stable(Kind::Task, &source);
        assert_eq!(task, stable(Kind::Task, &source));
        assert!(is(Kind::Task, &task));
        assert_ne!(task, stable(Kind::ChildConversation, &source));
        for changed in [
            ("other", "group", "actor", "device", "call"),
            ("root", "other", "actor", "device", "call"),
            ("root", "group", "other", "device", "call"),
            ("root", "group", "actor", "other", "call"),
            ("root", "group", "actor", "device", "other"),
        ] {
            assert_ne!(task, stable(Kind::Task, &changed));
        }
        assert_ne!(
            stable(Kind::Attachment, &("a:b", "c")),
            stable(Kind::Attachment, &("a", "b:c"))
        );
    }
}
