//! Separate generated text from the per-run directory selector after Provider preflight.
use super::*;

pub fn generated_text_input(
    tool: &crate::chat::ToolCall,
) -> Result<TaskGeneratedTextArtifact, TaskContractError> {
    if tool.name != "create_text_artifact_in_selected_directory" {
        return Err(TaskContractError::InvalidInput);
    }
    let action = crate::provider_preflight::artifact_action_from_call(tool)
        .map_err(|_| TaskContractError::InvalidInput)?;
    match action {
        desk_agent_protocol::computer_use::FilePatchAction::CreateTextArtifact {
            file_name,
            content_utf8,
        } => Ok(TaskGeneratedTextArtifact {
            file_name,
            content_utf8,
        }),
        _ => Err(TaskContractError::InvalidInput),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projection_preserves_text_without_promoting_directory_selector_to_authority() {
        let call = crate::chat::ToolCall { id: "create".into(), name: "create_text_artifact_in_selected_directory".into(),
            arguments_json: serde_json::json!({"file_name":"report.txt","content_utf8":"New report\n", "directory_request_id":"this-run-directory"}).to_string() };
        let projected = generated_text_input(&call).unwrap();
        assert_eq!(projected.file_name, "report.txt");
        assert_eq!(projected.content_utf8, "New report\n");
        assert!(
            serde_json::to_value(projected)
                .unwrap()
                .get("directory_request_id")
                .is_none()
        );
        for arguments in [
            serde_json::json!({"file_name":"../report.txt","content_utf8":"report"}),
            serde_json::json!({"file_name":"report.txt","content_utf8":"report","directory_request_id":""}),
            serde_json::json!({"file_name":"report.txt","content_utf8":"report","shell":"echo report"}),
        ] {
            assert!(
                generated_text_input(&crate::chat::ToolCall {
                    arguments_json: arguments.to_string(),
                    ..call.clone()
                })
                .is_err()
            );
        }
    }
}

/// Stable contract label, never a native directory token or an execution grant.
pub fn directory_resource_scope(
    device: &str,
    path: &str,
) -> Result<Vec<String>, TaskContractError> {
    id(device)?;
    if !is_absolute_directory(path)
        || path.len() > 4096
        || path.trim() != path
        || path.chars().any(char::is_control)
    {
        return Err(TaskContractError::InvalidScope);
    }
    let encoded = canonical(
        serde_json::json!({"schema":"task-artifact-directory/v1","device":device,"canonical_path":path}),
    )?;
    Ok(vec![format!(
        "task-directory:sha256:{:x}",
        Sha256::digest(encoded.as_bytes())
    )])
}

/// Resolve the stored directory selection for this run and require the actual
/// Provider preflight to target that exact fresh object. No selection is created.
pub fn bind_directory(
    contract: &ValidatedTaskContract,
    session: &crate::session::PersistedAgentSession,
    tool: &crate::chat::ToolCall,
    step_id: &str,
    resources: &[String],
    now: u64,
) -> Result<Vec<String>, TaskContractError> {
    let step = contract
        .contract()
        .steps
        .iter()
        .find(|step| step.step_id == step_id)
        .ok_or(TaskContractError::InvalidSteps)?;
    let TaskStepBinding::ProduceTextArtifact {
        canonical_directory,
        ..
    } = &step.binding
    else {
        return Err(TaskContractError::InvalidSteps);
    };
    if session.device_id != contract.contract().target_device_id {
        return Err(TaskContractError::InvalidIdentity);
    }
    let path = observed_directory(session, tool, resources, now)?;
    if path != canonical_directory {
        return Err(TaskContractError::InvalidScope);
    }
    directory_resource_scope(&session.device_id, canonical_directory)
}

// Interpret the remote path independently of the scheduler host's OS. Reject
// traversal and Windows device namespaces; canonicalization belongs to Provider.
fn is_absolute_directory(path: &str) -> bool {
    let bytes = path.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\');
    let unc =
        path.starts_with("\\\\") && !path.starts_with("\\\\?\\") && !path.starts_with("\\\\.\\");
    if !(path.starts_with('/') || drive || unc) {
        return false;
    }
    let components: Vec<_> = path
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .collect();
    if components.iter().any(|part| matches!(*part, "." | "..")) {
        return false;
    }
    !unc || components.len() >= 2
}

#[cfg(test)]
mod directory_tests {
    use super::*;

    #[test]
    fn remote_directory_identity_requires_absolute_non_traversing_path() {
        for path in [
            "/",
            "/reports",
            "C:/Reports",
            r"C:\Reports",
            r"\\server\share\reports",
        ] {
            assert!(directory_resource_scope("device", path).is_ok(), "{path}");
        }
        for path in [
            "",
            "reports",
            "C:reports",
            "./reports",
            "/reports/../private",
            r"C:\Reports\..\Private",
            r"\\?\C:\Reports",
            r"\\.\pipe\report",
            r"\\server",
            "/reports\n",
        ] {
            assert!(directory_resource_scope("device", path).is_err(), "{path}");
        }
        assert_ne!(
            directory_resource_scope("device-a", "/reports").unwrap(),
            directory_resource_scope("device-b", "/reports").unwrap()
        );
        assert_ne!(
            directory_resource_scope("device", "/reports").unwrap(),
            directory_resource_scope("device", "/private").unwrap()
        );
    }
}

/// Recover an actual approved selection at the time of an observed operation.
/// The caller verifies the original action receipt and its completion timestamp.
pub fn observed_directory<'a>(
    session: &'a crate::session::PersistedAgentSession,
    tool: &crate::chat::ToolCall,
    resources: &[String],
    at: u64,
) -> Result<&'a str, TaskContractError> {
    let input: Value =
        serde_json::from_str(&tool.arguments_json).map_err(|_| TaskContractError::InvalidInput)?;
    let directory = crate::file_scope::select_output_directory(session, tool, at)
        .map_err(|_| TaskContractError::InvalidScope)?;
    let selected = input.get("directory_request_id").and_then(Value::as_str);
    let subject = session
        .file_scope_subject(
            &session.actor_id,
            &session.device_id,
            &session.conversation_id,
        )
        .map_err(|_| TaskContractError::InvalidIdentity)?;
    let mut records = session.file_scope.records().iter().filter(|record| {
        selected.is_none_or(|selected| record.proposal.request_id == selected)
            && session
                .file_scope
                .approved_directory(
                    &subject,
                    session.file_scope.revision(),
                    &record.proposal.request_id,
                    at,
                )
                .is_ok_and(|approved| approved == &directory)
    });
    let record = records.next().ok_or(TaskContractError::InvalidScope)?;
    if records.next().is_some() {
        return Err(TaskContractError::InvalidScope);
    }
    let exact =
        crate::capability_grant::fresh_object_resource_scope(std::slice::from_ref(&directory));
    if !subset(resources, &exact) || !subset(&exact, resources) {
        return Err(TaskContractError::InvalidScope);
    }
    directory_resource_scope(&session.device_id, &record.proposal.canonical_path)?;
    Ok(&record.proposal.canonical_path)
}

impl ValidatedTaskContract {
    /// Historical coverage, requiring separately authenticated action and model receipts.
    pub fn observed_generated_text_step(
        &self,
        session: &crate::session::PersistedAgentSession,
        tool: &crate::chat::ToolCall,
        observed: &crate::provider_preflight::ObservedCapabilityAuthority,
        output: &desk_agent_protocol::computer_use::CreatedFileArtifactOutput,
        completed_at: u64,
        sources: &crate::schedule::source_graph::ResolvedTaskSources,
    ) -> Option<&TaskFixedStep> {
        if session.device_id != self.contract.target_device_id
            || sources.scopes.is_empty()
            || sources.root_envelope_ids.is_empty()
            || tool.name != observed.tool_name
        {
            return None;
        }
        let canonical_input = crate::permission_tools::canonical_tool_permission_input_json(
            &tool.name,
            serde_json::from_str(&tool.arguments_json).ok()?,
        )
        .ok()?;
        if format!("{:x}", Sha256::digest(canonical_input.as_bytes()))
            != observed.canonical_input_sha256
        {
            return None;
        }
        crate::schedule::source_graph::attachment::verify_text_artifact_output(
            &tool.name,
            &canonical_input,
            output,
        )
        .ok()?;
        let input = generated_text_input(tool).ok()?;
        self.contract.steps.iter().find(|step| {
            let TaskStepBinding::ProduceTextArtifact {
                allowed_source_scopes,
                ..
            } = &step.binding
            else {
                return false;
            };
            let Some(rule) = self
                .contract
                .permissions
                .iter()
                .find(|rule| rule.rule_id == step.rule_id)
            else {
                return false;
            };
            let TaskInputConstraint::GeneratedTextArtifact {
                file_name,
                max_content_bytes,
            } = &rule.input
            else {
                return false;
            };
            rule.provider_id == observed.provider_id
                && rule.capability_id == observed.capability_id
                && rule.tool_name == observed.tool_name
                && rule.tool_schema_version == observed.tool_schema_version
                && rule.effect == observed.effect
                && rule.risk_tier == observed.risk_tier
                && input.file_name == *file_name
                && input.content_utf8.len() <= *max_content_bytes as usize
                && subset(&sources.scopes, allowed_source_scopes)
                && subset(&rule.automatic.operations, &observed.operations)
                && subset(&observed.operations, &rule.automatic.operations)
                && rule.automatic.export_destinations == observed.export_destinations
                && bind_directory(
                    self,
                    session,
                    tool,
                    &step.step_id,
                    &observed.resources,
                    completed_at,
                )
                .is_ok_and(|resources| resources == rule.automatic.resources)
        })
    }
}

/// Match fresh Provider resolution to a directory approved by the task contract.
/// This predicate neither persists consent nor grants any file operation. Hosts
/// must hold the current task and session fences when recording that consent.
pub fn permits_directory_resolution(
    contract: &ValidatedTaskContract,
    session: &crate::session::PersistedAgentSession,
    proposal: &crate::file_scope::DirectoryProposal,
    now: u64,
) -> bool {
    if session.trigger_origin != crate::session::TriggerOrigin::ScheduledTask
        || session.device_id != contract.contract.target_device_id
        || !session.turn_state.is_active()
        || crate::file_scope::validate_resolved_proposal(proposal, now).is_err()
        || proposal.requested_path != proposal.canonical_path
    {
        return false;
    }
    let Ok(resources) = directory_resource_scope(&session.device_id, &proposal.canonical_path)
    else {
        return false;
    };
    contract.contract.steps.iter().any(|step| {
        let TaskStepBinding::ProduceTextArtifact {
            canonical_directory,
            ..
        } = &step.binding
        else {
            return false;
        };
        canonical_directory == &proposal.canonical_path
            && contract.contract.permissions.iter().any(|rule| {
                rule.rule_id == step.rule_id
                    && matches!(
                        rule.input,
                        TaskInputConstraint::GeneratedTextArtifact { .. }
                    )
                    && rule.effect == CapabilityEffect::WriteArtifact
                    && rule.automatic.resources == resources
                    && rule.approval_ceiling.resources == resources
            })
    })
}
