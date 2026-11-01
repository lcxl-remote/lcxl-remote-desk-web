//! Fit optional current facts into an already selected required context view.
//! Durable source references are saved before a labelled body enters a request.

use super::*;
use crate::{
    ai_assistant::{
        current_memory::{self, CurrentMemoryPayload},
        data_access::eligible_origin,
        data_control::CurrentMemoryUse,
        memory_access::usable_memory_scope,
        seam::{AssistantDataReadUse, CurrentMemoryBlock},
    },
    native_ids::{self, Kind},
};
use desk_agent_protocol::ai_assistant::memory::MemoryUsePurpose;

pub(in crate::agent_loop) async fn prepare(
    deps: &LoopDeps<'_>,
    session: &mut PersistedAgentSession,
    request: &mut ModelRequest,
    turn_id: &str,
) -> Result<usize, AgentError> {
    if request.use_case != crate::model_profile::ModelUseCase::Agent
        || request.compression_identity.is_some()
        || deps.model.command_completion_event_id().is_some()
        || !eligible_origin(session)
        || deps.session_seam.memory().is_none()
    {
        return Ok(0);
    }
    if request.context_conversation_id.as_deref() != Some(session.conversation_id.as_str()) {
        return Err(changed(
            "current memory preparation changed its original request",
        ));
    }
    let now = current_unix_ms(deps.clock)?;
    let access = fresh_access(deps, session, now).await?;
    if !access.settings.automatic_memory_enabled || usable_memory_scope(session, now).is_none() {
        if session
            .assistant_data
            .as_ref()
            .is_some_and(|data| !data.current_memories.is_empty())
        {
            return Err(changed(
                "current memory references need source reconciliation",
            ));
        }
        request
            .messages
            .retain(|message| !current_memory::is_projection(message));
        request.preparation = None;
        request.assistant_data = None;
        return Ok(0);
    }
    let mut base = Box::new(request.clone());
    base.messages
        .retain(|message| !current_memory::is_projection(message));
    base.preparation = None;
    base.assistant_data = None;
    let Some(baseline) = deps.model.prepare_request(&base)? else {
        // An optional read cannot consume unmeasured provider context. The
        // original request keeps its existing projection and budget contracts.
        omit(deps, session, request, base).await?;
        return Ok(0);
    };
    baseline.require_fit()?;
    let available = baseline
        .maximum_context_bytes
        .saturating_sub(baseline.context_bytes);
    let mut maximum = available.min(baseline.maximum_context_bytes / 2);
    if maximum == 0 {
        omit(deps, session, request, base).await?;
        return Ok(0);
    }
    let policy = deps
        .model
        .model_egress_policy()?
        .ok_or_else(|| changed("current memory requires an audited model destination"))?;
    // A wire mismatch gets one smaller preparation using the measured excess.
    // Both attempts stay within the original context budget and turn.
    for _ in 0..2 {
        let data = session
            .assistant_data
            .as_ref()
            .ok_or_else(|| changed("current memory has no original assistant state"))?;
        let preparation_id = native_ids::stable(
            Kind::AssistantReadProjection,
            &(
                "automatic-current-memory-v2",
                &session.actor_id,
                &session.conversation_id,
                turn_id,
                session.input_revision,
                data.current_actor_input_seq,
                &data.retrieval_budget.episode_id,
                access.policy_revision,
                access.memory_state_generation,
                access.control_generation,
                &policy.destination,
                maximum,
            ),
        );
        let block = deps
            .session_seam
            .memory()
            .ok_or_else(|| changed("current memory storage disappeared"))?
            .current_memories(session, &policy.destination, &preparation_id, maximum, now)
            .await?;
        let projected = project(
            session,
            &access,
            block,
            maximum,
            current_unix_ms(deps.clock)?,
        )?;
        let Some(message) = &projected.message else {
            omit(deps, session, request, base).await?;
            return Ok(0);
        };
        let mut candidate = base.clone();
        let mut at = usize::from(
            candidate
                .messages
                .first()
                .is_some_and(|message| message.role == ChatRole::System),
        );
        while candidate
            .messages
            .get(at)
            .is_some_and(crate::ai_assistant::settings::is_personalization_projection)
        {
            at += 1;
        }
        candidate.messages.insert(at, message.clone());
        let after = deps
            .model
            .prepare_request(&candidate)?
            .ok_or_else(|| changed("current memory lost the exact model projection"))?;
        if after.maximum_context_bytes != baseline.maximum_context_bytes {
            return Err(changed(
                "current memory changed the pinned model context budget",
            ));
        }
        if after.context_bytes > after.maximum_context_bytes {
            maximum = current_memory::framed_bytes(&message.text)
                .map_err(changed)?
                .saturating_sub(after.context_bytes - after.maximum_context_bytes);
            if maximum == 0 {
                break;
            }
            continue;
        }
        if !after.source_message_ids.contains(&message.message_id) {
            return Err(changed(
                "current memory was omitted from the actual model projection",
            ));
        }
        let (references, dependencies) = prepare_uses(session, projected.uses)?;
        let data = session
            .assistant_data
            .as_mut()
            .ok_or_else(|| changed("current memory lost its source session"))?;
        let dirty = data.current_memories != projected.memories
            || data.read_uses != references
            || data.history_dependencies != dependencies;
        if dirty {
            data.current_memories = projected.memories;
            data.read_uses = references;
            data.history_dependencies = dependencies;
            // Candidate measurements are pure. Publish the body only after the
            // original source-reference CAS succeeds; no model call occurs here.
            deps.session_seam.save(session).await?;
        }
        *request = *candidate;
        return Ok(after.context_bytes.saturating_sub(baseline.context_bytes));
    }
    omit(deps, session, request, base).await?;
    Ok(0)
}

async fn omit(
    deps: &LoopDeps<'_>,
    session: &mut PersistedAgentSession,
    request: &mut ModelRequest,
    base: Box<ModelRequest>,
) -> Result<(), AgentError> {
    let data = session
        .assistant_data
        .as_mut()
        .ok_or_else(|| changed("optional current memory lost its source session"))?;
    if !data.current_memories.is_empty() {
        data.current_memories.clear();
        // Existing historical/producer dependencies survive. Omission is not
        // forgetting, source revocation or permission to relabel older output.
        deps.session_seam.save(session).await?;
    }
    *request = *base;
    Ok(())
}

struct ProjectedCurrent {
    message: Option<ChatMessage>,
    memories: Vec<CurrentMemoryUse>,
    uses: Vec<AssistantDataReadUse>,
}

fn project(
    session: &PersistedAgentSession,
    access: &AssistantDataToolAccess,
    block: CurrentMemoryBlock,
    maximum: usize,
    now: u64,
) -> Result<ProjectedCurrent, AgentError> {
    let data = session
        .assistant_data
        .as_ref()
        .ok_or_else(|| changed("current memory has no source session"))?;
    if block.memory_state_generation != access.memory_state_generation
        || block.memory_state_generation != data.memory_state_generation
        || access.control_generation != data.control_generation
        || usable_memory_scope(session, now).is_none()
    {
        return Err(changed(
            "current memory changed its original generation or consent",
        ));
    }
    if block.records.is_empty() {
        return Ok(ProjectedCurrent {
            message: None,
            memories: Vec::new(),
            uses: Vec::new(),
        });
    }
    let content = CurrentMemoryPayload::new(block.memory_state_generation, block.records.clone())
        .encode(maximum, now)
        .map_err(changed)?
        .ok_or_else(|| changed("current memory exceeds its framed byte limit"))?;
    let usage = block
        .uses
        .into_iter()
        .next()
        .ok_or_else(|| changed("current memory source use is missing"))?;
    let envelope = block
        .source_envelopes
        .into_iter()
        .next()
        .ok_or_else(|| changed("current memory label is missing"))?;
    let memories = block
        .records
        .iter()
        .map(|record| CurrentMemoryUse {
            memory_id: record.memory_id.clone(),
            revision: record.revision,
            purpose: MemoryUsePurpose::CurrentFact,
        })
        .collect::<Vec<_>>();
    let mut message = ChatMessage::text(
        native_ids::stable(
            Kind::Message,
            &("current-memory-message-v1", &envelope.envelope_id),
        ),
        ChatRole::User,
        content,
    );
    message.turn_id = session.current_turn_id.clone();
    message.data_envelope = Some(envelope);
    Ok(ProjectedCurrent {
        message: Some(message),
        memories,
        uses: vec![usage],
    })
}

fn changed(message: &'static str) -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message: message.into(),
        retryable: true,
        safe_for_model: false,
        error_code: Some(desk_utils::error::DeskErrorCode::AI_ASSISTANT_DATA_CHANGED.code()),
    }
}

#[cfg(test)]
mod tests;

fn prepare_uses(
    session: &PersistedAgentSession,
    uses: Vec<AssistantDataReadUse>,
) -> Result<
    (
        Vec<crate::ai_assistant::seam::AssistantDataProjectionRef>,
        Vec<desk_agent_protocol::ai_assistant::source::HistoryDependency>,
    ),
    AgentError,
> {
    let data = session
        .assistant_data
        .as_ref()
        .ok_or_else(|| invalid("missing assistant data state"))?;
    let mut combined = data.read_uses.clone();
    let mut dependencies = data.history_dependencies.clone();
    for usage in uses {
        for dependency in &usage.history_dependencies {
            if !dependencies.iter().any(|old| {
                old.source_id == dependency.source_id
                    && old.source_revision == dependency.source_revision
            }) {
                dependencies.push(dependency.clone());
            }
        }
        let reference = usage.reference();
        if !combined.iter().any(|old| {
            old.projection_id == usage.projection_id
                && old.source_envelope_id == usage.source_envelope_id
        }) {
            combined.push(reference);
        }
    }
    Ok((combined, dependencies))
}
