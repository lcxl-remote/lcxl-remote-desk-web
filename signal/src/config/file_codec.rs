//! Current document encoding: decimal revisions, explicit disabled limits and JSON options.

use super::{GlobalConfig, Metadata, SECTION_NAMES, invalid};
use sea_orm::DbErr;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::DeserializeOwned};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

fn value<T: Serialize>(source: &T) -> Result<Value, DbErr> {
    serde_json::to_value(source).map_err(|_| invalid("encoding"))
}

fn raw(config: &GlobalConfig) -> Result<Map<String, Value>, DbErr> {
    Ok(Map::from_iter([
        ("ai_gateway".into(), value(&config.ai_gateway)?),
        ("approval_gateway".into(), value(&config.approval_gateway)?),
        ("web_search".into(), value(&config.web_search)?),
        (
            "context_management".into(),
            value(&config.context_management)?,
        ),
        ("subagent_policy".into(), value(&config.subagent_policy)?),
        (
            "terminal_completion".into(),
            value(&config.terminal_completion)?,
        ),
        (
            "goal_budget_policy".into(),
            value(&config.goal_budget_policy)?,
        ),
        (
            "schedule_budget_policy".into(),
            value(&config.schedule_budget_policy)?,
        ),
        ("usage_retention".into(), value(&config.usage_retention)?),
        ("model_metrics".into(), value(&config.model_metrics)?),
        ("oss_config_metadata".into(), value(&config.metadata)?),
    ]))
}

fn encode_fields(value: &mut Value, path: &str, keep_null: bool) {
    let Value::Object(map) = value else {
        return;
    };
    if let Some(options) = map.remove("request_options") {
        map.insert(
            "request_options_json".into(),
            Value::String(options.to_string()),
        );
    }
    for (key, child) in map.iter_mut() {
        if key == "revision" || key.ends_with("_revision") {
            if child.is_number() {
                *child = Value::String(child.to_string());
            }
        } else if path.ends_with("goal_budget_policy.limits") && child.is_null() {
            *child = Value::String("disabled".into());
        } else {
            encode_fields(child, &format!("{path}.{key}"), keep_null);
        }
    }
    if !keep_null {
        map.retain(|_, value| !value.is_null());
    }
}

fn decode_fields(value: &mut Value, path: &str) -> Result<(), DbErr> {
    let Value::Object(map) = value else {
        return Ok(());
    };
    if let Some(options) = map.remove("request_options_json") {
        let options: Value = serde_json::from_str(options.as_str().ok_or_else(|| invalid(path))?)
            .map_err(|_| invalid(&format!("{path}.request_options_json")))?;
        if !options.is_object() {
            return Err(invalid(&format!("{path}.request_options_json")));
        }
        map.insert("request_options".into(), options);
    }
    for (key, child) in map.iter_mut() {
        let field = format!("{path}.{key}");
        if (key == "revision" || key.ends_with("_revision")) && path != "model_metrics" {
            let text = child.as_str().ok_or_else(|| invalid(&field))?;
            let number = text.parse::<u64>().map_err(|_| invalid(&field))?;
            if text != number.to_string() {
                return Err(invalid(&field));
            }
            *child = Value::Number(number.into());
        } else if path.ends_with("goal_budget_policy.limits") && child.as_str() == Some("disabled")
        {
            *child = Value::Null;
        } else if key != "request_options" && path != "oss_config_metadata" {
            decode_fields(child, &field)?;
        }
    }
    Ok(())
}

fn merge(defaults: &mut Value, provided: Value, path: &str) -> Result<(), DbErr> {
    if let (Value::Object(expected), Value::Object(provided)) = (&mut *defaults, &provided) {
        for (key, incoming) in provided {
            let field = format!("{path}.{key}");
            if path == "oss_config_metadata.fingerprints" || path == "oss_config_metadata.revisions"
            {
                expected.insert(key.clone(), incoming.clone());
                continue;
            }
            let target = expected.get_mut(key).ok_or_else(|| invalid(&field))?;
            merge(target, incoming.clone(), &field)?;
        }
    } else {
        *defaults = provided;
    }
    Ok(())
}

fn decode<T: DeserializeOwned>(mut encoded: Value, path: &str) -> Result<T, DbErr> {
    decode_fields(&mut encoded, path)?;
    serde_json::from_value(encoded).map_err(|_| invalid(path))
}

impl Serialize for GlobalConfig {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sections = raw(self).map_err(serde::ser::Error::custom)?;
        for (path, section) in &mut sections {
            if path != "oss_config_metadata" {
                encode_fields(section, path, false);
            }
        }
        sections.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for GlobalConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let provided = Map::<String, Value>::deserialize(deserializer)?;
        let mut defaults = raw(&GlobalConfig::default()).map_err(serde::de::Error::custom)?;
        // Keep null defaults in the merge skeleton, so optional fields remain known.
        for (path, section) in &mut defaults {
            if path == "oss_config_metadata" {
                continue;
            }
            encode_fields(section, path, true);
        }
        for name in SECTION_NAMES {
            if let Some(section) = provided.get(name) {
                merge(
                    defaults.get_mut(name).expect("known section"),
                    section.clone(),
                    name,
                )
                .map_err(serde::de::Error::custom)?;
            }
        }
        let mut take = |name: &str| defaults.remove(name).expect("known section");
        let config = Self {
            ai_gateway: decode(take("ai_gateway"), "ai_gateway")
                .map_err(serde::de::Error::custom)?,
            approval_gateway: decode(take("approval_gateway"), "approval_gateway")
                .map_err(serde::de::Error::custom)?,
            web_search: decode(take("web_search"), "web_search")
                .map_err(serde::de::Error::custom)?,
            context_management: decode(take("context_management"), "context_management")
                .map_err(serde::de::Error::custom)?,
            terminal_completion: decode(take("terminal_completion"), "terminal_completion")
                .map_err(serde::de::Error::custom)?,
            subagent_policy: decode(take("subagent_policy"), "subagent_policy")
                .map_err(serde::de::Error::custom)?,
            goal_budget_policy: decode(take("goal_budget_policy"), "goal_budget_policy")
                .map_err(serde::de::Error::custom)?,
            schedule_budget_policy: decode(
                take("schedule_budget_policy"),
                "schedule_budget_policy",
            )
            .map_err(serde::de::Error::custom)?,
            usage_retention: decode(take("usage_retention"), "usage_retention")
                .map_err(serde::de::Error::custom)?,
            model_metrics: decode(take("model_metrics"), "model_metrics")
                .map_err(serde::de::Error::custom)?,
            metadata: serde_json::from_value::<Metadata>(take("oss_config_metadata"))
                .map_err(|_| serde::de::Error::custom("invalid OSS configuration metadata"))?,
        };
        config.validate().map_err(serde::de::Error::custom)?;
        Ok(config)
    }
}

pub(super) fn validate_gateway(
    config: &crate::model_provider::ModelProviderConfig,
) -> Result<(), DbErr> {
    use crate::model_provider::*;
    if config.connection_revision < 1
        || config.profile_revision < 1
        || !config.request_options.is_object()
        || !(MAX_STEPS_MIN..=MAX_STEPS_MAX).contains(&config.max_steps_per_turn)
        || !(MAX_SAME_TOOL_CALLS_MIN..=MAX_SAME_TOOL_CALLS_MAX)
            .contains(&config.max_same_tool_calls_per_turn)
        || !step_budget_covers_same_tool_limit(
            config.max_steps_per_turn,
            config.max_same_tool_calls_per_turn,
        )
        || !(EXEC_APPROVAL_TIMEOUT_MIN_SECS..=EXEC_APPROVAL_TIMEOUT_MAX_SECS)
            .contains(&config.exec_approval_timeout_secs)
    {
        return Err(invalid("model_gateway"));
    }
    let profile = desk_diagnose_core::model_profile::ModelRequestProfile {
        reasoning_contract: config.reasoning_contract,
        anthropic_prefix_binding: config.anthropic_prefix_binding,
        profile_schema_version: config.profile_schema_version,
        request_options: config.request_options.clone(),
        output_limit_field: config.output_limit_field,
        runtime_max_output_tokens: config.runtime_max_output_tokens,
        max_context_bytes: config.max_context_bytes.unwrap_or(131_072),
        profile_revision: config.profile_revision,
    };
    profile
        .validate(
            config
                .wire_protocol
                .unwrap_or(desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions),
        )
        .map_err(|_| invalid("model_gateway.profile"))
}

pub(super) fn business_values(config: &GlobalConfig) -> Result<BTreeMap<String, Value>, DbErr> {
    let mut values = BTreeMap::new();
    for (name, gateway) in [
        ("ai_gateway", &config.ai_gateway),
        ("approval_gateway", &config.approval_gateway.gateway),
    ] {
        let raw = value(gateway)?;
        let select = |fields: &[&str]| {
            Value::Object(
                fields
                    .iter()
                    .map(|key| ((*key).into(), raw[*key].clone()))
                    .collect(),
            )
        };
        values.insert(
            format!("{name}.connection"),
            select(&["wire_protocol", "base_url", "api_key"]),
        );
        values.insert(
            format!("{name}.profile"),
            select(&[
                "model",
                "supports_image_input",
                "profile_schema_version",
                "reasoning_contract",
                "anthropic_prefix_binding",
                "request_options",
                "output_limit_field",
                "runtime_max_output_tokens",
                "max_context_bytes",
            ]),
        );
    }
    values.insert("approval_gateway.configuration".into(), serde_json::json!({"enabled": config.approval_gateway.enabled, "connection": values["approval_gateway.connection"], "profile": values["approval_gateway.profile"]}));
    for (name, mut section) in raw(config)? {
        if matches!(
            name.as_str(),
            "ai_gateway" | "approval_gateway" | "oss_config_metadata" | "usage_retention"
        ) {
            continue;
        }
        section
            .as_object_mut()
            .ok_or_else(|| invalid(&name))?
            .remove("revision");
        values.insert(name, section);
    }
    Ok(values)
}

pub(super) fn revision(config: &GlobalConfig, key: &str) -> String {
    match key {
        "ai_gateway.connection" => config.ai_gateway.connection_revision.to_string(),
        "ai_gateway.profile" => config.ai_gateway.profile_revision.to_string(),
        "approval_gateway.connection" => config
            .approval_gateway
            .gateway
            .connection_revision
            .to_string(),
        "approval_gateway.profile" => config.approval_gateway.gateway.profile_revision.to_string(),
        "approval_gateway.configuration" => {
            config.approval_gateway.configuration_revision.to_string()
        }
        "web_search" => config.web_search.revision.to_string(),
        "context_management" => config.context_management.revision.to_string(),
        "terminal_completion" => config.terminal_completion.revision.to_string(),
        "subagent_policy" => config.subagent_policy.revision.to_string(),
        "goal_budget_policy" => config.goal_budget_policy.revision.to_string(),
        "schedule_budget_policy" => config.schedule_budget_policy.revision.to_string(),
        "model_metrics" => config.model_metrics.revision.clone(),
        _ => unreachable!("known versioned section"),
    }
}

pub(super) fn advance_revision(
    config: &mut GlobalConfig,
    key: &str,
    previous: &str,
) -> Result<(), DbErr> {
    if key.starts_with("ai_gateway.") || key.starts_with("approval_gateway.") {
        let next = previous
            .parse::<i64>()
            .map_err(|_| invalid(key))?
            .saturating_add(1)
            .max(1);
        match key {
            "ai_gateway.connection" => config.ai_gateway.connection_revision = next,
            "ai_gateway.profile" => config.ai_gateway.profile_revision = next,
            "approval_gateway.connection" => {
                config.approval_gateway.gateway.connection_revision = next
            }
            "approval_gateway.profile" => config.approval_gateway.gateway.profile_revision = next,
            "approval_gateway.configuration" => {
                config.approval_gateway.configuration_revision = next
            }
            _ => unreachable!("known gateway revision"),
        }
    } else if key == "model_metrics" {
        config.model_metrics.revision = previous
            .parse::<i64>()
            .map_err(|_| invalid(key))?
            .checked_add(1)
            .ok_or_else(|| invalid(key))?
            .to_string();
    } else {
        let next = previous
            .parse::<u64>()
            .map_err(|_| invalid(key))?
            .checked_add(1)
            .ok_or_else(|| invalid(key))?;
        match key {
            "web_search" => config.web_search.revision = next,
            "context_management" => config.context_management.revision = next,
            "terminal_completion" => config.terminal_completion.revision = next,
            "subagent_policy" => config.subagent_policy.revision = next,
            "goal_budget_policy" => config.goal_budget_policy.revision = next,
            "schedule_budget_policy" => config.schedule_budget_policy.revision = next,
            _ => unreachable!("known policy revision"),
        }
    }
    Ok(())
}
