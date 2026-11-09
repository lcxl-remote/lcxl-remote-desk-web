//! Visible main-assistant history is prepared before the native session CAS.
//! Original model acceptance proves production; it grants no new model use.

use super::history::staged::CaptureRequest;
use super::{AssistantStoreError, unsigned};
use crate::config::connection::DatabaseTransaction;
use crate::entity::agent_session;
use desk_agent_protocol::{
    ai_assistant::{
        source::{HistoryDependency, SourcePolicyClass, SourceUsePolicy},
        user_instructions::AssistantDataPolicy,
    },
    data_lineage::DataEnvelope,
};
use desk_diagnose_core::{
    ai_assistant::{
        data_access::eligible_origin,
        output_archive::ArchivedAssistantOutput,
        source::{HistoryCapture, HistorySourceDirectory},
        source_registry::is_native_output_policy_source,
    },
    chat::{ChatMessage, ChatRole},
    session::PersistedAgentSession,
};
use sea_orm::TransactionTrait;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) mod citations;
mod fence;
pub(crate) mod native;
mod snapshot;
pub(crate) use native::retained_sources as retained_native_sources;

struct Draft {
    role: &'static str,
    kind: desk_agent_protocol::ai_assistant::source::AssistantSourceKind,
    native_id: String,
    content: String,
    envelope: DataEnvelope,
    policy: SourceUsePolicy,
    dependencies: Vec<HistoryDependency>,
    recorded_at: u64,
    input_seq: u64,
    timezone: String,
    omit_policy: bool,
    citations: desk_diagnose_core::ai_assistant::citation::AnswerCitations,
}

pub(crate) struct PreparedVisibleHistory<'a> {
    pub original: Box<agent_session::Model>,
    expected: &'a PersistedAgentSession,
    pub policy: AssistantDataPolicy,
    drafts: Vec<Draft>,
    needs_live_lease: bool,
    native_status: Option<Box<super::native_status::PreparedNativeStatus>>,
    instance: String,
}

impl PreparedVisibleHistory<'_> {
    /// Observe native rows after the caller's original child-result or other
    /// native writes, before captures are prepared and final quota is acquired.
    pub(crate) async fn observe_native_status_on(
        &mut self,
        db: &DatabaseTransaction,
    ) -> Result<(), AssistantStoreError> {
        if !self.expected.has_admitted_personal_input()
            && self
                .expected
                .assistant_data
                .as_ref()
                .is_some_and(|data| !data.temporary)
        {
            prepare_native_status_on(db, self).await?;
        }
        Ok(())
    }

    pub(crate) fn captures(&self) -> Vec<HistoryCapture<'_>> {
        let data = self
            .expected
            .assistant_data
            .as_ref()
            .expect("prepared native assistant scope");
        let mut captures = self
            .drafts
            .iter()
            .map(|draft| HistoryCapture {
                native: None,
                actor_id: &self.expected.actor_id,
                device_ref: &data.source_device_ref,
                directory: (!data.source_device_ref.is_empty()).then_some(HistorySourceDirectory {
                    instance_id: &self.instance,
                    audience: &self.expected.device_id,
                    device_name: None,
                }),
                conversation_id: &self.expected.conversation_id,
                root_conversation_id: &self.expected.conversation_id,
                native_id: &draft.native_id,
                content: &draft.content,
                envelope: &draft.envelope,
                source_policy: &draft.policy,
                dependencies: &draft.dependencies,
                accepted_input_seq: draft.input_seq,
                kind: draft.kind,
                role: draft.role,
                recorded_at_unix_ms: draft.recorded_at,
                statement_timezone: &draft.timezone,
                revision: 1,
            })
            .collect::<Vec<_>>();
        if let Some(status) = &self.native_status {
            captures.push(status.projection.capture(
                data.current_actor_input_seq,
                Some(HistorySourceDirectory {
                    instance_id: &self.instance,
                    audience: &self.expected.device_id,
                    device_name: None,
                }),
            ));
        }
        captures
    }

    pub(crate) fn requests<'a>(
        &self,
        captures: &'a [HistoryCapture<'a>],
    ) -> Result<Vec<CaptureRequest<'a>>, AssistantStoreError> {
        if captures.len() != self.drafts.len() + usize::from(self.native_status.is_some()) {
            return Err(AssistantStoreError::Inaccessible);
        }
        Ok(captures
            .iter()
            .enumerate()
            .map(|(index, capture)| CaptureRequest {
                capture,
                omit_policy: self
                    .drafts
                    .get(index)
                    .is_some_and(|draft| draft.omit_policy),
            })
            .collect())
    }

    pub(crate) fn deadlines(&self) -> Result<Vec<Option<u64>>, AssistantStoreError> {
        let mut deadlines = self
            .drafts
            .iter()
            .map(|draft| {
                if draft.omit_policy {
                    return Ok(None);
                }
                let natural = draft
                    .recorded_at
                    .checked_add(u64::from(self.policy.history_retention_days) * 86_400_000)
                    .ok_or(AssistantStoreError::Capacity)?;
                Ok(Some(
                    draft
                        .dependencies
                        .iter()
                        .filter_map(|source| source.expires_at_unix_ms)
                        .chain(draft.policy.content_hard_expires_at_unix_ms)
                        .fold(natural, u64::min),
                ))
            })
            .collect::<Result<Vec<_>, AssistantStoreError>>()?;
        if let Some(status) = &self.native_status {
            let capture = status.projection.capture(0, None);
            deadlines.push(Some(
                capture
                    .recorded_at_unix_ms
                    .checked_add(u64::from(self.policy.history_retention_days) * 86_400_000)
                    .ok_or(AssistantStoreError::Capacity)?,
            ));
        }
        Ok(deadlines)
    }

    pub(crate) async fn write_citations_on(
        &self,
        db: &DatabaseTransaction,
    ) -> Result<(), AssistantStoreError> {
        let mut rows = Vec::new();
        for draft in &self.drafts {
            if draft.citations.citations.is_empty() && !draft.citations.incomplete {
                continue;
            }
            let expiry = draft
                .recorded_at
                .checked_add(u64::from(self.policy.history_retention_days) * 86_400_000)
                .ok_or(AssistantStoreError::Capacity)?;
            let expiry = draft
                .dependencies
                .iter()
                .filter_map(|source| source.expires_at_unix_ms)
                .chain(draft.policy.content_hard_expires_at_unix_ms)
                .fold(expiry, u64::min);
            rows.push(super::citations::new_row(
                &self.expected.actor_id,
                &self.expected.conversation_id,
                &draft.native_id,
                &draft.envelope.envelope_id,
                &draft.citations,
                draft.recorded_at,
                expiry,
            )?);
        }
        if rows.is_empty() {
            return Ok(());
        }
        let savepoint = db.begin().await?;
        match super::citations::write_on(&savepoint, &self.expected.actor_id, rows, &self.policy)
            .await
        {
            Ok(()) => {
                savepoint.commit().await?;
                Ok(())
            }
            Err(error) => {
                savepoint.rollback().await?;
                Err(error)
            }
        }
    }

    /// Re-select the original row and device before the native CAS, preserving
    /// raw JSON/version/lease predicates even while the actor fence is held.
    pub(crate) async fn finish_preparation_on(
        &self,
        db: &DatabaseTransaction,
    ) -> Result<Option<u64>, AssistantStoreError> {
        if let Some(status) = &self.native_status {
            status.validate_on(db, self.expected).await?;
        }
        fence::finish_on(db, self.expected, &self.original, self.needs_live_lease).await
    }
}

pub(crate) async fn prepare_on<'a>(
    db: &DatabaseTransaction,
    expected: &'a PersistedAgentSession,
) -> Result<Box<PreparedVisibleHistory<'a>>, AssistantStoreError> {
    let (original, persisted, policy) = fence::prepare_on(db, expected).await?;
    let instance = expected
        .assistant_data
        .as_ref()
        .and_then(|data| data.source_instance_id.clone())
        .ok_or(AssistantStoreError::Inaccessible)?;
    let data = expected
        .assistant_data
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    let mut prepared = Box::new(PreparedVisibleHistory {
        original,
        expected,
        policy,
        drafts: Vec::new(),
        needs_live_lease: false,
        native_status: None,
        instance,
    });
    if data.temporary {
        return Ok(prepared);
    }
    if !eligible_origin(&persisted) || !eligible_origin(expected) {
        // Native status remains an owner history observation after a main
        // trigger/focus closes. This does not admit model tools or answers.
        return Ok(prepared);
    }
    let old = persisted
        .conversation
        .iter()
        .map(|message| (message.message_id.as_str(), message))
        .collect::<BTreeMap<_, _>>();
    if old.len() != persisted.conversation.len() {
        return Err(AssistantStoreError::Invalid(
            "duplicate original saved message identity",
        ));
    }
    let mut message_ids = BTreeSet::new();
    let mut outputs = BTreeSet::new();
    for message in &expected.conversation {
        if !message_ids.insert(message.message_id.as_str()) {
            return Err(AssistantStoreError::Invalid(
                "duplicate saved message identity",
            ));
        }
        if message.data_envelope.as_ref().is_some_and(|label| {
            label.provenance.source_provider_id == "assistant-conversation-snapshot"
        }) {
            if let Some(previous) = old.get(message.message_id.as_str()) {
                if previous != &message {
                    return Err(AssistantStoreError::Inaccessible);
                }
            } else {
                prepared.needs_live_lease = true;
                prepared
                    .drafts
                    .push(snapshot::prepare_on(db, expected, message, &prepared.policy).await?);
            }
            continue;
        }
        let Some(output) = message
            .data_envelope
            .as_ref()
            .filter(|label| is_native_output_policy_source(label))
        else {
            continue;
        };
        if message.role != ChatRole::Assistant {
            continue;
        }
        if let Some(previous) = old.get(message.message_id.as_str()) {
            if previous.role != message.role
                || previous.text != message.text
                || previous.tool_calls != message.tool_calls
                || previous.data_envelope != message.data_envelope
            {
                return Err(AssistantStoreError::Inaccessible);
            }
            continue;
        }
        prepared.needs_live_lease = true;
        let stored = super::output_archive::find_on(db, &expected.conversation_id, output)
            .await?
            .ok_or_else(|| AssistantStoreError::Inaccessible)?;
        let proof = &stored.proof;
        if proof.source_set.actor_id != expected.actor_id
            || proof.source_set.source_device_ref != data.source_device_ref
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        let actual =
            crate::model_egress_store::SignalModelEgressStore::read_context_output_reusing_on(
                db,
                &expected.conversation_id,
                output,
                Some(&stored),
            )
            .await?;
        if actual != proof.sources {
            return Err(AssistantStoreError::Inaccessible);
        }
        match proof.bind_visible_message(message) {
            Ok(_) => {}
            Err(_) if control_display_witness(expected, proof, message) => {
                prepared.drafts.push(draft(
                    message,
                    proof,
                    unresolved_policy(proof.source_set.context.policy_revision)?,
                    Vec::new(),
                    true,
                    Vec::new(),
                    &data.limits.conversation,
                ));
                continue;
            }
            Err(_) => return Err(AssistantStoreError::Inaccessible),
        };
        proof
            .source_set
            .context
            .validate_session(&persisted)
            .map_err(AssistantStoreError::Invalid)?;
        proof
            .source_set
            .context
            .validate_session(expected)
            .map_err(AssistantStoreError::Invalid)?;
        if !outputs.insert(output.envelope_id.clone()) {
            return Err(AssistantStoreError::Inaccessible);
        }
        let now = unsigned(chrono::Utc::now().timestamp_millis())?;
        let (policy, mut omit_policy, read_roots) = match native::retained_sources(proof) {
            Some(value) => (value.policy, false, value.read_roots),
            None => (
                unresolved_policy(proof.source_set.context.policy_revision)?,
                true,
                native::citation_roots_on(db, proof).await?,
            ),
        };
        let natural = proof
            .sources
            .evidence
            .completed_at_unix_ms
            .checked_add(u64::from(prepared.policy.history_retention_days) * 86_400_000)
            .ok_or(AssistantStoreError::Capacity)?;
        if natural <= now
            || policy
                .content_hard_expires_at_unix_ms
                .is_some_and(|until| until <= now)
        {
            omit_policy = true;
        }
        if !message.text.trim().is_empty() {
            prepared.drafts.push(draft(
                message,
                proof,
                policy,
                Vec::new(),
                omit_policy,
                read_roots,
                &data.limits.conversation,
            ));
        }
    }
    Ok(prepared)
}

async fn prepare_native_status_on(
    db: &DatabaseTransaction,
    prepared: &mut PreparedVisibleHistory<'_>,
) -> Result<(), AssistantStoreError> {
    let expected = prepared.expected;
    let data = expected
        .assistant_data
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    if prepared.drafts.len() < prepared.policy.limits.processing.history_capture_batch_size as usize
    {
        let status = desk_diagnose_core::future::boxed(|| {
            super::native_status::prepare_on(db, expected, prepared.policy.revision)
        })
        .await?;
        let capture = status
            .projection
            .capture(data.current_actor_input_seq, None);
        let expires = capture
            .recorded_at_unix_ms
            .checked_add(u64::from(prepared.policy.history_retention_days) * 86_400_000)
            .ok_or(AssistantStoreError::Capacity)?;
        let origin_id = status.projection.origin_id();
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        let existing = crate::entity::agent_assistant_origin::Entity::find()
            .filter(crate::entity::agent_assistant_origin::Column::ActorId.eq(&expected.actor_id))
            .filter(crate::entity::agent_assistant_origin::Column::OriginId.eq(origin_id))
            .one(db)
            .await?;
        if existing.is_none() && expires > unsigned(chrono::Utc::now().timestamp_millis())? {
            prepared.native_status = Some(status);
        }
    }
    Ok(())
}

fn draft(
    message: &ChatMessage,
    proof: &ArchivedAssistantOutput,
    policy: SourceUsePolicy,
    dependencies: Vec<HistoryDependency>,
    omit_policy: bool,
    read_roots: Vec<desk_diagnose_core::ai_assistant::seam::AssistantReadRoot>,
    limits: &desk_agent_protocol::ai_assistant::limits::AssistantConversationLimits,
) -> Draft {
    let citations = desk_diagnose_core::ai_assistant::citation::for_answer(
        &message.message_id,
        &message.text,
        &read_roots,
        limits,
    );
    Draft {
        role: "assistant",
        kind: desk_agent_protocol::ai_assistant::source::AssistantSourceKind::AssistantMessage,
        native_id: message.message_id.clone(),
        content: message.text.clone(),
        envelope: proof.output.clone(),
        policy,
        dependencies,
        recorded_at: proof.sources.evidence.completed_at_unix_ms,
        input_seq: proof.actor_input_seq,
        timezone: proof.statement_timezone.clone(),
        omit_policy,
        citations,
    }
}

pub(crate) fn unresolved_policy(revision: u64) -> Result<SourceUsePolicy, AssistantStoreError> {
    let policy = SourceUsePolicy {
        policy_revision: revision,
        class: SourcePolicyClass::Unknown,
        history_retention_allowed: false,
        independent_fact_allowed: false,
        derived_visible_text_allowed: false,
        no_copy: true,
        no_export: true,
        content_hard_expires_at_unix_ms: None,
        reference_fresh_until_unix_ms: None,
        fixed_destinations: Vec::new(),
    };
    Ok(policy)
}

/// A goal-control display has its original exact tool-call message as witness.
/// Its different visible text is not a second original model response.
fn control_display_witness(
    session: &PersistedAgentSession,
    proof: &ArchivedAssistantOutput,
    display: &ChatMessage,
) -> bool {
    if !display.tool_calls.is_empty()
        || display.tool_call_id.is_some()
        || display.image_data_url.is_some()
        || proof.input_revision != session.input_revision
        || session
            .assistant_data
            .as_ref()
            .is_none_or(|data| proof.actor_input_seq != data.current_actor_input_seq)
        || proof.source_set.context.lease_token != session.lease_token
    {
        return false;
    }
    session.conversation.iter().any(|message| {
        message.message_id != display.message_id
            && message.model_controls.is_some()
            && message
                .tool_calls
                .iter()
                .any(|call| call.name == desk_diagnose_core::goal_tools::CONTROL_GOAL_TOOL_NAME)
            && proof.bind_visible_message(message).is_ok()
    })
}
