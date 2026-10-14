//! Decision facts for one review, separate from its internal authority proof.
use super::*;
use serde_json::{Value, json};

#[derive(Serialize)]
pub struct ReviewModelView<'a> {
    source_kind: &'static str,
    risk: CapabilityRiskTier,
    tool_name: &'a str,
    descriptor_json: String,
    input_kind: ApprovalInputKind,
    action_json: String,
    expires_at_unix_ms: u64,
    context: ReviewModelContext<'a>,
}

#[derive(Serialize)]
struct ReviewModelContext<'a> {
    current_user_requirement: &'a str,
    goal_text: &'a Option<String>,
    current_authority: ReviewAuthority,
    skipped_call_count: usize,
    evidence: &'a [ApprovalEvidence],
    omitted_evidence_count: usize,
}

#[derive(Serialize)]
struct ReviewAuthority {
    relation: &'static str,
    status: ApprovalAuthorityStatus,
}

fn command_plan(value: &Value) -> Value {
    let mut output = json!({});
    for key in [
        "program",
        "argv",
        "cwd",
        "shell",
        "risk",
        "io_mode",
        "execution_basis",
        "timeout_ms",
        "max_stdout_bytes",
        "max_stderr_bytes",
        "containment",
    ] {
        if let Some(value) = value.get(key) {
            output[key] = value.clone();
        }
    }
    output
}

fn permission_action(action: &mut Value) {
    if let Some(command) = action
        .get_mut("command_confirmation")
        .filter(|value| !value.is_null())
    {
        *command = json!({"admission_policy":command["admission_policy"],"validation_input":command["validation_input"],"plan":command_plan(&command["plan"])});
    }
    if let Some(launch) = action
        .get_mut("launch_confirmation")
        .filter(|value| !value.is_null())
    {
        *launch = json!({"request":launch["request"],"identity":{"canonical_target":launch["identity"]["canonical_target"],"resolved_cwd":launch["identity"]["resolved_cwd"]},"target":launch["target"]});
    }
    for key in ["resource_scope", "operation_scope"] {
        if let Some(values) = action.get_mut(key).and_then(Value::as_array_mut) {
            for value in values {
                if let Some(native) = value.as_str() {
                    for (prefix, meaning) in [
                        ("ui_application:sha256:", "selected_application"),
                        ("selected:sha256:", "selected_object"),
                        ("command:sha256:", "exact_command"),
                    ] {
                        if native.starts_with(prefix) {
                            *value = json!(meaning);
                            break;
                        }
                    }
                }
            }
        }
    }
}

impl<'a> ReviewModelView<'a> {
    pub fn from_candidate(
        candidate: &'a ApprovalReviewCandidate,
    ) -> Result<Self, ApprovalReviewError> {
        let native: Value = serde_json::from_str(&candidate.descriptor_json)
            .map_err(|_| ApprovalReviewError::InvalidCandidate)?;
        let mut descriptor = json!({"tool_name":candidate.tool_name});
        // Declared descriptor fields carry decision semantics, not registry identity.
        for key in [
            "description",
            "effect",
            "risk_tier",
            "requires_live_carrier",
        ] {
            if let Some(value) = native.get(key) {
                descriptor[key] = value.clone();
            }
        }
        if let Some(capability) = native.get("capability") {
            for key in [
                "effect",
                "data_policy",
                "authorization_hint",
                "maximum_risk_tier",
            ] {
                if let Some(value) = capability.get(key) {
                    descriptor[key] = value.clone();
                }
            }
        }
        if let Some(spec) = native.get("tool_spec") {
            let mut spec: crate::chat::ToolSpec = serde_json::from_value(spec.clone())
                .map_err(|_| ApprovalReviewError::InvalidCandidate)?;
            crate::ui_model_ids::project_tool(&mut spec);
            descriptor["tool_spec"] =
                serde_json::to_value(spec).map_err(|_| ApprovalReviewError::InvalidCandidate)?;
        }
        let source_kind = match candidate.source {
            ApprovalSource::PermissionItem { .. } => "permission_item",
            ApprovalSource::DirectoryProposal { .. } => "directory_proposal",
            ApprovalSource::OssCommand { .. } | ApprovalSource::ManagerCommand { .. } => "command",
            ApprovalSource::ManagerAction { .. } => "action",
            ApprovalSource::ConcreteCall { .. } => "concrete_call",
        };
        let authority = candidate
            .context
            .current_authority
            .iter()
            .find(|fact| fact.source == candidate.source)
            .ok_or(ApprovalReviewError::InvalidContext)?;
        let mut action: Value = serde_json::from_str(&candidate.action_json)
            .map_err(|_| ApprovalReviewError::InvalidCandidate)?;
        match candidate.source {
            ApprovalSource::PermissionItem { .. } => permission_action(&mut action),
            ApprovalSource::DirectoryProposal { .. } => {
                let proposal = &action["directory_proposal"];
                action = json!({"requested_path":proposal["requested_path"],"canonical_path":proposal["canonical_path"],"purpose":proposal["purpose"],"file_operation_authorized":false});
            }
            ApprovalSource::ManagerCommand { .. } => {
                action = json!({"validation_input":action["validation_input"],"sealed_draft":command_plan(&action["sealed_draft"]),"classification":action["classification"],"admission_policy":action["admission_policy"]});
            }
            _ => {}
        }
        Ok(Self {
            source_kind,
            risk: candidate.risk,
            tool_name: &candidate.tool_name,
            descriptor_json: descriptor.to_string(),
            input_kind: candidate.input_kind,
            action_json: action.to_string(),
            expires_at_unix_ms: candidate.expires_at_unix_ms,
            context: ReviewModelContext {
                current_user_requirement: &candidate.context.current_user_requirement,
                goal_text: &candidate.context.goal_text,
                current_authority: ReviewAuthority {
                    relation: if candidate.input_kind == ApprovalInputKind::ConcreteCall {
                        "existing_scope_for_this_call"
                    } else {
                        "this_pending_request"
                    },
                    status: authority.status,
                },
                skipped_call_count: candidate.context.skipped_calls.len(),
                evidence: &candidate.context.evidence,
                omitted_evidence_count: candidate.context.omitted_evidence_count,
            },
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ModelDecision {
    pub verdict: ApprovalVerdict,
    pub reason_code: String,
    pub reason: String,
    pub evidence_event_ids: Vec<String>,
}
