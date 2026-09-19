//! Platform-native service search and bounded page results.
use crate::native_diagnostic::NativeDiagnostic;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

pub const WINDOWS_START_TYPES: &[&str] = &["auto", "manual", "disabled", "system", "boot"];
pub const UNIT_FILE_STATES: &[&str] = &[
    "enabled",
    "enabled-runtime",
    "linked",
    "linked-runtime",
    "alias",
    "masked",
    "masked-runtime",
    "static",
    "disabled",
    "indirect",
    "generated",
    "transient",
    "bad",
];
pub const LAUNCH_POLICIES: &[&str] = &[
    "run_at_load",
    "keep_alive",
    "scheduled",
    "socket",
    "mach_service",
    "path_watch",
];
pub const MAX_SERVICE_PAGE_BYTES: usize = 32 * 1024;

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(default, deny_unknown_fields)]
pub struct ServiceStatusParams {
    pub queries: Vec<String>,
    pub allow_unfiltered: bool,
    pub start_types: Vec<String>,
    pub unit_file_states: Vec<String>,
    pub launch_policies: Vec<String>,
    pub scope: Option<String>,
    pub limit: u32,
    pub cursor: Option<String>,
}
impl Default for ServiceStatusParams {
    fn default() -> Self {
        Self {
            queries: vec![],
            allow_unfiltered: false,
            start_types: vec![],
            unit_file_states: vec![],
            launch_policies: vec![],
            scope: None,
            limit: 50,
            cursor: None,
        }
    }
}
impl ServiceStatusParams {
    pub fn validate_selection(&self) -> Result<(), &'static str> {
        if !crate::validate_search_terms(&self.queries) {
            return Err("queries accepts at most 16 nonempty substrings, each at most 128 bytes");
        }
        for (values, allowed) in [
            (&self.start_types, WINDOWS_START_TYPES),
            (&self.unit_file_states, UNIT_FILE_STATES),
            (&self.launch_policies, LAUNCH_POLICIES),
        ] {
            if values.len() > 16 || values.iter().any(|s| !allowed.contains(&s.as_str())) {
                return Err(
                    "unsupported service filter value; use the platform's advertised values (at most 16)",
                );
            }
        }
        if !(1..=200).contains(&self.limit) {
            return Err("limit must be 1..=200");
        }
        if self
            .cursor
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > 4096)
        {
            return Err("invalid service cursor");
        }
        if self
            .scope
            .as_ref()
            .is_some_and(|s| !["system", "user", "all"].contains(&s.as_str()))
        {
            return Err("scope must be system, user or all");
        }
        if self.queries.is_empty()
            && self.start_types.is_empty()
            && self.unit_file_states.is_empty()
            && self.launch_policies.is_empty()
            && !self.allow_unfiltered
        {
            return Err(
                "Search conditions are required: use queries or platform filters, or allow_unfiltered=true for enumeration",
            );
        }
        Ok(())
    }
    pub fn validate_platform(&self, platform: &str) -> Result<(), &'static str> {
        self.validate_selection()?;
        match platform {
            "windows"
                if self.unit_file_states.is_empty()
                    && self.launch_policies.is_empty()
                    && self.scope.is_none() =>
            {
                Ok(())
            }
            "linux" if self.start_types.is_empty() && self.launch_policies.is_empty() => Ok(()),
            "macos"
                if self.start_types.is_empty()
                    && self.unit_file_states.is_empty()
                    && self.scope.is_none() =>
            {
                Ok(())
            }
            _ => Err(
                "filters do not apply to this platform: Windows=start_types; Linux=unit_file_states,scope; macOS=launch_policies",
            ),
        }
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ServiceEntry {
    pub name: String,
    pub display_name: Option<String>,
    pub state: String,
    pub start_type: Option<String>,
    pub scope: String,
    pub unit_file_state: Option<String>,
    /// None means unknown; an empty array means no advertised policy was found.
    pub launch_policies: Option<Vec<String>>,
    pub policy_source: Option<String>,
    pub policy_domain: Option<String>,
    pub policy_summary: Option<String>,
    pub metadata_error: Option<NativeDiagnostic>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ServiceStatusOutput {
    pub services: Vec<ServiceEntry>,
    pub truncated: bool,
    pub next_cursor: Option<String>,
    pub scope: String,
    pub collected_at_unix_ms: u64,
    /// False for an incomplete scan or unavailable metadata, independently of paging.
    pub complete: bool,
    pub unknown_count: u32,
    pub errors: Vec<NativeDiagnostic>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn platform_filters_count_as_search_but_paging_does_not() {
        assert!(ServiceStatusParams::default().validate_selection().is_err());
        let p = ServiceStatusParams {
            start_types: vec!["auto".into(), "manual".into()],
            ..Default::default()
        };
        assert!(p.validate_platform("windows").is_ok());
        assert!(p.validate_platform("linux").is_err());
        assert!(
            ServiceStatusParams {
                allow_unfiltered: true,
                ..Default::default()
            }
            .validate_platform("macos")
            .is_ok()
        );
    }
    #[test]
    fn search_filters_and_paging_survive_binary_and_json_transport() {
        let params = ServiceStatusParams {
            queries: vec!["Windows".into(), "服务".into()],
            start_types: vec!["auto".into(), "manual".into()],
            limit: 17,
            cursor: Some("opaque".into()),
            ..Default::default()
        };
        let bytes = wincode::serialize(&params).unwrap();
        assert_eq!(
            wincode::deserialize::<ServiceStatusParams>(&bytes).unwrap(),
            params
        );
        assert_eq!(
            serde_json::from_str::<ServiceStatusParams>(&serde_json::to_string(&params).unwrap())
                .unwrap(),
            params
        );
    }
    #[test]
    fn rejects_invalid_filter_and_limit() {
        assert!(
            ServiceStatusParams {
                start_types: vec!["automatic".into()],
                ..Default::default()
            }
            .validate_selection()
            .is_err()
        );
        assert!(
            ServiceStatusParams {
                allow_unfiltered: true,
                limit: 201,
                ..Default::default()
            }
            .validate_selection()
            .is_err()
        );
    }
}
