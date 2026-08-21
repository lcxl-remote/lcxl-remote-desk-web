//! Conversation history trimming for the agentic loop.
//!
//! A long multi-turn conversation eventually exceeds the model's context budget,
//! so before each model call the loop sends only a trailing window of the stored
//! history (the system prompt is prepended separately and is not subject to this
//! trim). The window is the largest suffix that fits a byte budget, adjusted so it
//! never begins with an orphaned tool result — a [`ChatRole::Tool`] message whose
//! originating assistant `tool_calls` message fell outside the window. Sending a
//! tool result without its preceding tool call is a protocol error on both the
//! OpenAI and Anthropic dialects, so well-formedness wins over the byte budget in
//! the degenerate case where even one group does not fit.
//!
//! Trimming is a non-destructive *view*: it returns a fresh `Vec` and never
//! mutates the stored conversation (which the manager persists and reconciles by
//! `message_id`).

use serde::Serialize;

use crate::chat::{ChatMessage, ChatRole, ToolCallRef, frame_context_summary};

/// The serialized byte cost charged against the budget for one message. Uses the
/// JSON encoding (what actually crosses to the gateway) and falls back to the text
/// length if encoding somehow fails.
pub fn model_context_cost(msg: &ChatMessage) -> usize {
    #[derive(Serialize)]
    struct ModelContextCostView<'a> {
        role: ChatRole,
        text: &'a str,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: &'a Vec<ToolCallRef>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_call_id: &'a Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        background_task_id: &'a Option<String>,
    }

    let framed_summary =
        (msg.role == ChatRole::ContextSummary).then(|| frame_context_summary(&msg.text));
    let model_text = framed_summary.as_deref().unwrap_or(&msg.text);
    let base = serde_json::to_string(&ModelContextCostView {
        role: msg.role,
        text: model_text,
        tool_calls: &msg.tool_calls,
        tool_call_id: &msg.tool_call_id,
        background_task_id: &msg.background_task_id,
    })
    .map(|s| s.len())
    .unwrap_or(msg.text.len());
    base.saturating_add(
        msg.replay_disposition
            .as_ref()
            .map_or(0, crate::replay::ReplayDisposition::model_context_cost),
    )
}

/// Return the trailing window of `messages` that fits `max_bytes`, never starting
/// with an orphaned tool result.
///
/// The window is the largest suffix whose summed [`message_cost`] is `<= max_bytes`
/// (always keeping at least the newest message), then advanced past any leading
/// run of [`ChatRole::Tool`] messages so the first kept message is a user or
/// assistant message. If that advance would empty the window, it falls back to the
/// shortest non-empty well-formed suffix (from the last non-`Tool` message), so a
/// budget smaller than the final group still yields a well-formed request.
pub fn trim_conversation(messages: &[ChatMessage], max_bytes: usize) -> Vec<ChatMessage> {
    if messages.is_empty() {
        return Vec::new();
    }

    // Largest suffix that fits the budget; always keep the newest message even if
    // it alone exceeds the budget (an empty request is never useful).
    let mut start = messages.len();
    let mut used = 0usize;
    for (idx, msg) in messages.iter().enumerate().rev() {
        let cost = model_context_cost(msg);
        if start != messages.len() && used + cost > max_bytes {
            break;
        }
        used += cost;
        start = idx;
    }

    // Drop a leading run of orphaned tool results (their assistant call is out of
    // window). In a well-formed history a tool result follows either its
    // assistant(tool_calls) or another tool result for the same call, so trimming
    // only the front run leaves every remaining tool result paired with an
    // in-window assistant call.
    let mut window_start = start;
    while window_start < messages.len() && messages[window_start].role == ChatRole::Tool {
        window_start += 1;
    }

    // Degenerate: the budget admitted only tool results. Fall back to the last
    // non-`Tool` message so the window is non-empty and starts on a safe boundary.
    if window_start >= messages.len() {
        window_start = messages
            .iter()
            .rposition(|m| m.role != ChatRole::Tool)
            .unwrap_or(messages.len() - 1);
    }

    messages[window_start..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ChatRole, ToolCallRef};

    fn user(id: &str, text: &str) -> ChatMessage {
        ChatMessage::text(id, ChatRole::User, text)
    }
    fn answer(id: &str, text: &str) -> ChatMessage {
        ChatMessage::text(id, ChatRole::Assistant, text)
    }
    fn calls(id: &str, call_id: &str) -> ChatMessage {
        ChatMessage::assistant_tool_calls(
            id,
            "",
            vec![ToolCallRef {
                id: call_id.into(),
                name: "sysinfo".into(),
                arguments_json: "{}".into(),
            }],
        )
    }
    fn result(id: &str, call_id: &str, text: &str) -> ChatMessage {
        ChatMessage::tool_result(id, call_id, text)
    }

    /// A conversation under budget is returned whole.
    #[test]
    fn under_budget_keeps_everything() {
        let msgs = vec![user("u1", "hi"), answer("a1", "hello")];
        let out = trim_conversation(&msgs, 100_000);
        assert_eq!(out, msgs);
    }

    /// An empty conversation trims to empty.
    #[test]
    fn empty_is_empty() {
        assert!(trim_conversation(&[], 1000).is_empty());
    }

    /// Over budget keeps the newest messages and drops the oldest.
    #[test]
    fn over_budget_keeps_newest_suffix() {
        let msgs = vec![
            user("u1", &"x".repeat(500)),
            answer("a1", &"y".repeat(500)),
            user("u2", "recent question"),
            answer("a2", "recent answer"),
        ];
        // Budget admits only the two recent short messages.
        let out = trim_conversation(&msgs, 200);
        let ids: Vec<_> = out.iter().map(|m| m.message_id.clone()).collect();
        assert_eq!(ids, vec!["u2", "a2"]);
    }

    /// Always keeps at least the newest message even if it alone exceeds the
    /// budget.
    #[test]
    fn keeps_newest_even_when_oversized() {
        let msgs = vec![answer("a1", &"z".repeat(10_000))];
        let out = trim_conversation(&msgs, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message_id, "a1");
    }

    /// A window that would begin with an orphaned tool result drops the leading
    /// orphan run so the result never appears without its assistant call.
    #[test]
    fn drops_leading_orphan_tool_results() {
        let msgs = vec![
            calls("a1", "c1"),
            result("t1", "c1", &"r".repeat(400)),
            user("u2", "next"),
            answer("a2", "ok"),
        ];
        // Budget admits the big tool result + the two recent, but not a1; the
        // orphaned t1 must be dropped, leaving the well-formed [u2, a2].
        let out = trim_conversation(&msgs, 600);
        let ids: Vec<_> = out.iter().map(|m| m.message_id.clone()).collect();
        assert_eq!(ids, vec!["u2", "a2"]);
        assert!(
            out.iter()
                .all(|m| m.role != ChatRole::Tool || m.message_id != "t1")
        );
    }

    /// A trailing tool group keeps its assistant call paired in the window (the
    /// assistant precedes its results, so no orphan results survive).
    #[test]
    fn keeps_trailing_group_paired() {
        let msgs = vec![
            user("u1", &"x".repeat(800)),
            calls("a1", "c1"),
            result("t1", "c1", "tool output"),
        ];
        let out = trim_conversation(&msgs, 200);
        // u1 drops; the window begins at the assistant call, not the tool result.
        let ids: Vec<_> = out.iter().map(|m| m.message_id.clone()).collect();
        assert_eq!(ids, vec!["a1", "t1"]);
        assert_eq!(out[0].role, ChatRole::Assistant);
    }

    /// Degenerate budget that admits only a trailing tool result still yields a
    /// non-empty, well-formed window (falls back to the last non-tool message).
    #[test]
    fn degenerate_budget_falls_back_to_safe_start() {
        let msgs = vec![
            user("u1", "q"),
            calls("a1", "c1"),
            result("t1", "c1", &"r".repeat(10_000)),
        ];
        let out = trim_conversation(&msgs, 50);
        assert!(!out.is_empty());
        assert_ne!(
            out[0].role,
            ChatRole::Tool,
            "window starts on a safe boundary"
        );
    }

    #[test]
    fn context_cost_ignores_server_id_and_image_payload() {
        let base = user("short-id", "same model content");
        let mut metadata_heavy = base
            .clone()
            .with_image(format!("data:image/png;base64,{}", "A".repeat(100_000)));
        metadata_heavy.message_id = "server-only-id".repeat(1000);
        assert_eq!(
            model_context_cost(&base),
            model_context_cost(&metadata_heavy)
        );
    }

    #[test]
    fn context_cost_counts_tool_protocol_payload() {
        let short = calls("a1", "c1");
        let mut long = short.clone();
        long.tool_calls[0].arguments_json = format!("{{\"value\":\"{}\"}}", "x".repeat(1000));
        assert!(model_context_cost(&long) > model_context_cost(&short) + 900);
    }
}
