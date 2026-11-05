//! Publication is an auxiliary physical call on the original work, not a new task.

use super::*;
use crate::issue_reply::{ReplyGeneration, ReplyToolReceipt};

pub(super) async fn run(
    deps: &LoopDeps<'_>,
    session: &mut PersistedAgentSession,
    call: &crate::chat::ToolCall,
) -> Result<ReplyToolReceipt, AgentError> {
    ensure_lease_healthy(deps).await?;
    let generation = deps.tools.prepare_issue_reply(session, call).await?;
    let input = match generation {
        ReplyGeneration::Existing(receipt) => return Ok(receipt),
        ReplyGeneration::Generate(input) => *input,
    };
    let policy = deps
        .model
        .model_egress_policy()?
        .ok_or_else(|| crate::subagent::invalid("Missing publication model policy"))?;
    let mut request = Box::new(
        policy
            .prepare_issue_reply_call(
                input.publication.origin.clone(),
                input.publication.access.clone(),
                input.materials,
            )
            .map_err(|error| error.agent_error())?,
    );
    request
        .issue_reply
        .as_mut()
        .ok_or_else(|| crate::subagent::invalid("Missing publication binding"))?
        .bind_native_input(session)
        .map_err(|error| error.agent_error())?;
    request.observation = deps.model.observation_context(
        crate::model_profile::ModelUseCase::Agent,
        crate::model_observability::Origin::System,
    );
    if let Some(prepared) = deps.model.prepare_request(&request)? {
        prepared.require_fit()?;
        request.preparation = Some(Box::new(prepared));
    }
    let identity = format!(
        "issue-reply:{}",
        crate::issue_reply::artifact_id(
            &session.actor_id,
            &session.conversation_id,
            session.input_revision,
            &call.id,
        )
    );
    let budget = call_budget::reserve_model(
        deps,
        session,
        &mut request,
        &identity,
        crate::subagent::reservation::DelegationCallKind::Model,
    )
    .await?;
    let mut sink = crate::seam::NullTurnSink;
    let result = phases::dial_request(deps.model, request, &mut sink).await;
    call_budget::settle_model(deps, session, budget, &result).await?;
    let turn = result?;
    session.record_auxiliary_usage(turn.usage);
    ensure_lease_healthy(deps).await?;
    if input_revision_advanced(deps, session).await?.is_some()
        || child_source_paused(deps, session).await?
    {
        return Err(crate::subagent::invalid(
            "Original publication work is no longer active",
        ));
    }
    if !matches!(classify_model_turn(&turn), Ok(TurnDisposition::Answer))
        || !turn.tool_calls.is_empty()
        || turn.text.trim().is_empty()
    {
        return Err(crate::subagent::invalid(
            "Issue reply generation did not return a complete comment",
        ));
    }
    if review_model_turn(deps, &turn).await? != ContentSafetyDecision::Allow {
        return Err(content_blocked_error());
    }
    ensure_lease_healthy(deps).await?;
    deps.tools
        .freeze_issue_reply(session, input.publication, turn)
        .await
}
