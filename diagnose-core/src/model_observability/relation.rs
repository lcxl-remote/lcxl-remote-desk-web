//! Server-owned observation aliases; no business payload or provider call ID.

use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationAlias {
    key: String,
}

impl ObservationAlias {
    pub fn source_message(message_id: &str, ordinal: u32) -> Option<Self> {
        Self::new(format!("source.{message_id}.{ordinal}"))
    }
    pub fn provider_work(server_work_id: &str) -> Option<Self> {
        Self::new(format!("provider_work.{server_work_id}"))
    }
    pub fn command_work(server_action_id: &str) -> Option<Self> {
        Self::new(format!("command_work.{server_action_id}"))
    }
    pub fn command_attempt(server_work_id: &str, attempt: u32) -> Option<Self> {
        (attempt > 0)
            .then(|| Self::new(format!("command_attempt.{server_work_id}.{attempt}")))
            .flatten()
    }
    pub fn permission_request(server_run_id: &str, server_request_id: &str) -> Option<Self> {
        Self::scoped_request("permission", server_run_id, server_request_id)
    }
    pub fn goal_open_request(server_run_id: &str, server_request_id: &str) -> Option<Self> {
        Self::scoped_request("goal_open", server_run_id, server_request_id)
    }
    pub fn directory_request(server_run_id: &str, server_request_id: &str) -> Option<Self> {
        Self::scoped_request("directory", server_run_id, server_request_id)
    }
    pub fn model_call(server_call_id: &str) -> Option<Self> {
        Self::scoped_request("model_call", server_call_id, "0")
    }
    fn scoped_request(
        namespace: &str,
        server_run_id: &str,
        server_request_id: &str,
    ) -> Option<Self> {
        if server_run_id.is_empty()
            || server_request_id.is_empty()
            || server_run_id.len() > 256
            || server_request_id.len() > 256
        {
            return None;
        }
        if !server_run_id
            .bytes()
            .chain(server_request_id.bytes())
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-.:".contains(&byte))
        {
            return None;
        }
        // Production runs and requests are already long opaque server IDs.
        // Hash bounded length-prefixed identities, never model arguments or
        // content, so nodes independently derive the same fixed-size alias.
        let mut hash = Sha256::new();
        for part in [namespace, server_run_id, server_request_id] {
            hash.update((part.len() as u32).to_le_bytes());
            hash.update(part.as_bytes());
        }
        Self::new(format!("{namespace}.{:x}", hash.finalize()))
    }
    fn new(key: String) -> Option<Self> {
        let value = Self { key };
        value.is_bounded().then_some(value)
    }
    pub fn key(&self) -> &str {
        &self.key
    }
    pub fn object_id(&self) -> String {
        format!("binding.{}", self.key)
    }
    pub fn operation_id(&self, ordinal: u32) -> String {
        format!("operation.{}.{ordinal}", self.key)
    }
    pub fn is_bounded(&self) -> bool {
        !self.key.is_empty()
            && self.key.len() <= 144
            && self
                .key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "relation",
    content = "alias",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ObservationRelation {
    Bind(ObservationAlias),
    Link {
        source: ObservationAlias,
        target: ObservationAlias,
    },
    ResolveTool(ObservationAlias),
    ResolveOperation(ObservationAlias),
    ResolveStartedOperation(ObservationAlias),
    Correction {
        source: ObservationAlias,
        fact: correction::CorrectionFact,
    },
    ProtocolCorrection {
        source: ObservationAlias,
        fact: protocol_correction::ProtocolCorrectionFact,
    },
}

impl ObservationRelation {
    pub fn alias(&self) -> &ObservationAlias {
        match self {
            Self::Bind(alias)
            | Self::ResolveTool(alias)
            | Self::ResolveOperation(alias)
            | Self::ResolveStartedOperation(alias) => alias,
            Self::Link { source, .. }
            | Self::Correction { source, .. }
            | Self::ProtocolCorrection { source, .. } => source,
        }
    }
    pub fn source_object_id(&self) -> String {
        match self {
            Self::ProtocolCorrection { fact, .. } => fact.source_call_id().to_owned(),
            _ => self.alias().object_id(),
        }
    }
    pub fn accepts(&self, payload: &ObservationPayload) -> bool {
        self.alias().is_bounded()
            && match self {
                Self::Bind(_) | Self::ResolveTool(_) => {
                    matches!(payload, ObservationPayload::Tool(_))
                }
                Self::Link { target, .. } => {
                    target.is_bounded() && matches!(payload, ObservationPayload::Tool(_))
                }
                Self::Correction { fact, .. } => {
                    fact.is_bounded() && matches!(payload, ObservationPayload::Tool(_))
                }
                Self::ProtocolCorrection { source, fact } => {
                    fact.is_bounded()
                        && ObservationAlias::model_call(fact.source_call_id()).as_ref()
                            == Some(source)
                        && matches!(payload, ObservationPayload::Call(_))
                }
                Self::ResolveOperation(_) | Self::ResolveStartedOperation(_) => {
                    matches!(payload, ObservationPayload::Operation(_))
                }
            }
    }
}

impl ObservationEvent {
    /// Reuses a collected source-message association without reading storage or
    /// injecting observation state into durable business records.
    pub fn link_alias(
        source: ObservationAlias,
        target: ObservationAlias,
        occurred_at_ms: i64,
    ) -> Self {
        let mut event = Self::deferred_tool(
            source.clone(),
            ObservationPhase::Stage,
            0,
            occurred_at_ms,
            BTreeMap::new(),
            PermissionOutcome::NotReached,
            None,
            InputIssue::None,
        );
        let id = format!("association.{}", target.key());
        event.event_id = format!("{id}.link");
        event.object_id = id;
        event.relation = Some(ObservationRelation::Link { source, target });
        event
    }

    /// The writer restores the original attribution from a bounded shared alias.
    /// Missing association never falls back to the current model or current user.
    pub fn deferred_operation(
        alias: ObservationAlias,
        ordinal: u32,
        phase: ObservationPhase,
        sequence: u32,
        dispatched_at_ms: i64,
        occurred_at_ms: i64,
        snapshot: OperationSnapshot,
    ) -> Self {
        let id = alias.operation_id(ordinal);
        Self {
            schema_version: EVENT_SCHEMA_VERSION,
            event_id: format!("{id}.{}.{sequence}", phase as u8),
            object_id: id,
            call_id: None,
            phase,
            sequence,
            started_at_ms: dispatched_at_ms,
            occurred_at_ms,
            attribution: unknown_attribution(),
            payload: ObservationPayload::Operation(snapshot),
            relation: Some(ObservationRelation::ResolveOperation(alias)),
        }
    }

    /// A result cannot infer its cohort from mutable business timestamps. The
    /// worker resolves the start from an already collected dispatch observation.
    pub fn deferred_started_operation(
        alias: ObservationAlias,
        ordinal: u32,
        phase: ObservationPhase,
        sequence: u32,
        occurred_at_ms: i64,
        snapshot: OperationSnapshot,
    ) -> Self {
        let mut event = Self::deferred_operation(
            alias.clone(),
            ordinal,
            phase,
            sequence,
            0,
            occurred_at_ms,
            snapshot,
        );
        event.relation = Some(ObservationRelation::ResolveStartedOperation(alias));
        event
    }

    // Preserve independent stage, permission and input facts until source binding.
    #[allow(clippy::too_many_arguments)]
    pub fn deferred_tool(
        alias: ObservationAlias,
        phase: ObservationPhase,
        sequence: u32,
        occurred_at_ms: i64,
        stages: BTreeMap<Stage, StageOutcome>,
        permission: PermissionOutcome,
        input: Option<InputConclusion>,
        issue: InputIssue,
    ) -> Self {
        let id = format!("tool_patch.{}", alias.key());
        Self {
            schema_version: EVENT_SCHEMA_VERSION,
            event_id: format!("{id}.{}.{sequence}", phase as u8),
            object_id: id,
            call_id: None,
            phase,
            sequence,
            started_at_ms: 0,
            occurred_at_ms,
            attribution: unknown_attribution(),
            relation: Some(ObservationRelation::ResolveTool(alias)),
            payload: ObservationPayload::Tool(ToolSnapshot {
                ordinal: 0,
                tool_key: "unknown".into(),
                stages,
                conclusion: input.unwrap_or(InputConclusion::Unknown),
                issue,
                schema_path: None,
                permission,
                correction_of: None,
                correction_status: CorrectionStatus::Uncorrelated,
                correction_input: None,
                argument_bytes: 0,
                stage_duration_ms: None,
            }),
        }
    }
}

fn unknown_attribution() -> Attribution {
    Attribution::unresolved(Purpose::Agent, Surface::Assistant, Origin::Unknown)
}

/// Select only a unique original assistant call from already-validated history.
pub fn original_tool_alias(
    session: &crate::session::PersistedAgentSession,
    call_id: &str,
) -> Option<ObservationAlias> {
    if session.conversation.len() > 4_096
        || session
            .conversation
            .iter()
            .try_fold(0usize, |count, message| {
                count.checked_add(message.tool_calls.len())
            })?
            > 4_096
    {
        return None;
    }
    let mut matches = session
        .conversation
        .iter()
        .filter(|message| message.role == crate::chat::ChatRole::Assistant)
        .flat_map(|message| {
            message
                .tool_calls
                .iter()
                .enumerate()
                .filter(move |(_, call)| call.id == call_id)
                .map(move |(ordinal, _)| (&message.message_id, ordinal))
        });
    let (message, ordinal) = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    ObservationAlias::source_message(message, u32::try_from(ordinal).ok()?)
}
