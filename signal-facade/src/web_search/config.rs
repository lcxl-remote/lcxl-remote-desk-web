//! Revisioned central search configuration. Secrets never enter public DTOs.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SearchProvider {
    #[default]
    DuckDuckGo,
    Brave,
    Tavily,
}

impl SearchProvider {
    pub const ALL: [Self; 3] = [Self::DuckDuckGo, Self::Brave, Self::Tavily];

    pub const fn connector_id(self) -> &'static str {
        match self {
            Self::DuckDuckGo => "duckduckgo_html_v1",
            Self::Brave => "brave_web_v1",
            Self::Tavily => "tavily_search_v1",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::DuckDuckGo => "DuckDuckGo",
            Self::Brave => "Brave Web Search",
            Self::Tavily => "Tavily Search",
        }
    }

    pub const fn requires_api_key(self) -> bool {
        !matches!(self, Self::DuckDuckGo)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchConfig {
    pub schema_version: u32,
    pub revision: u64,
    pub provider: SearchProvider,
    api_key: Option<String>,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            schema_version: 1,
            revision: 0,
            provider: SearchProvider::DuckDuckGo,
            api_key: None,
        }
    }
}

impl std::fmt::Debug for SearchConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchConfig")
            .field("revision", &self.revision)
            .field("provider", &self.provider)
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SearchConfigPublic {
    pub revision: u64,
    pub provider: SearchProvider,
    pub has_api_key: bool,
    pub configured: bool,
    pub providers: Vec<SearchProviderInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SearchProviderInfo {
    pub provider: SearchProvider,
    pub display_name: String,
    pub requires_api_key: bool,
}

/// Null/omitted keeps a key only for the same provider; empty explicitly clears.
#[derive(Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchConfigUpdate {
    pub expected_revision: u64,
    pub provider: SearchProvider,
    pub api_key: Option<String>,
}

impl SearchConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != 1 {
            return Err("unsupported Web Search configuration schema");
        }
        if self.provider == SearchProvider::DuckDuckGo && self.api_key.is_some() {
            return Err("DuckDuckGo does not accept an API key");
        }
        if let Some(key) = &self.api_key {
            if key.is_empty()
                || key.len() > 4096
                || key.trim() != key
                || key.chars().any(char::is_control)
            {
                return Err("Web Search API key is invalid");
            }
        }
        Ok(())
    }

    pub fn parse(raw: &str) -> Result<Self, &'static str> {
        // Never include a serde error: it can contain the original secret.
        let config: Self =
            serde_json::from_str(raw).map_err(|_| "invalid Web Search configuration")?;
        config.validate()?;
        Ok(config)
    }

    pub fn configured(&self) -> bool {
        self.validate().is_ok() && (!self.provider.requires_api_key() || self.api_key.is_some())
    }

    pub fn public(&self) -> SearchConfigPublic {
        SearchConfigPublic {
            revision: self.revision,
            provider: self.provider,
            has_api_key: self.api_key.is_some(),
            configured: self.configured(),
            providers: SearchProvider::ALL
                .into_iter()
                .map(|provider| SearchProviderInfo {
                    provider,
                    display_name: provider.display_name().into(),
                    requires_api_key: provider.requires_api_key(),
                })
                .collect(),
        }
    }

    pub fn candidate(&self, update: &SearchConfigUpdate) -> Result<Self, &'static str> {
        self.validate()?;
        if update.expected_revision != self.revision {
            return Err("Web Search configuration revision changed");
        }
        if update
            .api_key
            .as_ref()
            .is_some_and(|key| key.len() > 4096 || key.chars().any(char::is_control))
        {
            return Err("Web Search API key is invalid");
        }
        let api_key = match &update.api_key {
            Some(value) => (!value.trim().is_empty()).then(|| value.trim().to_owned()),
            None if update.provider == self.provider => self.api_key.clone(),
            None => None,
        };
        let candidate = Self {
            schema_version: 1,
            revision: self
                .revision
                .checked_add(1)
                .ok_or("Web Search revision overflow")?,
            provider: update.provider,
            api_key,
        };
        candidate.validate()?;
        Ok(candidate)
    }

    pub fn readiness_identity(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(self.provider.connector_id());
        hash.update(self.revision.to_le_bytes());
        hash.update(self.api_key.as_deref().unwrap_or_default());
        format!("{}:{:x}", self.provider.connector_id(), hash.finalize())
    }

    pub fn binding(&self) -> Option<desk_diagnose_core::web_research::SearchBinding> {
        self.configured()
            .then(|| desk_diagnose_core::web_research::SearchBinding {
                connector_id: self.provider.connector_id().into(),
                revision: self.revision,
            })
    }

    pub(super) fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(
        config: &SearchConfig,
        provider: SearchProvider,
        key: Option<&str>,
    ) -> SearchConfigUpdate {
        SearchConfigUpdate {
            expected_revision: config.revision,
            provider,
            api_key: key.map(str::to_owned),
        }
    }

    #[test]
    fn default_is_keyless_and_public_projection_never_contains_secret() {
        let initial = SearchConfig::default();
        assert!(initial.configured());
        assert!(!initial.public().has_api_key);
        let config = initial
            .candidate(&update(&initial, SearchProvider::Brave, Some(" secret ")))
            .unwrap();
        assert!(config.configured());
        assert!(!format!("{config:?}").contains("secret"));
        assert!(
            !serde_json::to_string(&config.public())
                .unwrap()
                .contains("secret")
        );
        assert_eq!(
            SearchConfig::parse(&serde_json::to_string(&config).unwrap()).unwrap(),
            config
        );
    }

    #[test]
    fn provider_switch_never_reuses_a_foreign_key_and_missing_key_never_falls_back() {
        let initial = SearchConfig::default();
        let brave = initial
            .candidate(&update(&initial, SearchProvider::Brave, Some("secret")))
            .unwrap();
        let kept = brave
            .candidate(&update(&brave, SearchProvider::Brave, None))
            .unwrap();
        assert_eq!(kept.api_key(), Some("secret"));
        let tavily = kept
            .candidate(&update(&kept, SearchProvider::Tavily, None))
            .unwrap();
        assert!(!tavily.configured());
        assert_eq!(tavily.provider, SearchProvider::Tavily);
        let ddg = brave
            .candidate(&update(&brave, SearchProvider::DuckDuckGo, None))
            .unwrap();
        assert!(ddg.configured());
        assert_eq!(ddg.api_key(), None);
        assert!(
            brave
                .candidate(&update(&brave, SearchProvider::DuckDuckGo, Some("secret")))
                .is_err()
        );
        assert_ne!(kept.readiness_identity(), brave.readiness_identity());
        assert!(
            !brave
                .candidate(&update(&brave, SearchProvider::Brave, Some("")))
                .unwrap()
                .configured()
        );
    }

    #[test]
    fn invalid_secret_stale_revision_and_corrupt_config_fail_closed() {
        let config = SearchConfig::default();
        for key in ["bad\nkey".to_owned(), "x".repeat(4097)] {
            assert!(
                config
                    .candidate(&update(&config, SearchProvider::Brave, Some(&key)))
                    .is_err()
            );
        }
        let mut stale = update(&config, SearchProvider::Brave, None);
        stale.expected_revision = 5;
        assert!(config.candidate(&stale).is_err());
        for raw in [
            r#"{"provider":"unknown"}"#,
            r#"{"schema_version":2,"revision":0,"provider":"duck_duck_go","api_key":null}"#,
            "secret",
        ] {
            assert!(SearchConfig::parse(raw).is_err());
        }
        let max = SearchConfig {
            revision: u64::MAX,
            ..config
        };
        assert!(
            max.candidate(&update(&max, SearchProvider::DuckDuckGo, None))
                .is_err()
        );
    }
}
