//! Host-owned external connections. Public projections never include credentials.

use desk_agent_protocol::ai_assistant::issue_subscription::IssueSubscriptionAccounts;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{McpClientError, endpoint_url, issue_connection::IssueConnection};
pub use desk_agent_protocol::external_tools::IssuePlatform;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueConnectionConfig {
    pub platform: IssuePlatform,
    pub connection_ref: String,
    pub name: String,
    pub enabled: bool,
    pub instance_url: String,
    pub mcp_url: String,
    pub allowed_actor_ids: Vec<String>,
    token: Option<String>,
    extra_ca_pem: Option<String>,
}

impl std::fmt::Debug for IssueConnectionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssueConnectionConfig")
            .field("connection_ref", &self.connection_ref)
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueConnectionsConfig {
    pub revision: u64,
    pub connections: Vec<IssueConnectionConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct IssueConnectionPublic {
    pub platform: IssuePlatform,
    pub connection_ref: String,
    pub name: String,
    pub enabled: bool,
    pub instance_url: String,
    pub mcp_url: String,
    pub allowed_actor_ids: Vec<String>,
    pub has_token: bool,
    pub has_custom_ca: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct IssueConnectionsPublic {
    pub revision: u64,
    pub connections: Vec<IssueConnectionPublic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct IssueConnectionChoice {
    pub platform: IssuePlatform,
    pub connection_ref: String,
    pub name: String,
    pub instance_url: String,
}

/// Omitted secrets preserve the credential of the same configured connection.
#[derive(Clone, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IssueConnectionUpdate {
    pub platform: IssuePlatform,
    pub connection_ref: String,
    pub name: String,
    pub enabled: bool,
    pub instance_url: String,
    pub mcp_url: String,
    pub allowed_actor_ids: Vec<String>,
    pub token: Option<String>,
    pub extra_ca_pem: Option<String>,
}

#[derive(Clone, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IssueConnectionsUpdate {
    pub expected_revision: u64,
    pub connections: Vec<IssueConnectionUpdate>,
}

impl IssueConnectionConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_id(&self.connection_ref) || self.name.trim().is_empty() || self.name.len() > 256 {
            return Err("Invalid external connection identity or name");
        }
        for endpoint in [&self.instance_url, &self.mcp_url] {
            if endpoint.len() > 2048 || endpoint_url(endpoint).is_err() {
                return Err(
                    "External connections require a configured HTTPS URL without URL credentials",
                );
            }
        }
        let mut actors = std::collections::BTreeSet::new();
        if self.allowed_actor_ids.len() > 256
            || self
                .allowed_actor_ids
                .iter()
                .any(|actor| !valid_id(actor) || !actors.insert(actor))
        {
            return Err("Invalid or duplicate external connection accounts");
        }
        if self.token.as_ref().is_some_and(|token| {
            token.is_empty()
                || token.len() > 8192
                || !token.bytes().all(|byte| byte.is_ascii_graphic())
        }) {
            return Err("Invalid external connection credential");
        }
        if let Some(pem) = &self.extra_ca_pem {
            use rustls::pki_types::{CertificateDer, pem::PemObject};
            if pem.is_empty() || pem.len() > 128 * 1024 {
                return Err("Invalid external connection CA certificate");
            }
            let mut certificates = CertificateDer::pem_slice_iter(pem.as_bytes()).peekable();
            if certificates.peek().is_none() || certificates.any(|certificate| certificate.is_err())
            {
                return Err("Invalid external connection CA certificate");
            }
        }
        if self.enabled && (self.token.is_none() || self.allowed_actor_ids.is_empty()) {
            return Err(
                "An enabled external connection requires a credential and an allowed account",
            );
        }
        Ok(())
    }

    pub fn permits(&self, actor: &str) -> bool {
        self.enabled
            && self
                .allowed_actor_ids
                .iter()
                .any(|allowed| allowed == actor)
    }

    pub async fn resolve_subscription(
        &self,
        owner: &str,
        repository: &str,
        issue_number: Option<u64>,
    ) -> Result<
        (
            desk_diagnose_core::issue_authority::IssueRepositoryScope,
            Option<desk_diagnose_core::issue_authority::IssueFilter>,
        ),
        McpClientError,
    > {
        let config = self.clone();
        let client = tokio::task::spawn_blocking(move || config.client())
            .await
            .map_err(|_| McpClientError::Unavailable)??;
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            client.bind_subscription(&self.connection_ref, owner, repository, issue_number),
        )
        .await
        .map_err(|_| McpClientError::Unavailable)?
    }

    pub async fn subscription_accounts(
        &self,
        login: Option<&str>,
    ) -> Result<IssueSubscriptionAccounts, McpClientError> {
        let config = self.clone();
        let client = tokio::task::spawn_blocking(move || config.client())
            .await
            .map_err(|_| McpClientError::Unavailable)??;
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            client.subscription_accounts(login),
        )
        .await
        .map_err(|_| McpClientError::Unavailable)?
    }

    /// Creating the HTTP client loads OS roots. Hosts run this at a blocking I/O boundary.
    pub fn client(&self) -> Result<IssueConnection, McpClientError> {
        self.validate()
            .map_err(|_| McpClientError::InvalidConfiguration)?;
        IssueConnection::new(
            self.platform,
            &self.instance_url,
            &self.mcp_url,
            self.token
                .as_deref()
                .ok_or(McpClientError::InvalidConfiguration)?,
            self.extra_ca_pem.as_deref().map(str::as_bytes),
        )
    }

    fn public(&self) -> IssueConnectionPublic {
        IssueConnectionPublic {
            platform: self.platform,
            connection_ref: self.connection_ref.clone(),
            name: self.name.clone(),
            enabled: self.enabled,
            instance_url: self.instance_url.clone(),
            mcp_url: self.mcp_url.clone(),
            allowed_actor_ids: self.allowed_actor_ids.clone(),
            has_token: self.token.is_some(),
            has_custom_ca: self.extra_ca_pem.is_some(),
        }
    }
}

impl IssueConnectionsConfig {
    pub fn connection_for_actor(
        &self,
        actor: &str,
        reference: &str,
    ) -> Option<&IssueConnectionConfig> {
        self.connections
            .iter()
            .find(|connection| connection.connection_ref == reference && connection.permits(actor))
    }

    pub fn connection_for_repository(
        &self,
        actor: &str,
        repository: &desk_diagnose_core::issue_authority::IssueRepositoryScope,
    ) -> Option<&IssueConnectionConfig> {
        self.connections.iter().find(|connection| {
            connection.connection_ref == repository.connection_ref
                && connection.platform == repository.platform
                && connection.permits(actor)
                && super::endpoint_url(&connection.instance_url)
                    .is_ok_and(|url| url.as_str() == repository.instance)
        })
    }

    pub fn connection_for_issue(
        &self,
        actor: &str,
        issue: &desk_diagnose_core::external_tools::IssueAccess,
    ) -> Option<&IssueConnectionConfig> {
        self.connections.iter().find(|connection| {
            connection.connection_ref == issue.connection_ref
                && connection.platform == issue.target.platform
                && connection.permits(actor)
                && super::endpoint_url(&connection.instance_url)
                    .is_ok_and(|url| url.as_str() == issue.target.instance)
        })
    }

    /// Resolve only the locators selected for this input; no connection-wide
    /// permission is inherited by a model or a delegated request.
    pub async fn bind_issues(
        &self,
        actor: &str,
        selected: &[desk_agent_protocol::external_tools::IssueSelection],
    ) -> Result<Vec<desk_diagnose_core::external_tools::IssueAccess>, McpClientError> {
        desk_agent_protocol::external_tools::validate_selections(selected)
            .map_err(|_| McpClientError::InvalidConfiguration)?;
        let mut resolved = Vec::with_capacity(selected.len());
        for issue in selected {
            let connection = self
                .connections
                .iter()
                .find(|connection| {
                    connection.connection_ref == issue.connection_ref && connection.permits(actor)
                })
                .cloned()
                .ok_or(McpClientError::InvalidConfiguration)?;
            let client = tokio::task::spawn_blocking(move || connection.client())
                .await
                .map_err(|_| McpClientError::Unavailable)??;
            let target = client
                .bind_issue(&issue.owner, &issue.repository, issue.issue_number)
                .await?;
            resolved.push(desk_diagnose_core::external_tools::IssueAccess {
                connection_ref: issue.connection_ref.clone(),
                target,
            });
        }
        Ok(resolved)
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        let mut identities = std::collections::BTreeSet::new();
        if self.connections.len() > 64 {
            return Err("External connection directory exceeds its configured capacity");
        }
        for connection in &self.connections {
            connection.validate()?;
            if !identities.insert(&connection.connection_ref) {
                return Err("Duplicate external connection identity");
            }
        }
        Ok(())
    }

    pub fn parse(raw: &str) -> Result<Self, &'static str> {
        let config: Self =
            serde_json::from_str(raw).map_err(|_| "Invalid external connection settings")?;
        config.validate()?;
        Ok(config)
    }

    pub fn public(&self) -> IssueConnectionsPublic {
        IssueConnectionsPublic {
            revision: self.revision,
            connections: self
                .connections
                .iter()
                .map(IssueConnectionConfig::public)
                .collect(),
        }
    }

    pub fn choices(&self, actor: &str) -> Vec<IssueConnectionChoice> {
        self.connections
            .iter()
            .filter(|connection| connection.permits(actor))
            .map(|connection| IssueConnectionChoice {
                platform: connection.platform,
                connection_ref: connection.connection_ref.clone(),
                name: connection.name.clone(),
                instance_url: connection.instance_url.clone(),
            })
            .collect()
    }

    pub fn candidate(&self, update: &IssueConnectionsUpdate) -> Result<Self, &'static str> {
        if self.revision != update.expected_revision {
            return Err("External connection settings changed");
        }
        let mut connections = Vec::with_capacity(update.connections.len());
        for proposed in &update.connections {
            let instance_url = endpoint_url(&proposed.instance_url)
                .map_err(|_| "Invalid issue instance URL")?
                .to_string();
            let mcp_url = endpoint_url(&proposed.mcp_url)
                .map_err(|_| "Invalid MCP endpoint URL")?
                .to_string();
            let original = self
                .connections
                .iter()
                .find(|existing| existing.connection_ref == proposed.connection_ref);
            // Endpoint changes require an explicitly supplied credential.
            let same_endpoint = original.filter(|existing| {
                existing.platform == proposed.platform
                    && endpoint_url(&existing.instance_url)
                        .is_ok_and(|value| value.as_str() == instance_url)
                    && endpoint_url(&existing.mcp_url).is_ok_and(|value| value.as_str() == mcp_url)
            });
            let secret = |incoming: &Option<String>, previous: Option<&String>| match incoming {
                Some(value) => (!value.is_empty()).then(|| value.clone()),
                None => previous.cloned(),
            };
            connections.push(IssueConnectionConfig {
                platform: proposed.platform,
                connection_ref: proposed.connection_ref.clone(),
                name: proposed.name.clone(),
                enabled: proposed.enabled,
                instance_url,
                mcp_url,
                allowed_actor_ids: proposed.allowed_actor_ids.clone(),
                token: secret(
                    &proposed.token,
                    same_endpoint.and_then(|existing| existing.token.as_ref()),
                ),
                extra_ca_pem: secret(
                    &proposed.extra_ca_pem,
                    same_endpoint.and_then(|existing| existing.extra_ca_pem.as_ref()),
                ),
            });
        }
        let candidate = Self {
            revision: self
                .revision
                .checked_add(1)
                .ok_or("External connection revision overflow")?,
            connections,
        };
        candidate.validate()?;
        Ok(candidate)
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_settings_hide_secrets_and_endpoint_changes_require_a_new_credential() {
        let mut update = IssueConnectionsUpdate {
            expected_revision: 0,
            connections: vec![IssueConnectionUpdate {
                platform: desk_agent_protocol::external_tools::IssuePlatform::Gitea,
                connection_ref: "forge".into(),
                name: "Team Gitea".into(),
                enabled: true,
                instance_url: "https://forge.invalid".into(),
                mcp_url: "https://mcp.invalid/mcp".into(),
                allowed_actor_ids: vec!["7".into()],
                token: Some("test-secret".into()),
                extra_ca_pem: None,
            }],
        };
        let first = IssueConnectionsConfig::default()
            .candidate(&update)
            .unwrap();
        assert!(first.connections[0].permits("7"));
        assert!(!first.connections[0].permits("8"));
        assert!(
            !serde_json::to_string(&first.public())
                .unwrap()
                .contains("test-secret")
        );
        assert!(!format!("{first:?}").contains("test-secret"));
        assert_eq!(
            IssueConnectionsConfig::parse(&serde_json::to_string(&first).unwrap()).unwrap(),
            first
        );
        update.expected_revision = first.revision;
        update.connections[0].token = None;
        assert_eq!(
            first.candidate(&update).unwrap().connections[0].token,
            first.connections[0].token
        );
        update.connections[0].mcp_url = "https://other.invalid/mcp".into();
        assert!(first.candidate(&update).is_err());
        update.connections[0].mcp_url = "https://mcp.invalid/mcp".into();
        update.connections[0].platform = IssuePlatform::Github;
        assert!(first.candidate(&update).is_err());
        update.connections[0].platform = IssuePlatform::Gitea;
        update.connections[0].token = Some("replacement".into());
        assert!(first.candidate(&update).is_ok());
        update.connections[0].allowed_actor_ids.push("7".into());
        assert!(first.candidate(&update).is_err());
    }
}
