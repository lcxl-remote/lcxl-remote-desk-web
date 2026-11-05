//! Owner-selected external issue locators. Hosts resolve stable identities at intake.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

pub const MAX_SELECTED_ISSUES: usize = 8;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaRead,
    SchemaWrite,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum IssuePlatform {
    Gitea,
    Github,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct IssueSelection {
    pub connection_ref: String,
    pub owner: String,
    pub repository: String,
    pub issue_number: u64,
}

impl IssueSelection {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_component(&self.connection_ref)
            || !valid_component(&self.owner)
            || !valid_component(&self.repository)
            || self.issue_number == 0
        {
            return Err("Invalid selected external issue");
        }
        Ok(())
    }
}

pub fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
}

pub fn validate_selections(issues: &[IssueSelection]) -> Result<(), &'static str> {
    if issues.len() > MAX_SELECTED_ISSUES {
        return Err("Too many selected external issues");
    }
    let mut ids = std::collections::BTreeSet::new();
    for issue in issues {
        issue.validate()?;
        if !ids.insert((
            &issue.connection_ref,
            issue.owner.to_ascii_lowercase(),
            issue.repository.to_ascii_lowercase(),
            issue.issue_number,
        )) {
            return Err("Duplicate selected external issue");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn issue_selection_round_trips_and_rejects_ambiguous_or_duplicate_targets() {
        let issue = IssueSelection {
            connection_ref: "forge".into(),
            owner: "Org".into(),
            repository: "Project".into(),
            issue_number: 4,
        };
        let encoded = wincode::serialize(&issue).unwrap();
        assert_eq!(
            wincode::deserialize::<IssueSelection>(&encoded).unwrap(),
            issue
        );
        assert!(validate_selections(std::slice::from_ref(&issue)).is_ok());
        let mut duplicate = issue.clone();
        duplicate.owner = "org".into();
        assert!(validate_selections(&[issue.clone(), duplicate]).is_err());
        let mut invalid = issue.clone();
        invalid.repository = "../other".into();
        assert!(invalid.validate().is_err());
        invalid = issue;
        invalid.issue_number = 0;
        assert!(invalid.validate().is_err());
    }
}
