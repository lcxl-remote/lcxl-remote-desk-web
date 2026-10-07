//! Physical-call admission and settlement shared by planning and compression.
use super::*;
use crate::goal::GoalUsage;
use crate::subagent::reservation::{CallAdmission, DelegationCallKind, DelegationCallReservation};

pub(super) const OUTPUT_HARD_CAP: i64 = 8_192;
const EXHAUSTED: &str =
    "The delegation source has exhausted its call budget or reached its deadline.";

pub(super) fn exhausted() -> AgentError {
    AgentError {
        kind: AgentErrorKind::OutputLimitExceeded,
        message: EXHAUSTED.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

pub(super) fn is_exhausted(error: &AgentError) -> bool {
    error.kind == AgentErrorKind::OutputLimitExceeded && error.message == EXHAUSTED
}

// Bounded records transfer as one owned value across admission and claim.
#[allow(clippy::large_enum_variant)]
enum Accounting {
    Delegation(DelegationCallReservation),
    Goal(String),
    Untracked,
}

pub(super) struct PhysicalCall {
    accounting: Accounting,
    upper: GoalUsage,
    started: std::time::Instant,
}

/// Every physical retry has its own logical id. Replaying an admitted identity
/// is reconciled by the provider ledger, never by starting another provider call.
async fn reserve(
    deps: &LoopDeps<'_>,
    session: &PersistedAgentSession,
    logical_id: &str,
    kind: DelegationCallKind,
    digest: &str,
    upper: GoalUsage,
) -> Result<PhysicalCall, AgentError> {
    let accounting = match deps
        .session_seam
        .reserve_delegation_call(session, logical_id, kind, digest, upper, &(deps.clock)())
        .await?
    {
        CallAdmission::Reserved(receipt) => Accounting::Delegation(receipt),
        CallAdmission::Exhausted => return Err(exhausted()),
        CallAdmission::Untracked if session.agent_role.binding().is_some() => {
            return Err(crate::subagent::invalid(
                "a child call requires durable budget admission",
            ));
        }
        CallAdmission::Untracked if session.trigger_origin == TriggerOrigin::GoalContinuation => {
            let goal = deps.session_seam.load_claimed_goal(session).await?;
            match goal.available_for(upper) {
                Ok(()) => {}
                Err(crate::goal::GoalError::BudgetExceeded) => return Err(exhausted()),
                Err(_) => return Err(crate::subagent::invalid("invalid source goal budget")),
            }
            if let Err(error) = deps
                .session_seam
                .reserve_goal_budget(session, logical_id, upper, &(deps.clock)())
                .await
            {
                let latest = deps.session_seam.load_claimed_goal(session).await?;
                if latest.available_for(upper) == Err(crate::goal::GoalError::BudgetExceeded) {
                    return Err(exhausted());
                }
                return Err(error);
            }
            Accounting::Goal(logical_id.into())
        }
        CallAdmission::Untracked => Accounting::Untracked,
    };
    Ok(PhysicalCall {
        accounting,
        upper,
        started: std::time::Instant::now(),
    })
}

/// Hash the exact neutral request, including steering and provider cache input.
/// The byte bound is conservative and the output cap only narrows model config.
pub(super) async fn reserve_model(
    deps: &LoopDeps<'_>,
    session: &PersistedAgentSession,
    request: &mut ModelRequest,
    logical_id: &str,
    kind: DelegationCallKind,
) -> Result<PhysicalCall, AgentError> {
    if request.delegation_call.is_some() {
        return Err(crate::subagent::invalid(
            "model request already carries a call reservation",
        ));
    }
    let original_output_cap = request.caller_output_hard_cap;
    let (digest, mut upper) = prepare_model_budget(request)?;
    if let Some(input_tokens) = deps.model.model_input_token_upper_bound(request)? {
        upper.input_tokens = input_tokens;
    }
    let admitted = reserve(deps, session, logical_id, kind, &digest, upper).await?;
    match &admitted.accounting {
        Accounting::Delegation(receipt) => request.delegation_call = Some(receipt.clone()),
        Accounting::Untracked => request.caller_output_hard_cap = original_output_cap,
        Accounting::Goal(_) => {}
    }
    Ok(admitted)
}

fn prepare_model_budget(request: &mut ModelRequest) -> Result<(String, GoalUsage), AgentError> {
    let output_cap = request
        .caller_output_hard_cap
        .unwrap_or(OUTPUT_HARD_CAP)
        .min(OUTPUT_HARD_CAP);
    if output_cap <= 0 {
        return Err(crate::subagent::invalid("invalid model budget output cap"));
    }
    // The reserved output bound must also constrain the actual provider request.
    request.caller_output_hard_cap = Some(output_cap);
    let format = match &request.response_format {
        crate::prompt::ResponseFormatSpec::None => serde_json::json!({"type": "none"}),
        crate::prompt::ResponseFormatSpec::JsonObject => serde_json::json!({"type": "json_object"}),
        crate::prompt::ResponseFormatSpec::JsonSchema { name, schema } => {
            serde_json::json!({"type": "json_schema", "name": name, "schema": schema})
        }
    };
    let neutral = serde_json::json!({
        "messages": request.messages, "tools": request.tools, "tool_choice": request.tool_choice,
        "image_input": request.tool_requirements.image_input, "response_format": format,
        "use_case": format!("{:?}", request.use_case), "output_cap": request.caller_output_hard_cap,
        "previous_cache_projection": request.previous_cache_projection,
    });
    let bytes = serde_json::to_vec(&neutral)
        .map_err(|_| crate::subagent::invalid("model budget request cannot be serialized"))?;
    // Reserve provider frame overhead and validated image allowance as well as
    // the neutral text. The actual rendered body is checked again before send.
    let total = crate::schedule::model_usage::image_request_reservation(
        &neutral,
        output_cap as u64,
        request
            .messages
            .iter()
            .filter_map(|message| message.image_data_url.as_deref()),
    )
    .ok_or_else(exhausted)?;
    let upper = GoalUsage {
        input_tokens: total.checked_sub(output_cap as u64).ok_or_else(exhausted)?,
        output_tokens: output_cap as u64,
        model_calls: 1,
        active_time_ms: 600_000,
        ..GoalUsage::default()
    };
    Ok((format!("{:x}", Sha256::digest(&bytes)), upper))
}

fn known_model_usage(
    result: &Result<crate::chat::ModelTurn, AgentError>,
    elapsed_ms: u64,
) -> Option<GoalUsage> {
    let turn = result.as_ref().ok()?;
    crate::subagent::reservation::known_model_usage(turn.usage, elapsed_ms)
}

pub(super) async fn settle_model(
    deps: &LoopDeps<'_>,
    session: &PersistedAgentSession,
    call: PhysicalCall,
    result: &Result<crate::chat::ModelTurn, AgentError>,
) -> Result<(), AgentError> {
    let elapsed = u64::try_from(call.started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let actual = known_model_usage(result, elapsed);
    match call.accounting {
        Accounting::Delegation(receipt) => {
            deps.session_seam
                .settle_delegation_call(&receipt, actual, &(deps.clock)())
                .await
        }
        Accounting::Goal(id) => {
            deps.session_seam
                .settle_goal_budget(
                    session,
                    &id,
                    actual.unwrap_or(GoalUsage {
                        active_time_ms: elapsed,
                        ..call.upper
                    }),
                    &(deps.clock)(),
                )
                .await
        }
        Accounting::Untracked => Ok(()),
    }
}

/// Count each returned invocation once, even if protocol validation later rejects
/// it. Actual device permission and dispatch still use their existing authority.
pub(super) async fn charge_tool(
    deps: &LoopDeps<'_>,
    session: &PersistedAgentSession,
    turn_id: &str,
    call: &crate::chat::ToolCall,
) -> Result<(), AgentError> {
    let bytes = serde_json::to_vec(&(
        call.id.as_str(),
        call.name.as_str(),
        call.arguments_json.as_str(),
    ))
    .map_err(|_| crate::subagent::invalid("tool budget request cannot be serialized"))?;
    let digest = format!("{:x}", Sha256::digest(bytes));
    let id = format!("tool:{turn_id}:{}", &digest[..24]);
    let charge = GoalUsage {
        tool_calls: 1,
        ..GoalUsage::default()
    };
    let admitted = reserve(
        deps,
        session,
        &id,
        DelegationCallKind::Tool,
        &digest,
        charge,
    )
    .await?;
    match admitted.accounting {
        Accounting::Delegation(receipt) => {
            deps.session_seam
                .settle_delegation_call(&receipt, Some(charge), &(deps.clock)())
                .await
        }
        Accounting::Goal(id) => {
            deps.session_seam
                .settle_goal_budget(session, &id, charge, &(deps.clock)())
                .await
        }
        Accounting::Untracked => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_budget_caps_the_actual_output_and_covers_rendered_framing() {
        for (configured, expected) in [
            (None, OUTPUT_HARD_CAP),
            (Some(256), 256),
            (Some(32_768), OUTPUT_HARD_CAP),
        ] {
            let mut request = ModelRequest::text_only(
                vec![crate::chat::ChatMessage::text(
                    "budget-input",
                    crate::chat::ChatRole::User,
                    "hello",
                )],
                crate::prompt::ResponseFormatSpec::None,
            );
            request.caller_output_hard_cap = configured;
            let (_, upper) = prepare_model_budget(&mut request).unwrap();
            assert_eq!(request.caller_output_hard_cap, Some(expected));
            assert_eq!(upper.output_tokens, expected as u64);
            let wire = serde_json::json!({"model": "pinned-model", "messages": [{"role": "user", "content": "hello"}], "max_tokens": expected});
            let units =
                crate::schedule::model_usage::text_request_reservation(&wire, expected as u64)
                    .unwrap();
            assert!(upper.total_tokens().unwrap() >= units);
        }
        for invalid in [0, -1] {
            let mut request =
                ModelRequest::text_only(Vec::new(), crate::prompt::ResponseFormatSpec::None);
            request.caller_output_hard_cap = Some(invalid);
            assert!(prepare_model_budget(&mut request).is_err());
        }
    }

    #[test]
    fn unknown_usage_never_becomes_a_free_or_estimated_settlement() {
        let mut turn = crate::chat::ModelTurn::default();
        turn.usage.input_tokens = Some(10);
        assert_eq!(known_model_usage(&Ok(turn.clone()), 7), None);
        turn.usage.output_tokens = Some(2);
        turn.usage.cache_read_tokens = Some(-1);
        assert_eq!(known_model_usage(&Ok(turn.clone()), 7), None);
        turn.usage.cache_read_tokens = Some(3);
        let actual = known_model_usage(&Ok(turn), 7).unwrap();
        assert_eq!(actual.total_tokens(), Some(15));
        assert_eq!(actual.model_calls, 1);
        assert_eq!(actual.active_time_ms, 7);
        assert_eq!(known_model_usage(&Err(exhausted()), 7), None);
    }
}
