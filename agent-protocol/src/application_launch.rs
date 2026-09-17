//! Application discovery and independently authorized, persistent launches.
//!
//! Catalog entries are untrusted reference data, never launch authority.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

pub const LIST_APPLICATIONS_TOOL: &str = "list_applications";
pub const LAUNCH_APPLICATION_TOOL: &str = "launch_application";

/// Host-authored identity facts; never accepted as model launch arguments.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ResolvedApplicationIdentity {
    pub canonical_target: String,
    pub identity_digest: String,
    pub resolved_cwd: Option<String>,
    pub user_identity: String,
    pub session_identity: String,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ListApplicationsRequest {
    #[serde(default)]
    pub queries: Vec<String>,
    #[serde(default)]
    pub allow_unfiltered: bool,
    #[serde(default = "default_limit")]
    pub limit: u32,
    pub cursor: Option<String>,
}

fn default_limit() -> u32 {
    20
}

impl ListApplicationsRequest {
    /// Must run before enumeration, including requests carrying a cursor.
    pub fn normalize(&mut self) -> Result<(), &'static str> {
        if self.queries.len() > 16 || !(1..=100).contains(&self.limit) {
            return Err("queries accepts at most 16 alternatives; limit must be 1..=100");
        }
        let mut normalized = Vec::new();
        for query in &self.queries {
            let query = query.trim();
            if query.is_empty() || query.len() > 128 || query.contains('\0') {
                return Err("each query must contain 1..=128 UTF-8 bytes without NUL");
            }
            let query = query.to_lowercase();
            if !normalized.contains(&query) {
                normalized.push(query);
            }
        }
        if normalized.is_empty() && !self.allow_unfiltered {
            return Err("provide queries or explicitly set allow_unfiltered=true");
        }
        if self
            .cursor
            .as_ref()
            .is_some_and(|v| v.is_empty() || v.len() > 512 || v.contains('\0'))
        {
            return Err("invalid application catalog cursor");
        }
        self.queries = normalized;
        Ok(())
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationTargetKind {
    Executable,
    MacosBundle,
    WindowsAppId,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ApplicationTarget {
    pub kind: ApplicationTargetKind,
    pub value: String,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct LaunchApplicationRequest {
    pub target: ApplicationTarget,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub run_as_admin: bool,
}

impl LaunchApplicationRequest {
    /// Pure bounds validation. The target host separately resolves absolute paths,
    /// file identity, user token, environment and platform support before approval.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.target.value.trim().is_empty()
            || self.target.value.len() > 4096
            || self.target.value.contains('\0')
        {
            return Err("invalid application target");
        }
        if self.args.len() > 256
            || self
                .args
                .iter()
                .any(|v| v.contains('\0') || v.len() > 16384)
            || self.args.iter().map(String::len).sum::<usize>() > 32768
        {
            return Err("application arguments exceed bounds or contain NUL");
        }
        if self
            .cwd
            .as_ref()
            .is_some_and(|v| v.trim().is_empty() || v.len() > 4096 || v.contains('\0'))
        {
            return Err("invalid working directory");
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
pub struct ApplicationCatalogEntry {
    pub display_name: String,
    pub aliases: Vec<String>,
    pub target: Option<ApplicationTarget>,
    /// None means unavailable or not reliably parsed; Some([]) means no arguments.
    pub suggested_args: Option<Vec<String>>,
    /// Platform entry syntax, such as desktop Exec field codes; never executable argv.
    pub argument_template: Option<String>,
    pub suggested_cwd: Option<String>,
    pub sources: Vec<String>,
    pub unsupported_reason: Option<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
pub struct ApplicationCatalogPage {
    pub entries: Vec<ApplicationCatalogEntry>,
    pub next_cursor: Option<String>,
    pub enumeration_complete: bool,
    pub warnings: Vec<String>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum LaunchOutcome {
    NotDispatched,
    LaunchAccepted,
    LaunchFailed,
    OutcomeUnknown,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ArgumentDelivery {
    NotRequested,
    Submitted,
    Unsupported,
    Unknown,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum LaunchFailureReason {
    InvalidTarget,
    IdentityChanged,
    ElevationRequired,
    AdminRequired,
    AdminLaunchUnavailable,
    Unsupported,
    SessionUnavailable,
    LifetimeIsolationUnavailable,
    PermissionDenied,
    NativeFailure,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationObservationKind {
    ApplicationObserved,
    WindowObserved,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
pub struct ApplicationLaunchObservation {
    pub kind: ApplicationObservationKind,
    pub observed_at_unix_ms: u64,
    pub process_id: Option<u32>,
    /// Separate from requested privilege and from a newly created launcher process.
    pub elevated: Option<bool>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
pub struct LaunchApplicationResult {
    pub dispatch_id: String,
    pub launch_outcome: LaunchOutcome,
    pub argument_delivery: ArgumentDelivery,
    pub failure_reason: Option<LaunchFailureReason>,
    pub requested_admin: bool,
    pub created_process_id: Option<u32>,
    pub created_process_elevated: Option<bool>,
    pub observations: Vec<ApplicationLaunchObservation>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn search_requires_explicit_intent_even_with_cursor() {
        for value in [
            json!({}),
            json!({"cursor":"existing"}),
            json!({"queries":[]}),
        ] {
            let mut request: ListApplicationsRequest = serde_json::from_value(value).unwrap();
            assert!(request.normalize().is_err());
        }
        let mut request: ListApplicationsRequest =
            serde_json::from_value(json!({"allow_unfiltered":true})).unwrap();
        assert!(request.normalize().is_ok());
        request.queries = vec![" ".into()];
        assert!(request.normalize().is_err());
    }

    #[test]
    fn queries_are_trimmed_deduplicated_and_unicode_bounded() {
        let mut request: ListApplicationsRequest =
            serde_json::from_value(json!({"queries":[" Chrome ","CHROME","谷歌"]})).unwrap();
        request.normalize().unwrap();
        assert_eq!(request.queries, ["chrome", "谷歌"]);
        request.queries = vec!["中".repeat(43)];
        assert!(request.normalize().is_err());
    }

    #[test]
    fn defaults_are_canonical_but_admin_and_arguments_are_distinct() {
        let value = json!({"target":{"kind":"executable","value":"C:\\Apps\\app.exe"}});
        let request: LaunchApplicationRequest = serde_json::from_value(value.clone()).unwrap();
        assert!(!request.run_as_admin);
        assert!(request.args.is_empty());
        assert!(request.validate().is_ok());
        let mut explicit = value;
        explicit["run_as_admin"] = json!(false);
        explicit["args"] = json!([]);
        explicit["cwd"] = json!(null);
        assert_eq!(serde_json::to_value(&request).unwrap(), explicit);
        let mut elevated = request.clone();
        elevated.run_as_admin = true;
        assert_ne!(serde_json::to_value(&elevated).unwrap(), explicit);
        elevated.args = vec![
            "".into(),
            "https://example.test".into(),
            "-Command".into(),
            "Write-Output 'hello'".into(),
        ];
        assert!(elevated.validate().is_ok());
        elevated.args.push("bad\0argument".into());
        assert!(elevated.validate().is_err());
    }

    #[test]
    fn unknown_launch_options_are_not_silently_ignored() {
        assert!(
            serde_json::from_value::<LaunchApplicationRequest>(json!({
                "target":{"kind":"executable","value":"/usr/bin/app"}, "url":"https://example.test"
            }))
            .is_err()
        );
    }
}

/// Internal preflight receipt; never an application-catalog entry or model input.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
pub struct LaunchPreflightReceipt {
    pub identity: ResolvedApplicationIdentity,
    pub target: crate::computer_use::ObjectRef,
    pub interactive_session_incarnation: String,
}

pub fn launch_input_digest(
    request: &LaunchApplicationRequest,
    identity: &ResolvedApplicationIdentity,
) -> Result<String, &'static str> {
    request.validate()?;
    if identity.canonical_target.is_empty()
        || identity.identity_digest.is_empty()
        || identity.user_identity.is_empty()
        || identity.session_identity.is_empty()
    {
        return Err("host application identity is incomplete");
    }
    let bytes = serde_json::to_vec(&(request, identity)).map_err(|_| "invalid launch input")?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

/// Server-owned subject and revisions, not accepted as model tool arguments.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct LaunchApprovalSubject {
    pub actor_id: String,
    pub device_id: String,
    pub session_id: String,
    pub input_revision: u64,
    pub policy_revision: i64,
    pub readiness_revision: u64,
}

/// An approval freezes both explicit arguments and independently resolved host facts.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct LaunchApprovalBinding {
    subject: LaunchApprovalSubject,
    request: LaunchApplicationRequest,
    identity: ResolvedApplicationIdentity,
    digest: String,
    target: Option<crate::computer_use::ObjectRef>,
}

impl LaunchApprovalBinding {
    pub fn prepare(
        subject: LaunchApprovalSubject,
        request: LaunchApplicationRequest,
        identity: ResolvedApplicationIdentity,
    ) -> Result<Self, &'static str> {
        if subject.actor_id.is_empty()
            || subject.device_id.is_empty()
            || subject.session_id.is_empty()
            || subject.input_revision == 0
            || subject.policy_revision < 1
            || subject.readiness_revision == 0
        {
            return Err("launch approval requires current server-owned identity and revisions");
        }
        let input_digest = launch_input_digest(&request, &identity)?;
        let bytes =
            serde_json::to_vec(&(&subject, input_digest)).map_err(|_| "invalid launch binding")?;
        Ok(Self {
            subject,
            request,
            identity,
            digest: format!("{:x}", Sha256::digest(bytes)),
            target: None,
        })
    }

    pub fn revalidate(
        &self,
        subject: &LaunchApprovalSubject,
        request: &LaunchApplicationRequest,
        identity: &ResolvedApplicationIdentity,
    ) -> Result<(), &'static str> {
        let mut expected = Self::prepare(subject.clone(), request.clone(), identity.clone())?;
        if let Some(target) = &self.target {
            expected = expected.with_target(target.clone())?;
        }
        if &expected != self {
            return Err("launch identity, parameters or authority changed; request a new approval");
        }
        Ok(())
    }

    pub fn with_target(
        mut self,
        target: crate::computer_use::ObjectRef,
    ) -> Result<Self, &'static str> {
        if self.target.is_some()
            || target.object_kind != crate::computer_use::ObjectKind::ApplicationLaunchTarget
            || target.token.is_empty()
            || target.snapshot_id.is_empty()
        {
            return Err("invalid native launch reference");
        }
        let bytes =
            serde_json::to_vec(&(&self.digest, &target)).map_err(|_| "invalid launch reference")?;
        self.digest = format!("{:x}", Sha256::digest(bytes));
        self.target = Some(target);
        Ok(self)
    }

    pub fn target(&self) -> Option<&crate::computer_use::ObjectRef> {
        self.target.as_ref()
    }

    pub fn request(&self) -> &LaunchApplicationRequest {
        &self.request
    }
    pub fn identity(&self) -> &ResolvedApplicationIdentity {
        &self.identity
    }
    pub fn subject(&self) -> &LaunchApprovalSubject {
        &self.subject
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
    pub fn resource_scope(&self) -> Vec<String> {
        vec![format!("application_input:sha256:{}", self.digest)]
    }
}
