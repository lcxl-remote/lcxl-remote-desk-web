//! File-owned OSS settings and the snapshot shared by runtime transactions.

pub mod connection;
mod file_codec;
#[cfg(test)]
pub mod test_support;
#[cfg(test)]
mod tests;

use async_trait::async_trait;
use desk_agent_protocol::{
    ai_assistant::{goal_budget::GoalBudgetPolicy, subagent_policy::SubAgentPolicy},
    schedule::policy::ScheduleBudgetPolicy,
};
use desk_diagnose_core::model_context::PlatformContextPolicy;
use desk_signal_facade::{model::model_metrics::MetricsSettings, web_search::SearchConfig};
use sea_orm::DbErr;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::{OwnedRwLockReadGuard, RwLock};

use crate::{
    approval_model_provider::ApprovalModelConfig, model_provider::ModelProviderConfig,
    usage_retention::UsageRetentionConfig,
};

pub const SECTION_NAMES: [&str; 11] = [
    "ai_gateway",
    "approval_gateway",
    "web_search",
    "context_management",
    "terminal_completion",
    "subagent_policy",
    "goal_budget_policy",
    "schedule_budget_policy",
    "usage_retention",
    "model_metrics",
    "oss_config_metadata",
];

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Metadata {
    pub instance: String,
    pub fingerprints: BTreeMap<String, String>,
    pub revisions: BTreeMap<String, String>,
}

#[derive(Clone)]
pub struct GlobalConfig {
    pub ai_gateway: ModelProviderConfig,
    pub approval_gateway: ApprovalModelConfig,
    pub web_search: SearchConfig,
    pub context_management: PlatformContextPolicy,
    pub terminal_completion:
        desk_diagnose_core::terminal_completion_policy::TerminalCompletionPolicy,
    pub subagent_policy: SubAgentPolicy,
    pub goal_budget_policy: GoalBudgetPolicy,
    pub schedule_budget_policy: ScheduleBudgetPolicy,
    pub usage_retention: UsageRetentionConfig,
    pub model_metrics: MetricsSettings,
    pub metadata: Metadata,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            ai_gateway: ModelProviderConfig::default(),
            approval_gateway: ApprovalModelConfig::default(),
            web_search: SearchConfig::default(),
            context_management: PlatformContextPolicy::default(),
            terminal_completion: Default::default(),
            subagent_policy: desk_diagnose_core::subagent::policy::initial(),
            goal_budget_policy: desk_diagnose_core::goal_budget::initial(),
            schedule_budget_policy: desk_diagnose_core::schedule::policy::initial(),
            usage_retention: UsageRetentionConfig::default(),
            model_metrics: MetricsSettings::defaults(false),
            metadata: Metadata::default(),
        }
    }
}

impl std::fmt::Debug for GlobalConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GlobalConfig").finish_non_exhaustive()
    }
}

impl GlobalConfig {
    pub fn validate(&self) -> Result<(), DbErr> {
        file_codec::validate_gateway(&self.ai_gateway)?;
        file_codec::validate_gateway(&self.approval_gateway.gateway)?;
        if self.approval_gateway.configuration_revision < 1 {
            return Err(invalid("approval_gateway"));
        }
        self.web_search
            .validate()
            .map_err(|_| invalid("web_search"))?;
        self.context_management
            .validate()
            .map_err(|_| invalid("context_management"))?;
        self.terminal_completion
            .validate()
            .map_err(|_| invalid("terminal_completion"))?;
        desk_diagnose_core::subagent::policy::validate(&self.subagent_policy)
            .map_err(|_| invalid("subagent_policy"))?;
        desk_diagnose_core::goal_budget::validate(&self.goal_budget_policy)
            .map_err(|_| invalid("goal_budget_policy"))?;
        desk_diagnose_core::schedule::policy::validate(&self.schedule_budget_policy)
            .map_err(|_| invalid("schedule_budget_policy"))?;
        self.usage_retention
            .validate()
            .map_err(|_| invalid("usage_retention"))?;
        self.model_metrics
            .validate()
            .map_err(|_| invalid("model_metrics"))?;
        Ok(())
    }

    /// Reconcile stopped-service file edits before consumers start.
    pub fn initialize_metadata(&mut self) -> Result<bool, DbErr> {
        self.validate()?;
        let before =
            serde_json::to_vec(&self.metadata).map_err(|_| invalid("oss_config_metadata"))?;
        if self.metadata.instance.is_empty() {
            self.metadata = Metadata {
                instance: uuid::Uuid::new_v4().to_string(),
                ..Metadata::default()
            };
        } else if uuid::Uuid::parse_str(&self.metadata.instance).is_err() {
            return Err(invalid("oss_config_metadata.instance"));
        }
        self.reconcile_fingerprints(true)?;
        Ok(before
            != serde_json::to_vec(&self.metadata).map_err(|_| invalid("oss_config_metadata"))?)
    }

    pub fn reconcile_fingerprints(&mut self, manual_edit: bool) -> Result<(), DbErr> {
        // Preserve the gateway's existing prompt-cache reset rules for file edits.
        let previous_values = file_codec::business_values(self)?;
        for (name, gateway) in [
            ("ai_gateway", &mut self.ai_gateway),
            ("approval_gateway", &mut self.approval_gateway.gateway),
        ] {
            let connection_key = format!("{name}.connection");
            let connection_changed =
                self.metadata
                    .fingerprints
                    .get(&connection_key)
                    .is_some_and(|previous| {
                        *previous != fingerprint(previous_values[&connection_key].clone())
                    });
            let history_key = format!("{name}.history_binding");
            let history = serde_json::json!({"model": gateway.model, "options": desk_diagnose_core::prompt_cache::without_cache(&gateway.request_options)});
            if manual_edit {
                if connection_changed {
                    desk_diagnose_core::prompt_cache::reset_connection(
                        &mut gateway.request_options,
                    );
                } else if self
                    .metadata
                    .fingerprints
                    .get(&history_key)
                    .is_some_and(|previous| *previous != fingerprint(history.clone()))
                {
                    desk_diagnose_core::prompt_cache::reset_history(&mut gateway.request_options);
                }
            }
            self.metadata
                .fingerprints
                .insert(history_key, fingerprint(history));
        }
        let values = file_codec::business_values(self)?;
        for (key, value) in values {
            let fingerprint = fingerprint(value);
            if manual_edit
                && self
                    .metadata
                    .fingerprints
                    .get(&key)
                    .is_some_and(|old| old != &fingerprint)
            {
                let previous = self
                    .metadata
                    .revisions
                    .get(&key)
                    .cloned()
                    .ok_or_else(|| invalid("oss_config_metadata.revisions"))?;
                file_codec::advance_revision(self, &key, &previous)?;
            }
            self.metadata
                .revisions
                .insert(key.clone(), file_codec::revision(self, &key));
            self.metadata.fingerprints.insert(key, fingerprint);
        }
        Ok(())
    }
}

fn fingerprint(mut value: serde_json::Value) -> String {
    canonicalize(&mut value);
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&value).expect("JSON value serialization"))
    )
}

fn canonicalize(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.sort_keys();
            for child in map.values_mut() {
                canonicalize(child);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                canonicalize(child);
            }
        }
        _ => {}
    }
}

pub(crate) fn invalid(field: &str) -> DbErr {
    DbErr::Custom(format!("invalid OSS configuration field: {field}"))
}

#[async_trait]
pub trait ConfigPersistence: Send + Sync {
    /// Save the complete document and publish host settings before returning.
    async fn persist(&self, candidate: &GlobalConfig) -> Result<(), DbErr>;
}

pub type ConfigRead = Arc<OwnedRwLockReadGuard<GlobalConfig>>;

pub struct ConfigContext {
    state: Arc<RwLock<GlobalConfig>>,
    persistence: Arc<dyn ConfigPersistence>,
}

impl std::fmt::Debug for ConfigContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigContext").finish_non_exhaustive()
    }
}

impl ConfigContext {
    pub fn new(
        initial: GlobalConfig,
        persistence: Arc<dyn ConfigPersistence>,
    ) -> Result<Arc<Self>, DbErr> {
        initial.validate()?;
        if initial.metadata.instance.is_empty() {
            return Err(invalid("oss_config_metadata.instance"));
        }
        Ok(Arc::new(Self {
            state: Arc::new(RwLock::new(initial)),
            persistence,
        }))
    }

    pub async fn read(&self) -> ConfigRead {
        Arc::new(self.state.clone().read_owned().await)
    }

    pub async fn instance(&self) -> String {
        self.state.read().await.metadata.instance.clone()
    }

    pub async fn update<R, E, F>(&self, change: F) -> Result<Option<R>, E>
    where
        E: From<DbErr>,
        F: FnOnce(&mut GlobalConfig) -> Result<Option<R>, E>,
    {
        let mut live = self.state.write().await;
        let mut candidate = live.clone();
        let Some(result) = change(&mut candidate)? else {
            return Ok(None);
        };
        candidate.validate().map_err(E::from)?;
        candidate.reconcile_fingerprints(false).map_err(E::from)?;
        self.persistence
            .persist(&candidate)
            .await
            .map_err(E::from)?;
        *live = candidate;
        Ok(Some(result))
    }
}

#[async_trait]
pub trait ConfigConnection: sea_orm::ConnectionTrait {
    fn config_context(&self) -> &Arc<ConfigContext>;
    async fn config_read(&self) -> ConfigRead;
}

pub async fn save_metrics(
    context: &ConfigContext,
    mut requested: MetricsSettings,
) -> Result<Option<MetricsSettings>, DbErr> {
    requested.validate().map_err(|_| invalid("model_metrics"))?;
    let expected = requested.revision.clone();
    requested.revision = expected
        .parse::<i64>()
        .map_err(|_| invalid("model_metrics.revision"))?
        .checked_add(1)
        .ok_or_else(|| invalid("model_metrics.revision"))?
        .to_string();
    context
        .update::<_, DbErr, _>(|config| {
            if config.model_metrics.revision != expected {
                return Ok(None);
            }
            config.model_metrics = requested.clone();
            Ok(Some(requested))
        })
        .await
}
