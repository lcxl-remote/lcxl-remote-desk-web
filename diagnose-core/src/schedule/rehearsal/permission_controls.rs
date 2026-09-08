//! Permission status is control lineage, never an execution or data-source grant.
use crate::schedule::source_graph::TaskSourceError;
use crate::{
    chat::{ChatRole, ToolCall},
    model_egress::ModelInputLineage,
    session::PersistedAgentSession,
};

pub struct PermissionControls {
    pub nodes: Vec<ModelInputLineage>,
    pub calls: Vec<(String, String)>,
}

/// Hosts authenticate the stored original requests; state changes do not change
/// the content of the historical pending result.
pub fn collect(session: &PersistedAgentSession) -> Result<PermissionControls, TaskSourceError> {
    let mut result = PermissionControls {
        nodes: vec![],
        calls: vec![],
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut pauses = Vec::new();
    for message in &session.conversation {
        if message.role != ChatRole::Tool
            || message.data_envelope.as_ref().is_none_or(|label| {
                label.provenance.source_provider_id != crate::dynamic_run::RUN_CONTROL_PROVIDER_ID
            })
        {
            continue;
        }
        let Some(id) = message.tool_call_id.as_deref() else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&message.text) else {
            continue;
        };
        let status = value.get("status").and_then(|value| value.as_str());
        let reused = status == Some("existing_permission_request");
        if !reused && status != Some("pending_user_decision") {
            continue;
        }
        if !seen.insert(id) {
            return Err(TaskSourceError::ConflictingNode);
        }
        let request_id = value
            .get("request_id")
            .and_then(|value| value.as_str())
            .ok_or(TaskSourceError::InvalidNode)?;
        let requests: Vec<_> = session
            .permission_requests
            .iter()
            .filter(|request| request.request_id == request_id)
            .collect();
        if requests.len() != 1 {
            return Err(TaskSourceError::ConflictingNode);
        }
        let request = requests[0];
        request
            .validate()
            .map_err(|_| TaskSourceError::InvalidNode)?;
        let proposals: Vec<_> = session
            .conversation
            .iter()
            .filter(|parent| parent.role == ChatRole::Assistant)
            .flat_map(|parent| {
                parent
                    .tool_calls
                    .iter()
                    .filter(move |call| call.id == id)
                    .map(move |call| (parent, call))
            })
            .collect();
        if proposals.len() != 1 {
            return Err(TaskSourceError::ConflictingNode);
        }
        let (parent, original) = proposals[0];
        let explicit =
            original.name == crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME;
        if reused && !explicit {
            return Err(TaskSourceError::ConflictingNode);
        }
        let tool = ToolCall {
            id: original.id.clone(),
            name: original.name.clone(),
            arguments_json: original.arguments_json.clone(),
        };
        if explicit {
            let rebuilt = crate::permission_tools::build_permission_request(
                &tool,
                &crate::device_assistant::device_assistant_provider_registry(),
                request.request_id.clone(),
                request.input_revision,
                request.created_at.clone(),
            )
            .map_err(|_| TaskSourceError::InvalidNode)?;
            let mut historical = request.clone();
            historical.state = crate::dynamic_run::PermissionRequestState::Pending;
            if if reused {
                !crate::permission_tools::equivalent_permission_request(request, &rebuilt)
            } else {
                rebuilt != historical
            } {
                return Err(TaskSourceError::ConflictingNode);
            }
        } else {
            let canonical = crate::permission_tools::canonical_tool_permission_input_json(
                &tool.name,
                serde_json::from_str(&tool.arguments_json)
                    .map_err(|_| TaskSourceError::InvalidNode)?,
            )
            .map_err(|_| TaskSourceError::InvalidNode)?;
            if request.items.len() != 1
                || request.items[0].item_id != id
                || request.items[0].tool_name != tool.name
                || request.items[0].suggested_max_uses != 1
                || request.items[0].canonical_input_json.as_deref() != Some(canonical.as_str())
            {
                return Err(TaskSourceError::ConflictingNode);
            }
        }
        let expected = if reused {
            // A historical state label is not evidence of a present decision.
            let state = value
                .get("decision_state")
                .and_then(|value| value.as_str())
                .ok_or(TaskSourceError::InvalidNode)?;
            if ![
                "pending",
                "needs_revalidation",
                "approved",
                "partially_approved",
                "denied",
                "replaced",
                "withdrawn",
            ]
            .contains(&state)
            {
                return Err(TaskSourceError::InvalidNode);
            }
            crate::permission_tools::existing_request_result(request_id, state)
        } else {
            let mut pending = serde_json::json!({"status":"pending_user_decision", "request_id":request_id,
                "item_count":request.items.len(), "authority":"none"});
            if !explicit {
                pending["executed"] = false.into();
            }
            pending
        };
        // Require the exact canonical control receipt text, not parsed JSON equivalence.
        let expected_text = expected.to_string();
        if message.text != expected_text
            || message.image_data_url.is_some()
            || message.background_task_id.is_some()
            || !message.tool_calls.is_empty()
        {
            return Err(TaskSourceError::ConflictingNode);
        }
        let envelope = crate::model_message_labels::internal_tool_result_envelope(
            parent.data_envelope.as_ref(),
            id,
            &message.text,
            if explicit {
                crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME
            } else {
                "provider_execution_status"
            },
        )
        .map_err(|_| TaskSourceError::InvalidNode)?
        .ok_or(TaskSourceError::MissingSource)?;
        if message.data_envelope.as_ref() != Some(&envelope) {
            return Err(TaskSourceError::ConflictingNode);
        }
        result.nodes.push(super::sources::source_node(&envelope));
        if !reused
            || value
                .get("decision_state")
                .and_then(|value| value.as_str())
                .is_some_and(|state| matches!(state, "pending" | "needs_revalidation"))
        {
            pauses.push((id.to_owned(), request_id.to_owned()));
        }
        if explicit {
            result.calls.push((id.to_owned(), request_id.to_owned()));
        }
    }
    let (nodes, calls) = super::paused_controls::collect(
        session,
        &pauses,
        "permission_pause_tool_call",
        &[
            "not executed: waiting for user permission decision",
            "not executed: waiting for the existing user permission decision",
        ],
    )?;
    result.nodes.extend(nodes);
    result.calls.extend(calls);
    Ok(result)
}
