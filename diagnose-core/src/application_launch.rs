//! Pure application catalog filtering and launch dispatch contracts.
pub mod completion;
mod preflight;
use desk_agent_protocol::application_launch::{
    ApplicationCatalogEntry, ApplicationCatalogPage, LaunchApplicationRequest,
    ListApplicationsRequest,
};
pub use preflight::LaunchCallPreflight;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use sha2::{Digest, Sha256};

pub const PROVIDER_ID: &str = "application.launch";
pub const CAPABILITY_ID: &str = "application.launch.confirmed";
pub const ADAPTER_ID: &str = "native-application-launch";
pub const ADAPTER_VERSION: &str = "native-application-launch/v1";
pub const TOOL_NAME: &str = "launch_application";

pub fn tool() -> crate::registry::RegisteredTool {
    crate::registry::RegisteredTool {
        spec: crate::chat::ToolSpec {
            name: TOOL_NAME.into(),
            description: "Start a generic application or long-running executable on the selected device with one exact R3 owner approval. First describe_tools, then request_permissions with the complete exact_input and wait for approval. Supply an absolute executable path, an absolute macOS .app bundle path, or an exact Windows packaged application ID. list_applications is optional discovery only: choose the arguments yourself; catalog arguments and working directories are never inherited automatically. args defaults to []; cwd is optional and must be absolute when provided (unsupported for macOS bundles and Windows packaged apps). run_as_admin defaults to false and is Windows-executable-only: it uses this user's already available elevated token, never SYSTEM, never UAC or a password prompt. Ordinary launches that require elevation fail. No URL or document association lookup is performed; pass URLs as arguments to an explicitly selected executable. Unlike exec_command, this tool does not capture stdout/stderr and does not reclaim the application process tree when the task finishes or is cancelled. A receipt proves only the reported launch outcome, not window/browser readiness. Obtain separate observation/control permissions as needed. Explain failures from diagnostic.stage, operation, domain and code; native OS refusal after approval is not an approval denial. Distinguish target elevation requirements from unavailable host administrator tokens and helper failures. Do not infer a UAC dialog. Never retry OutcomeUnknown automatically.".into(),
            parameters_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "target": {"type":"object", "additionalProperties":false,
                        "properties": {"kind":{"type":"string","enum":["executable","macos_bundle","windows_app_id"]},
                            "value":{"type":"string","minLength":1,"maxLength":4096}},
                        "required":["kind","value"]},
                    "args":{"type":"array","maxItems":256,"items":{"type":"string","maxLength":16384},"default":[]},
                    "cwd":{"type":["string","null"],"minLength":1,"maxLength":4096},
                    "run_as_admin":{"type":"boolean","default":false}
                }, "required":["target"]
            }),
        },
        required_capability: desk_agent_protocol::Capability::ApplicationLaunchConfirmed,
        effect: crate::registry::ToolEffect::Mutating,
    }
}

/// Catalog contents belong to one authenticated device/session snapshot.
/// The host retains this object and supplies unpredictable cursor tokens.
pub struct ApplicationCatalogSnapshot {
    device_id: String,
    session_id: String,
    expires_at_unix_ms: u64,
    request: ListApplicationsRequest,
    entries: Vec<ApplicationCatalogEntry>,
    enumeration_complete: bool,
    warnings: Vec<String>,
    continuations: Vec<(String, usize)>,
}

impl ApplicationCatalogSnapshot {
    pub fn new(
        device_id: String,
        session_id: String,
        expires_at_unix_ms: u64,
        mut request: ListApplicationsRequest,
        entries: Vec<ApplicationCatalogEntry>,
        enumeration_complete: bool,
        warnings: Vec<String>,
    ) -> Result<Self, &'static str> {
        request.normalize()?;
        if request.cursor.is_some() || device_id.is_empty() || session_id.is_empty() {
            return Err("new catalog snapshots require a subject and no cursor");
        }
        // Filter the complete collected set before slicing pages.
        let mut entries: Vec<_> = entries
            .into_iter()
            .filter(|entry| {
                request.queries.is_empty()
                    || request.queries.iter().any(|query| {
                        std::iter::once(&entry.display_name)
                            .chain(entry.aliases.iter())
                            .chain(entry.target.iter().map(|target| &target.value))
                            .any(|value| value.to_lowercase().contains(query))
                    })
            })
            .collect();
        entries.sort_by_cached_key(|entry| {
            (
                entry.display_name.to_lowercase(),
                serde_json::to_string(&entry.target).unwrap_or_default(),
                entry.suggested_args.clone(),
                entry.suggested_cwd.clone(),
            )
        });
        Ok(Self {
            device_id,
            session_id,
            expires_at_unix_ms,
            request,
            entries,
            enumeration_complete,
            warnings,
            continuations: Vec::new(),
        })
    }

    /// Cursor storage is host-owned. Never derive offsets from model input.
    pub fn page(
        &mut self,
        device_id: &str,
        session_id: &str,
        now_unix_ms: u64,
        mut request: ListApplicationsRequest,
        next_cursor_token: String,
    ) -> Result<ApplicationCatalogPage, &'static str> {
        request.normalize()?;
        if self.device_id != device_id
            || self.session_id != session_id
            || now_unix_ms >= self.expires_at_unix_ms
            || request.queries != self.request.queries
            || request.allow_unfiltered != self.request.allow_unfiltered
        {
            return Err("application snapshot expired or belongs to another query/session");
        }
        let offset = match request.cursor.as_ref() {
            None => 0,
            Some(token) => self
                .continuations
                .iter()
                .find(|(value, _)| value == token)
                .map(|(_, offset)| *offset)
                .ok_or("invalid application cursor")?,
        };
        let end = (offset + request.limit as usize).min(self.entries.len());
        let next_cursor = if end < self.entries.len() {
            if next_cursor_token.is_empty()
                || next_cursor_token.len() > 512
                || next_cursor_token.contains('\0')
            {
                return Err("host must provide a bounded opaque cursor");
            }
            if let Some((token, _)) = self.continuations.iter().find(|(_, offset)| *offset == end) {
                Some(token.clone())
            } else {
                if self
                    .continuations
                    .iter()
                    .any(|(token, _)| *token == next_cursor_token)
                {
                    return Err("host cursor collision");
                }
                self.continuations.push((next_cursor_token.clone(), end));
                Some(next_cursor_token)
            }
        } else {
            None
        };
        Ok(ApplicationCatalogPage {
            entries: self.entries[offset..end].to_vec(),
            next_cursor,
            enumeration_complete: self.enumeration_complete,
            warnings: self.warnings.clone(),
        })
    }
}

/// The storage adapter must compare-and-swap these transitions durably.
/// Invoking has no lease expiry or retry transition: a crash is OutcomeUnknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchDispatchState {
    Prepared,
    CancelledBeforeInvoke,
    Invoking,
    Recorded,
}

impl LaunchDispatchState {
    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Prepared, Self::CancelledBeforeInvoke | Self::Invoking)
                | (Self::Invoking, Self::Recorded)
        )
    }
}

pub use desk_agent_protocol::application_launch::ResolvedApplicationIdentity;

pub use desk_agent_protocol::application_launch::{
    LaunchApprovalBinding, LaunchApprovalSubject, launch_input_digest,
};

/// Resolve targets before displaying an approval. Catalog discovery is optional.
pub async fn bind_permission_request(
    request: &mut crate::dynamic_run::PermissionRequest,
    session: &crate::session::PersistedAgentSession,
    readiness_revision: u64,
    tools: &dyn crate::seam::ToolSeam,
) -> Result<(), desk_agent_protocol::AgentError> {
    for item in &mut request.items {
        if item.tool_name != "launch_application" {
            continue;
        }
        let canonical = item
            .canonical_input_json
            .as_deref()
            .ok_or_else(|| permission_error("Application launch requires exact input"))?;
        let input: LaunchApplicationRequest = serde_json::from_str(canonical)
            .map_err(|_| permission_error("Invalid application launch input"))?;
        input.validate().map_err(permission_error)?;
        let receipt = tools.resolve_application_candidate(&input).await?;
        let subject = LaunchApprovalSubject {
            actor_id: session.actor_id.clone(),
            device_id: session.device_id.clone(),
            session_id: receipt.interactive_session_incarnation.clone(),
            input_revision: session.input_revision,
            policy_revision: session.policy_revision,
            readiness_revision,
        };
        let binding = LaunchApprovalBinding::prepare(subject, input, receipt.identity)
            .and_then(|binding| binding.with_target(receipt.target))
            .map_err(permission_error)?;
        item.resource_scope = binding.resource_scope();
        item.launch_confirmation = Some(binding);
        item.validate()
            .map_err(|_| permission_error("Invalid application launch approval"))?;
    }
    Ok(())
}

/// Recover only a persisted owner decision with unchanged arguments and authority.
pub fn approved_binding(
    session: &crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    readiness_revision: u64,
) -> Result<LaunchApprovalBinding, desk_agent_protocol::AgentError> {
    use crate::dynamic_run::PermissionRequestState;
    let input: LaunchApplicationRequest = serde_json::from_str(&call.arguments_json)
        .map_err(|_| permission_error("Invalid launch input"))?;
    input.validate().map_err(permission_error)?;
    if call.name != TOOL_NAME {
        return Err(permission_error("Not an application launch"));
    }
    for request in session.permission_requests.iter().rev() {
        if request.input_revision != session.input_revision
            || !matches!(
                request.state,
                PermissionRequestState::Approved | PermissionRequestState::PartiallyApproved
            )
        {
            continue;
        }
        for item in &request.items {
            let Some(binding) = &item.launch_confirmation else {
                continue;
            };
            let subject = binding.subject();
            if item.tool_name != TOOL_NAME
                || item.validate().is_err()
                || binding.target().is_none()
                || binding.request() != &input
                || subject.actor_id != session.actor_id
                || subject.device_id != session.device_id
                || subject.input_revision != session.input_revision
                || subject.policy_revision != session.policy_revision
                || subject.readiness_revision != readiness_revision
            {
                continue;
            }
            binding
                .revalidate(subject, &input, binding.identity())
                .map_err(permission_error)?;
            return Ok(binding.clone());
        }
    }
    Err(permission_error(
        "Application launch needs a current exact owner approval",
    ))
}

fn permission_error(message: impl Into<String>) -> desk_agent_protocol::AgentError {
    desk_agent_protocol::AgentError {
        kind: desk_agent_protocol::AgentErrorKind::InvalidInput,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::application_launch::{ApplicationTarget, ApplicationTargetKind};

    #[test]
    fn launch_approval_cannot_be_reused_for_another_subject_or_tampered_digest() {
        let request = LaunchApplicationRequest {
            target: ApplicationTarget {
                kind: ApplicationTargetKind::Executable,
                value: "/application".into(),
            },
            args: vec![],
            cwd: None,
            run_as_admin: false,
        };
        let identity = ResolvedApplicationIdentity {
            canonical_target: "/application".into(),
            identity_digest: "file".into(),
            resolved_cwd: None,
            user_identity: "user".into(),
            session_identity: "os-session".into(),
        };
        let subject = LaunchApprovalSubject {
            actor_id: "owner".into(),
            device_id: "device".into(),
            session_id: "host-session".into(),
            input_revision: 1,
            policy_revision: 1,
            readiness_revision: 1,
        };
        let binding =
            LaunchApprovalBinding::prepare(subject.clone(), request.clone(), identity.clone())
                .unwrap();
        binding.revalidate(&subject, &request, &identity).unwrap();
        let mut changed = subject.clone();
        changed.session_id = "other-host-session".into();
        assert!(binding.revalidate(&changed, &request, &identity).is_err());
        let mut json = serde_json::to_value(&binding).unwrap();
        json["digest"] = serde_json::json!("forged");
        let forged: LaunchApprovalBinding = serde_json::from_value(json).unwrap();
        assert!(forged.revalidate(&subject, &request, &identity).is_err());
        assert!(binding.resource_scope()[0].starts_with("application_input:sha256:"));
        assert!(!binding.resource_scope()[0].contains("/application"));
    }

    #[test]
    fn permission_item_rejects_changed_arguments_and_approval_scope() {
        use crate::dynamic_run::GrantRequestItem;
        use desk_agent_protocol::capability_provider::CapabilityEffect;
        let input = LaunchApplicationRequest {
            target: ApplicationTarget {
                kind: ApplicationTargetKind::Executable,
                value: "/app".into(),
            },
            args: vec!["original".into()],
            cwd: None,
            run_as_admin: false,
        };
        let identity = ResolvedApplicationIdentity {
            canonical_target: "/app".into(),
            identity_digest: "file-digest".into(),
            resolved_cwd: Some("/home/owner".into()),
            user_identity: "owner".into(),
            session_identity: "session".into(),
        };
        let binding = LaunchApprovalBinding::prepare(
            LaunchApprovalSubject {
                actor_id: "owner".into(),
                device_id: "device".into(),
                session_id: "session".into(),
                input_revision: 1,
                policy_revision: 1,
                readiness_revision: 1,
            },
            input.clone(),
            identity,
        )
        .unwrap();
        let canonical = serde_json::to_string(&input).unwrap();
        let mut item = GrantRequestItem {
            item_id: "launch".into(),
            provider_id: "application.launch".into(),
            tool_name: "launch_application".into(),
            expected_effect: CapabilityEffect::LaunchApplication,
            resource_scope: binding.resource_scope(),
            operation_scope: vec!["launch_application".into()],
            export_destinations: vec![],
            canonical_input_digest_sha256: Some(format!(
                "{:x}",
                Sha256::digest(canonical.as_bytes())
            )),
            canonical_input_json: Some(canonical),
            command_confirmation: None,
            launch_confirmation: Some(binding),
            suggested_ttl_seconds: 120,
            suggested_max_uses: 1,
            reason: "Start the requested app".into(),
        };
        item.validate().unwrap();
        let original = item.clone();
        let mut changed = input;
        changed.run_as_admin = true;
        let canonical = serde_json::to_string(&changed).unwrap();
        item.canonical_input_digest_sha256 =
            Some(format!("{:x}", Sha256::digest(canonical.as_bytes())));
        item.canonical_input_json = Some(canonical);
        assert!(item.validate().is_err());
        let mut item = original.clone();
        item.resource_scope = vec!["application:any".into()];
        assert!(item.validate().is_err());
        let mut item = original;
        item.suggested_max_uses = 2;
        assert!(item.validate().is_err());
    }

    #[test]
    fn launch_contract_is_exact_one_shot_and_not_command_authority() {
        let providers = crate::ai_assistant::ai_assistant_provider_registry();
        let launch = providers.capability_for_tool(TOOL_NAME).unwrap();
        assert_eq!(
            launch.required_capability,
            desk_agent_protocol::Capability::ApplicationLaunchConfirmed
        );
        assert_eq!(launch.wire.authorization_hint.resources,
            vec![desk_agent_protocol::capability_provider::AuthorizationResourceKind::ExactApplication]);
        assert_eq!(
            crate::capability_risk::classify_provider_descriptor_floor(
                launch.wire.effect,
                &launch.wire.data_policy
            ),
            desk_agent_protocol::capability_grant::CapabilityRiskTier::R3
        );
        for platform in [
            desk_agent_protocol::capability_provider::CapabilityPlatform::Windows,
            desk_agent_protocol::capability_provider::CapabilityPlatform::Macos,
            desk_agent_protocol::capability_provider::CapabilityPlatform::Linux,
        ] {
            assert!(launch.wire.prerequisites.platforms.contains(&platform));
        }
        let short = serde_json::json!({"target":{"kind":"executable","value":"/app"}});
        let full = serde_json::json!({"target":{"kind":"executable","value":"/app"},"args":[],"cwd":null,"run_as_admin":false});
        assert_eq!(
            crate::permission_tools::canonical_tool_permission_input_json(TOOL_NAME, short)
                .unwrap(),
            crate::permission_tools::canonical_tool_permission_input_json(TOOL_NAME, full).unwrap()
        );
        assert!(
            providers
                .capability_for_tool("execute_confirmed_command")
                .is_none()
        );
    }

    #[test]
    fn application_catalog_requires_permission_and_is_not_a_diagnostic_tool() {
        let providers = crate::ai_assistant::ai_assistant_provider_registry();
        let capability = providers.capability_for_tool("list_applications").unwrap();
        assert_eq!(
            capability.required_capability,
            desk_agent_protocol::Capability::ApplicationList
        );
        assert_eq!(
            crate::capability_risk::classify_provider_descriptor_floor(
                capability.wire.effect,
                &capability.wire.data_policy
            ),
            desk_agent_protocol::capability_grant::CapabilityRiskTier::R1
        );
        assert!(crate::ai_assistant::is_requestable_desktop_read(
            "list_applications"
        ));
        assert!(
            crate::read_tools::read_tool_registry()
                .iter()
                .all(|tool| tool.name() != "list_applications")
        );
        let call = crate::chat::ToolCall {
            id: "catalog".into(),
            name: "list_applications".into(),
            arguments_json: "{}".into(),
        };
        assert!(crate::read_tools::build_read_operation(&call).is_err());
    }

    fn query(queries: &[&str]) -> ListApplicationsRequest {
        ListApplicationsRequest {
            queries: queries.iter().map(|v| (*v).into()).collect(),
            allow_unfiltered: false,
            limit: 1,
            cursor: None,
        }
    }
    fn entry(name: &str) -> ApplicationCatalogEntry {
        ApplicationCatalogEntry {
            display_name: name.into(),
            aliases: vec![],
            target: None,
            suggested_args: None,
            argument_template: None,
            suggested_cwd: None,
            sources: vec![],
            unsupported_reason: None,
        }
    }
    #[test]
    fn search_precedes_paging_and_cursor_is_subject_query_and_expiry_bound() {
        let request = query(&["app"]);
        let mut snapshot = ApplicationCatalogSnapshot::new(
            "device".into(),
            "session".into(),
            100,
            request.clone(),
            vec![entry("Other"), entry("App B"), entry("App A")],
            false,
            vec!["source unavailable".into()],
        )
        .unwrap();
        let page = snapshot
            .page("device", "session", 1, request.clone(), "opaque".into())
            .unwrap();
        assert_eq!(page.entries[0].display_name, "App A");
        assert!(!page.enumeration_complete);
        let mut next = request;
        next.cursor = page.next_cursor;
        assert!(
            snapshot
                .page("other", "session", 2, next.clone(), "next".into())
                .is_err()
        );
        assert!(
            snapshot
                .page("device", "other", 2, next.clone(), "next".into())
                .is_err()
        );
        assert!(
            snapshot
                .page("device", "session", 100, next.clone(), "next".into())
                .is_err()
        );
        let mut changed = next.clone();
        changed.queries = vec!["other".into()];
        assert!(
            snapshot
                .page("device", "session", 2, changed, "next".into())
                .is_err()
        );
        let page = snapshot
            .page("device", "session", 2, next, "next".into())
            .unwrap();
        assert_eq!(page.entries[0].display_name, "App B");
        assert!(page.next_cursor.is_none());
    }
    #[test]
    fn dispatch_cannot_retry_or_cancel_after_invocation_boundary() {
        use LaunchDispatchState::*;
        assert!(Prepared.can_transition_to(Invoking));
        assert!(Prepared.can_transition_to(CancelledBeforeInvoke));
        assert!(Invoking.can_transition_to(Recorded));
        for terminal in [CancelledBeforeInvoke, Recorded] {
            for next in [Prepared, Invoking, Recorded, CancelledBeforeInvoke] {
                assert!(!terminal.can_transition_to(next));
            }
        }
        assert!(!Invoking.can_transition_to(Prepared));
        assert!(!Invoking.can_transition_to(CancelledBeforeInvoke));
    }
    #[test]
    fn approval_digest_binds_arguments_identity_cwd_user_session_and_admin() {
        let request = LaunchApplicationRequest {
            target: ApplicationTarget {
                kind: ApplicationTargetKind::Executable,
                value: "/app".into(),
            },
            args: vec![],
            cwd: None,
            run_as_admin: false,
        };
        let identity = ResolvedApplicationIdentity {
            canonical_target: "/app".into(),
            identity_digest: "file-version".into(),
            resolved_cwd: Some("/home/user".into()),
            user_identity: "user".into(),
            session_identity: "session".into(),
        };
        let digest = launch_input_digest(&request, &identity).unwrap();
        for mutate in [
            |r: &mut LaunchApplicationRequest| r.run_as_admin = true,
            |r: &mut LaunchApplicationRequest| r.args.push("new".into()),
            |r: &mut LaunchApplicationRequest| r.cwd = Some("/tmp".into()),
        ] {
            let mut changed = request.clone();
            mutate(&mut changed);
            assert_ne!(digest, launch_input_digest(&changed, &identity).unwrap());
        }
        for mutate in [
            |i: &mut ResolvedApplicationIdentity| i.identity_digest.push('2'),
            |i: &mut ResolvedApplicationIdentity| i.user_identity.push('2'),
            |i: &mut ResolvedApplicationIdentity| i.session_identity.push('2'),
            |i: &mut ResolvedApplicationIdentity| i.resolved_cwd = Some("/other".into()),
        ] {
            let mut changed = identity.clone();
            mutate(&mut changed);
            assert_ne!(digest, launch_input_digest(&request, &changed).unwrap());
        }
    }
}
