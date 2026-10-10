//! Durable facts and summary provenance survive compatible model configuration changes.
use super::*;

pub(crate) fn group_messages_for_policy(
    conversation: &[ChatMessage],
    policy: &PinnedContextPolicy,
) -> Result<Vec<MessageGroup>, ModelContextError> {
    let mut groups = group_messages(conversation, &policy.source_context_key)?;
    if !policy.preserve_history {
        return Ok(groups);
    }
    for group in &mut groups {
        let message = &conversation[group.start];
        if message.tool_calls.is_empty() && message.replay_disposition.is_none() {
            continue;
        }
        match &message.replay_disposition {
            Some(ReplayDisposition::NotRequired { .. }) => {}
            Some(ReplayDisposition::Present { envelope }) if envelope.validate().is_ok() => {}
            _ => {
                return Err(ModelContextError::ProtectedReplayUnsafe(
                    message.message_id.clone(),
                ));
            }
        }
        group.replay_safe = true;
        group.summary_eligible = true;
        group.discard_only = false;
        if message
            .replay_disposition
            .as_ref()
            .and_then(ReplayDisposition::source_context_key)
            != Some(&policy.source_context_key)
        {
            let mut portable = conversation[group.start..group.end].to_vec();
            project_portable_replay(&mut portable, policy);
            for message in &mut portable {
                message.prepared_context_cost.0 = None;
            }
            group.cost = portable.iter().map(crate::trim::model_context_cost).sum();
        }
    }
    Ok(groups)
}

pub(crate) fn project_portable_replay(messages: &mut [ChatMessage], policy: &PinnedContextPolicy) {
    if !policy.preserve_history {
        return;
    }
    for message in messages {
        if message.role == ChatRole::Assistant
            && (!message.tool_calls.is_empty() || message.replay_disposition.is_some())
            && message
                .replay_disposition
                .as_ref()
                .and_then(ReplayDisposition::source_context_key)
                != Some(&policy.source_context_key)
        {
            // Provider-specific replay is not portable; answers and tool facts are.
            message.replay_disposition = Some(ReplayDisposition::NotRequired {
                source_context_key: policy.source_context_key.clone(),
            });
        }
    }
}

/// Return an ephemeral state candidate. It is persisted only with a compatible
/// ready view, so a rejected smaller model cannot authorize silent compression
/// merely because the owner retries the same request.
pub fn adopt_compatible_checkpoint(
    state: &ModelContextState,
    conversation: &[ChatMessage],
    policy: &PinnedContextPolicy,
    egress: Option<&crate::model_egress::ModelEgressPolicy>,
) -> Result<ModelContextState, ModelContextError> {
    if !policy.preserve_history
        || state
            .entries
            .iter()
            .any(|entry| entry.policy_key == policy.key())
    {
        return Ok(state.clone());
    }
    let Some(previous) = state
        .entries
        .iter()
        .filter(|entry| entry.checkpoint.is_some())
        .max_by_key(|entry| entry.last_used_session_version)
    else {
        return Ok(state.clone());
    };
    let groups = group_messages_for_policy(conversation, policy)?;
    let floor = checkpoint::resolve_floor(conversation, &groups, Some(previous))?;
    let checkpoint = previous.checkpoint.as_ref().unwrap().v1();
    checkpoint::validate_checkpoint(checkpoint, conversation, &groups, policy, floor)?;
    if let Some(egress) = egress {
        authorize_context_checkpoint(egress, state, &previous.policy_key, conversation)?;
    }
    let mut next = state.clone();
    let mut adopted = previous.clone();
    adopted.policy_key = policy.key();
    // Keep original compressor identity, covered hashes and generation intact.
    upsert_entry(&mut next, adopted);
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy(source: &str, budget: usize) -> PinnedContextPolicy {
        let source = SourceContextKey::derive(
            crate::model_profile::WireProtocol::OpenAiChatCompletions,
            "gateway",
            source,
            source,
        );
        let mut policy = PinnedContextPolicy::window(source, 1, budget).unwrap();
        policy.preserve_history = true;
        policy
    }
    #[test]
    fn plain_answer_keeps_replay_on_same_model_and_drops_foreign_opaque_material() {
        let old = policy("old", 8192);
        let new = policy("new", 8192);
        let mut answer = ChatMessage::text("answer", ChatRole::Assistant, "historical answer");
        answer.replay_disposition = Some(ReplayDisposition::Present {
            envelope: crate::replay::ProviderReplayEnvelope::new(
                crate::replay::ReplayCodec::OpenAiReasoningContent,
                old.source_context_key.clone(),
                serde_json::json!("provider specific reasoning"),
            ),
        });
        let conversation = vec![answer.clone()];
        let same = group_messages_for_policy(&conversation, &old).unwrap();
        let switched = group_messages_for_policy(&conversation, &new).unwrap();
        assert!(same[0].replay_safe && switched[0].replay_safe);
        assert!(switched[0].cost < same[0].cost);
        let mut projected = conversation.clone();
        project_portable_replay(&mut projected, &old);
        assert_eq!(projected, conversation);
        project_portable_replay(&mut projected, &new);
        assert_eq!(projected[0].text, "historical answer");
        assert!(
            matches!(&projected[0].replay_disposition, Some(ReplayDisposition::NotRequired { source_context_key }) if source_context_key == &new.source_context_key)
        );
        assert_eq!(
            conversation[0].replay_disposition,
            answer.replay_disposition
        );
        answer.replay_disposition = Some(ReplayDisposition::legacy_unknown());
        assert!(matches!(
            group_messages_for_policy(&[answer], &new),
            Err(ModelContextError::ProtectedReplayUnsafe(_))
        ));
    }

    #[test]
    fn durable_window_rejects_overflow_instead_of_dropping_old_facts() {
        let policy = policy("model", 4096);
        let conversation = vec![
            ChatMessage::text("old", ChatRole::User, "old fact".repeat(600)),
            ChatMessage::text("new", ChatRole::User, "continue"),
        ];
        let mut state = ModelContextState::default();
        assert!(matches!(
            build_model_context_view(&conversation, &mut state, &policy, 1),
            Err(ModelContextError::ProtectedStateTooLarge { .. })
        ));
        assert!(state.entries.is_empty());
    }
    #[test]
    fn model_switch_keeps_complete_tool_group_and_does_not_relabel_persisted_replay() {
        let old = policy("old", 8192);
        let new = policy("new", 8192);
        let call = crate::chat::ToolCallRef {
            id: "call".into(),
            name: "read".into(),
            arguments_json: "{}".into(),
        };
        let message = ChatMessage::assistant_tool_calls_with_replay(
            "assistant",
            "",
            vec![call],
            ReplayDisposition::NotRequired {
                source_context_key: old.source_context_key.clone(),
            },
        );
        let conversation = vec![
            message,
            ChatMessage::tool_result("result", "call", "historical fact"),
        ];
        let plan = plan_model_context(
            &conversation,
            &ModelContextState::default(),
            &new,
            &ContextProtectionSet::default(),
            1,
        )
        .unwrap();
        let ContextBuildPlan::Ready(plan) = plan else {
            panic!("history must be ready")
        };
        assert_eq!(plan.view.messages.len(), 2);
        assert_eq!(plan.view.messages[1].text, "historical fact");
        assert_eq!(
            conversation[0]
                .replay_disposition
                .as_ref()
                .unwrap()
                .source_context_key(),
            Some(&old.source_context_key)
        );
    }
}
