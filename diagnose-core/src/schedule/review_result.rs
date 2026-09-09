//! Model-only projection of a completed review onto its original tool call.
//! Persisted draft and decision receipts remain immutable audit records.
use crate::{
    chat::{ChatMessage, ChatRole},
    dynamic_run::RUN_CONTROL_PROVIDER_ID,
};
use desk_agent_protocol::AgentError;
use serde_json::Value;

fn labelled(message: &ChatMessage, role: ChatRole, source: &str) -> bool {
    message.role == role
        && message.data_envelope.as_ref().is_some_and(|label| {
            label.provenance.source_provider_id == RUN_CONTROL_PROVIDER_ID
                && label.provenance.source_tool_name == source
        })
}

pub fn project(messages: &mut [ChatMessage]) -> Result<(), AgentError> {
    for index in 0..messages.len() {
        let draft = &messages[index];
        if !labelled(draft, ChatRole::Tool, "schedule_proposal") {
            continue;
        }
        let Ok(proposal) = serde_json::from_str::<Value>(&draft.text) else {
            continue;
        };
        if proposal["state"] != "pending_review" || proposal["kind"] != "conversation_resume" {
            continue;
        }
        let Some(id) = proposal["schedule_id"].as_str().filter(|id| !id.is_empty()) else {
            continue;
        };
        let Some(call_id) = draft.tool_call_id.as_deref() else {
            continue;
        };
        let decision = messages[index + 1..].iter().find_map(|message| {
            if !labelled(message, ChatRole::SystemEvent, "schedule_activation") {
                return None;
            }
            let value = serde_json::from_str::<Value>(&message.text).ok()?;
            let approved = value["event"] == "scheduled_task_activated"
                && value["owner_decision"] == "approved"
                && value["state"] == "active";
            let rejected = value["event"] == "scheduled_task_rejected"
                && value["owner_decision"] == "rejected"
                && value["state"] == "deleted";
            (value["schedule_id"].as_str() == Some(id) && (approved || rejected))
                .then_some((message, value))
        });
        let Some((receipt, mut value)) = decision else {
            continue;
        };
        value["kind"] = Value::String("conversation_resume".into());
        value["awaiting_confirmation"] = Value::Bool(false);
        value["review_complete"] = Value::Bool(true);
        value["message"] = Value::String(if value["owner_decision"] == "approved" {
            "The owner has already approved this task and scheduling is enabled. Report success using the returned execution time; do not ask the owner to approve again."
        } else {
            "The owner rejected this task. It is not scheduled. Report the rejection; do not ask for confirmation again or recreate it without a new user request."
        }.into());
        let text = value.to_string();
        // Both original records must authorize the destination. Never gain a
        // destination or extend retention by projecting the later decision.
        let mut envelope = crate::model_message_labels::conversation_history_result_envelope(
            draft.data_envelope.as_ref(),
            std::slice::from_ref(receipt),
            call_id,
            &text,
        )?;
        if let Some(label) = envelope.as_mut() {
            label.provenance.source_tool_name = "schedule_review_result".into();
        }
        messages[index].text = text;
        messages[index].data_envelope = envelope;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_message_labels::{internal_tool_result_envelope, model_bound_user_message};
    use desk_agent_protocol::data_lineage::DestinationIdentity;
    fn records(approved: bool) -> Vec<ChatMessage> {
        let user = model_bound_user_message(
            "u".into(),
            "timer".into(),
            DestinationIdentity::Model {
                connection_id: "gateway".into(),
                connection_revision: 1,
                model_id: "m".into(),
                profile_revision: 1,
            },
        )
        .unwrap();
        let mut draft = ChatMessage::tool_result(
            "draft",
            "call",
            r#"{"schedule_id":"timer","kind":"conversation_resume","state":"pending_review"}"#,
        );
        draft.data_envelope = internal_tool_result_envelope(
            user.data_envelope.as_ref(),
            "call",
            &draft.text,
            "schedule_proposal",
        )
        .unwrap();
        let text = serde_json::json!({"schedule_id":"timer", "event":if approved {"scheduled_task_activated"} else {"scheduled_task_rejected"}, "owner_decision":if approved {"approved"} else {"rejected"}, "state":if approved {"active"} else {"deleted"}, "next_run_at_utc_ms":12345}).to_string();
        let mut receipt = ChatMessage::system_event("receipt", &text);
        receipt.data_envelope = internal_tool_result_envelope(
            draft.data_envelope.as_ref(),
            "receipt",
            &text,
            "schedule_activation",
        )
        .unwrap();
        vec![draft, receipt]
    }
    #[test]
    fn projects_final_decision_onto_tool_without_modifying_original_records() {
        for approved in [true, false] {
            let original = records(approved);
            let mut projected = original.clone();
            project(&mut projected).unwrap();
            let result: Value = serde_json::from_str(&projected[0].text).unwrap();
            assert_eq!(result["state"], if approved { "active" } else { "deleted" });
            assert_eq!(result["awaiting_confirmation"], false);
            assert_eq!(result["next_run_at_utc_ms"], 12345);
            assert_eq!(projected[0].tool_call_id, original[0].tool_call_id);
            assert_eq!(projected[1], original[1]);
            assert_eq!(
                serde_json::from_str::<Value>(&original[0].text).unwrap()["state"],
                "pending_review"
            );
            let label = projected[0].data_envelope.as_ref().unwrap();
            assert_eq!(label.provenance.source_envelope_ids.len(), 2);
            assert_eq!(
                label.allowed_destinations,
                original[0]
                    .data_envelope
                    .as_ref()
                    .unwrap()
                    .allowed_destinations
            );
            let once = projected.clone();
            project(&mut projected).unwrap();
            assert_eq!(projected, once);
        }
    }
    #[test]
    fn unrelated_untrusted_or_missing_decisions_leave_draft_unchanged() {
        for case in 0..4 {
            let mut messages = records(true);
            match case {
                0 => {
                    messages.pop();
                }
                1 => messages[1].role = ChatRole::User,
                2 => messages[1].data_envelope = None,
                _ => messages[1].text = messages[1].text.replace("timer", "other"),
            }
            let original = messages.clone();
            project(&mut messages).unwrap();
            assert_eq!(messages, original);
        }
    }
}
