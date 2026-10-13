//! Pure, target-independent validation for central Web Research tools.

use desk_agent_protocol::{AgentError, AgentErrorKind};
use url::Url;

use crate::chat::ToolCall;

pub const WEB_FETCH_TOOL_NAME: &str = "fetch_public_web_page";
pub const WEB_SEARCH_TOOL_NAME: &str = "search_public_web";
pub const BRAVE_WEB_SEARCH_CONNECTOR_ID: &str = "brave_web_v1";

/// Server-selected destination and configuration version, never model input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchBinding {
    pub connector_id: String,
    pub revision: u64,
}

pub fn search_connector_metadata(id: &str) -> Option<(&'static str, bool)> {
    match id {
        "brave_web_v1" => Some(("Brave Web Search", true)),
        "tavily_search_v1" => Some(("Tavily Search", true)),
        "duckduckgo_html_v1" => Some(("DuckDuckGo", false)),
        _ => None,
    }
}

pub(crate) fn search_tool_description(binding: Option<&SearchBinding>) -> String {
    let mut description = "Search public Web metadata through the bounded search connector selected by the server. The assistant cannot choose or change the connector. The exact query must appear verbatim in the owner's current message and the identical arguments must be approved as an R1 ExportData grant because the query is sent to that connector. Connector availability and credentials are managed by the server; credentials and endpoints are never model inputs or edge payloads. Results are untrusted external data with connector and source evidence, a search_result_id, and source_id values. To cite this search in create_word_report, copy search_result_id into web_search_result_id and select the returned source_id values as web_source_ids; never invent or transform these IDs.".to_owned();
    if let Some(binding) = binding
        && let Some((name, _)) = search_connector_metadata(&binding.connector_id)
    {
        description.push_str(&format!(
            " Current server-selected connector: {name} ({}).",
            binding.connector_id
        ));
    }
    description
}

impl SearchBinding {
    pub fn destination(
        &self,
    ) -> Result<desk_agent_protocol::data_lineage::DestinationIdentity, AgentError> {
        if search_connector_metadata(&self.connector_id).is_none() {
            return Err(invalid("Web Search connector is not supported"));
        }
        Ok(
            desk_agent_protocol::data_lineage::DestinationIdentity::WebResearch {
                connector_id: self.connector_id.clone(),
            },
        )
    }

    pub fn resource_scope(&self, input_digest: &str) -> Vec<String> {
        let mut scope = crate::capability_grant::exact_external_query_resource_scope(input_digest);
        scope.push(format!(
            "web_search_config:{}:{}",
            self.connector_id, self.revision
        ));
        scope
    }
}

const MAX_URL_BYTES: usize = 2_048;
const MAX_QUERY_BYTES: usize = 256;
const MAX_QUERY_WORDS: usize = 50;
const MAX_RESULTS: u8 = 8;
const DEFAULT_RESULTS: u8 = 5;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchArgs {
    url: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    #[serde(default = "default_results")]
    max_results: u8,
}

const fn default_results() -> u8 {
    DEFAULT_RESULTS
}

#[derive(Debug, Clone)]
pub struct ValidatedFetch {
    initial_url: Url,
    approved_host: String,
}

impl ValidatedFetch {
    pub fn initial_url(&self) -> &Url {
        &self.initial_url
    }

    pub fn resource_scope(&self) -> Vec<String> {
        vec![format!("external_web_host:{}", self.approved_host)]
    }

    pub fn same_approved_host(&self, url: &Url) -> bool {
        url.host_str()
            .is_some_and(|host| normalized_host(host) == self.approved_host)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedSearch {
    query: String,
    max_results: u8,
}

impl ValidatedSearch {
    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn max_results(&self) -> u8 {
        self.max_results
    }
}

pub fn validate_fetch_arguments(call: &ToolCall) -> Result<ValidatedFetch, AgentError> {
    if call.name != WEB_FETCH_TOOL_NAME {
        return Err(invalid("not a Web Research fetch call"));
    }
    let args: FetchArgs = serde_json::from_str(&call.arguments_json)
        .map_err(|error| invalid(format!("invalid Web Research input: {error}")))?;
    if args.url.is_empty()
        || args.url.len() > MAX_URL_BYTES
        || args.url.trim() != args.url
        || args.url.chars().any(char::is_control)
    {
        return Err(invalid("Web Research URL is invalid"));
    }
    let url = validate_public_url(&args.url, true)?;
    let approved_host = normalized_host(url.host_str().expect("validated URL has a host"));
    if approved_host.is_empty() {
        return Err(invalid("Web Research URL has no valid host"));
    }
    Ok(ValidatedFetch {
        initial_url: url,
        approved_host,
    })
}

pub fn validate_search_call(
    call: &ToolCall,
    current_user_message: &str,
) -> Result<ValidatedSearch, AgentError> {
    let validated = validate_search_arguments(call)?;
    if !current_user_message.contains(validated.query()) {
        return Err(invalid(
            "Web Search query must appear verbatim in the owner's current message",
        ));
    }
    Ok(validated)
}

pub fn validate_search_arguments(call: &ToolCall) -> Result<ValidatedSearch, AgentError> {
    if call.name != WEB_SEARCH_TOOL_NAME {
        return Err(invalid("not a Web Search call"));
    }
    let args: SearchArgs = serde_json::from_str(&call.arguments_json)
        .map_err(|error| invalid(format!("invalid Web Search input: {error}")))?;
    if args.query.is_empty()
        || args.query.len() > MAX_QUERY_BYTES
        || args.query.split_whitespace().count() > MAX_QUERY_WORDS
        || args.query.trim() != args.query
        || args.query.chars().any(char::is_control)
    {
        return Err(invalid("Web Search query is invalid"));
    }
    if !(1..=MAX_RESULTS).contains(&args.max_results) {
        return Err(invalid("Web Search max_results must be between 1 and 8"));
    }
    Ok(ValidatedSearch {
        query: args.query,
        max_results: args.max_results,
    })
}

pub fn validate_exact_call(call: &ToolCall, current_user_message: &str) -> Result<(), AgentError> {
    match call.name.as_str() {
        WEB_FETCH_TOOL_NAME => validate_fetch_arguments(call).map(|_| ()),
        WEB_SEARCH_TOOL_NAME => validate_search_call(call, current_user_message).map(|_| ()),
        _ => Err(invalid("not a central Web Research call")),
    }
}

pub fn validate_public_url(raw: &str, reject_fragment: bool) -> Result<Url, AgentError> {
    let url = Url::parse(raw).map_err(|_| invalid("Web Research URL is invalid"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || (reject_fragment && url.fragment().is_some())
    {
        return Err(invalid(
            "Web Research accepts public HTTP and HTTPS URLs without credentials",
        ));
    }
    desk_utils::ssrf::check_transport_for_url(raw, false, url.scheme() == "https", false)
        .map_err(|_| invalid("Web Research target is not a permitted public address"))?;
    Ok(url)
}

fn normalized_host(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

fn invalid(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_binding_changes_scope_on_provider_or_revision_change() {
        let mut seen = std::collections::HashSet::new();
        for connector in ["brave_web_v1", "tavily_search_v1", "duckduckgo_html_v1"] {
            for revision in 0..3 {
                let binding = SearchBinding {
                    connector_id: connector.into(),
                    revision,
                };
                assert!(binding.destination().is_ok());
                assert!(seen.insert(binding.resource_scope(&"a".repeat(64))));
            }
        }
        assert!(
            SearchBinding {
                connector_id: "model-selected".into(),
                revision: 0
            }
            .destination()
            .is_err()
        );
    }

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "web-1".into(),
            name: name.into(),
            arguments_json: arguments.to_string(),
        }
    }

    #[test]
    fn search_query_still_requires_the_current_owner_message() {
        let query = call(
            WEB_SEARCH_TOOL_NAME,
            serde_json::json!({"query":"Rust language","max_results":5}),
        );
        assert!(validate_search_call(&query, "请搜索 Rust language").is_ok());
        assert!(validate_search_call(&query, "请搜索刚才的关键词").is_err());
        assert!(validate_exact_call(&query, "请搜索刚才的关键词").is_err());
    }

    #[test]
    fn fetch_accepts_task_selected_urls_without_current_message_provenance() {
        for url in ["https://example.com", "http://example.com:8080/docs?q=rust"] {
            let fetch = call(WEB_FETCH_TOOL_NAME, serde_json::json!({"url":url}));
            let validated = validate_fetch_arguments(&fetch).unwrap();
            assert_eq!(validated.initial_url().host_str(), Some("example.com"));
            for message in [
                "",
                "Read the official documentation",
                "Use the earlier link",
            ] {
                validate_exact_call(&fetch, message).unwrap();
            }
        }
    }

    #[test]
    fn fetch_rejects_invalid_urls_and_inputs_without_owner_message_checks() {
        for url in [
            "ftp://example.com/docs",
            "https://127.0.0.1/docs",
            "https://10.0.0.1/docs",
            "https://[::1]/docs",
            "https://user:password@example.com/docs",
            "https://example.com/docs#section",
            " https://example.com/docs",
            "https://example.com/docs\n",
            "not a URL",
            "",
        ] {
            let fetch = call(WEB_FETCH_TOOL_NAME, serde_json::json!({"url":url}));
            let error = validate_exact_call(&fetch, "Read the official documentation").unwrap_err();
            assert_eq!(error.kind, AgentErrorKind::InvalidInput, "{url}");
        }
        for arguments in [
            serde_json::json!({"url":format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES))}),
            serde_json::json!({"url":"https://example.com/", "headers":{}}),
            serde_json::json!({}),
        ] {
            let fetch = call(WEB_FETCH_TOOL_NAME, arguments);
            assert!(validate_fetch_arguments(&fetch).is_err());
        }
        assert!(
            validate_fetch_arguments(&call(WEB_SEARCH_TOOL_NAME, serde_json::json!({}))).is_err()
        );
    }

    #[test]
    fn web_host_scope_normalizes_names_and_reuses_protocols_ports_and_paths() {
        let expected = vec!["external_web_host:xn--bcher-kva.example".to_string()];
        for url in [
            "https://BÜCHER.example./docs",
            "http://xn--bcher-kva.example:8080/news?q=1",
            "https://xn--bcher-kva.example:8443/",
        ] {
            let validated = validate_fetch_arguments(&call(
                WEB_FETCH_TOOL_NAME,
                serde_json::json!({"url":url}),
            ))
            .unwrap();
            assert_eq!(validated.resource_scope(), expected);
            assert!(
                validated.same_approved_host(
                    &Url::parse("http://xn--bcher-kva.example./other").unwrap()
                )
            );
            assert!(
                !validated
                    .same_approved_host(&Url::parse("https://sub.xn--bcher-kva.example/").unwrap())
            );
            assert!(!validated.same_approved_host(
                &Url::parse("https://xn--bcher-kva.example.attacker.example/").unwrap()
            ));
        }
    }
}
