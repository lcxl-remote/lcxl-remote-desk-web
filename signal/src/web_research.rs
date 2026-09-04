//! OSS central Web Research adapters over the shared validated connectors.

use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::seam::ToolRunOutput;
pub use desk_diagnose_core::web_research::{
    ValidatedFetch, ValidatedSearch, WEB_FETCH_TOOL_NAME, WEB_SEARCH_TOOL_NAME,
    validate_fetch_call, validate_search_call,
};
use desk_signal_facade::web_search::{BraveSearchConfig, OSS_API_KEY_ENV};

pub(crate) async fn fetch_public_web_page(
    validated: ValidatedFetch,
) -> Result<ToolRunOutput, AgentError> {
    desk_signal_facade::web_fetch::fetch_public_web_page(validated).await
}

pub(crate) async fn search_public_web(
    validated: ValidatedSearch,
    server_call_id: &str,
) -> Result<ToolRunOutput, AgentError> {
    let config = production_search_config().ok_or_else(|| AgentError {
        kind: AgentErrorKind::UnsupportedCapability,
        message: "Web Search is not configured".into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    })?;
    desk_signal_facade::web_search::search_public_web(&config, validated, server_call_id).await
}

pub(crate) fn production_search_config() -> Option<BraveSearchConfig> {
    BraveSearchConfig::from_env(OSS_API_KEY_ENV)
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_diagnose_core::chat::ToolCall;

    #[test]
    fn exact_owner_inputs_are_required_before_any_network_call() {
        let url = "https://example.com/report?q=1";
        let fetch = ToolCall {
            id: "fetch-1".into(),
            name: WEB_FETCH_TOOL_NAME.into(),
            arguments_json: serde_json::json!({"url":url}).to_string(),
        };
        assert!(validate_fetch_call(&fetch, &format!("请读取 {url}")).is_ok());
        assert!(validate_fetch_call(&fetch, "读取刚才的网址").is_err());

        let search = ToolCall {
            id: "search-1".into(),
            name: WEB_SEARCH_TOOL_NAME.into(),
            arguments_json: serde_json::json!({"query":"Rust language","max_results":5})
                .to_string(),
        };
        assert!(validate_search_call(&search, "请搜索 Rust language").is_ok());
        assert!(validate_search_call(&search, "请搜索刚才的关键词").is_err());
    }
}
