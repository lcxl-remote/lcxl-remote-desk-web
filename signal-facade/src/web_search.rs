//! Shared production Web Search connector used by OSS Signal and Manager.
//!
//! The endpoint and destination identity are compiled constants. Only the
//! central service receives the credential; model and edge payloads contain
//! neither a key nor a caller-selected endpoint.

use futures_util::StreamExt;
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::{seam::ToolRunOutput, web_research::ValidatedSearch};

pub mod config;
mod duckduckgo;
pub use config::{
    SearchConfig, SearchConfigPublic, SearchConfigUpdate, SearchProvider, SearchProviderInfo,
};

pub const CONNECTION_TEST_QUERY: &str = "Rust programming language";

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct SearchTestResult {
    pub provider: SearchProvider,
    pub latency_ms: u64,
    pub result_count: usize,
}

/// Explicit administrative probe, not an assistant tool or a stored update.
pub async fn test_connection(config: &SearchConfig) -> Result<SearchTestResult, AgentError> {
    let started = std::time::Instant::now();
    let call = desk_diagnose_core::chat::ToolCall {
        id: uuid::Uuid::new_v4().to_string(),
        name: desk_diagnose_core::web_research::WEB_SEARCH_TOOL_NAME.into(),
        arguments_json: serde_json::json!({"query": CONNECTION_TEST_QUERY, "max_results": 1})
            .to_string(),
    };
    let validated =
        desk_diagnose_core::web_research::validate_search_call(&call, CONNECTION_TEST_QUERY)?;
    let output = search_configured_web(config, validated, &call.id).await?;
    let value: serde_json::Value = serde_json::from_str(&output.content)
        .map_err(|_| internal("invalid Web Search probe result"))?;
    Ok(SearchTestResult {
        provider: config.provider,
        latency_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        result_count: value["result_count"].as_u64().unwrap_or(0) as usize,
    })
}

const SEARCH_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_TITLE_CHARS: usize = 512;
const MAX_SNIPPET_CHARS: usize = 2_000;
const MAX_URL_BYTES: usize = 2_048;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WebSearchResult {
    title: String,
    url: String,
    snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    published_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BraveResponse {
    #[serde(default)]
    web: Option<BraveWebResults>,
}

#[derive(Debug, Deserialize)]
struct BraveWebResults {
    results: Vec<BraveResult>,
}

#[derive(Debug, Deserialize)]
struct BraveResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    page_age: Option<String>,
}

/// Execute a configured provider. The caller must bind and revalidate its
/// configuration revision before each dispatch; this layer never falls back.
pub async fn search_configured_web(
    config: &SearchConfig,
    validated: ValidatedSearch,
    server_call_id: &str,
) -> Result<ToolRunOutput, AgentError> {
    config.validate().map_err(unavailable)?;
    if !config.configured() {
        return Err(unavailable("Web Search is not configured"));
    }
    if server_call_id.trim().is_empty() || server_call_id.len() > 512 {
        return Err(invalid("Web Search call identity is invalid"));
    }
    let client = reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|_| unavailable("Web Search client is unavailable"))?;
    let request = configured_request(&client, config, &validated)?;
    let response = request.send().await.map_err(|error| {
        if error.is_timeout() {
            transport("Web Search request timed out")
        } else if error.is_connect() {
            transport("Web Search connection failed")
        } else {
            transport("Web Search request failed")
        }
    })?;
    project_http_response(config, response, &validated, server_call_id).await
}

async fn project_http_response(
    config: &SearchConfig,
    response: reqwest::Response,
    validated: &ValidatedSearch,
    server_call_id: &str,
) -> Result<ToolRunOutput, AgentError> {
    let status = response.status();
    if !status.is_success() {
        return Err(match status {
            StatusCode::TOO_MANY_REQUESTS => error(
                AgentErrorKind::TransportError,
                "Web Search rate limit was reached",
                false,
            ),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                unavailable("Web Search provider rejected access")
            }
            status if matches!(status.as_u16(), 432 | 433) => {
                unavailable("Web Search provider quota was reached")
            }
            status if status.is_server_error() => {
                transport("Web Search provider returned a server error")
            }
            _ => error(
                AgentErrorKind::TransportError,
                "Web Search provider returned an unexpected status",
                false,
            ),
        });
    }
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let expected = match config.provider {
        SearchProvider::DuckDuckGo => "text/html",
        _ => "application/json",
    };
    if content_type != expected {
        return Err(error(
            AgentErrorKind::TransportError,
            "Web Search response content type is invalid",
            false,
        ));
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
    {
        return Err(transport("Web Search response exceeds the byte limit"));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| transport("Web Search response body failed"))?;
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(transport("Web Search response exceeds the byte limit"));
        }
        body.extend_from_slice(&chunk);
    }
    project_configured_response(
        config,
        &body,
        validated,
        server_call_id,
        &chrono::Utc::now().to_rfc3339(),
    )
}

fn configured_request(
    client: &reqwest::Client,
    config: &SearchConfig,
    input: &ValidatedSearch,
) -> Result<reqwest::RequestBuilder, AgentError> {
    config.validate().map_err(unavailable)?;
    let token = || -> Result<header::HeaderValue, AgentError> {
        let mut value = header::HeaderValue::from_str(
            config
                .api_key()
                .ok_or_else(|| unavailable("Web Search is not configured"))?,
        )
        .map_err(|_| unavailable("Web Search credential is invalid"))?;
        value.set_sensitive(true);
        Ok(value)
    };
    Ok(match config.provider {
        SearchProvider::DuckDuckGo => client
            .get("https://html.duckduckgo.com/html/")
            .header(header::ACCEPT, "text/html")
            .query(&[("q", input.query()), ("kp", "1")]),
        SearchProvider::Brave => client
            .get(SEARCH_ENDPOINT)
            .header(header::ACCEPT, "application/json")
            .header("X-Subscription-Token", token()?)
            .query(&[
                ("q", input.query()),
                ("count", &input.max_results().to_string()),
                ("safesearch", "strict"),
            ]),
        SearchProvider::Tavily => {
            let mut value = header::HeaderValue::from_str(&format!(
                "Bearer {}",
                config
                    .api_key()
                    .ok_or_else(|| unavailable("Web Search is not configured"))?
            ))
            .map_err(|_| unavailable("Web Search credential is invalid"))?;
            value.set_sensitive(true);
            client
                .post("https://api.tavily.com/search")
                .header(header::ACCEPT, "application/json")
                .header(header::AUTHORIZATION, value)
                .json(&serde_json::json!({
                    "query": input.query(), "max_results": input.max_results(),
                    "search_depth": "basic", "topic": "general", "auto_parameters": false,
                    "include_answer": false, "include_raw_content": false, "include_images": false,
                    "safe_search": true,
                }))
        }
    })
}

#[derive(Deserialize)]
struct TavilyResponse {
    results: Vec<TavilyResult>,
}

#[derive(Deserialize)]
struct TavilyResult {
    title: String,
    url: String,
    content: String,
}

fn project_configured_response(
    config: &SearchConfig,
    body: &[u8],
    validated: &ValidatedSearch,
    call_id: &str,
    searched_at: &str,
) -> Result<ToolRunOutput, AgentError> {
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(transport("Web Search response exceeds the byte limit"));
    }
    let items = match config.provider {
        SearchProvider::Brave => {
            let response: BraveResponse = serde_json::from_slice(body)
                .map_err(|_| transport("Web Search response schema is invalid"))?;
            response
                .web
                .ok_or_else(|| transport("Web Search response schema is invalid"))?
                .results
        }
        SearchProvider::Tavily => {
            let response: TavilyResponse = serde_json::from_slice(body)
                .map_err(|_| transport("Web Search response schema is invalid"))?;
            response
                .results
                .into_iter()
                .map(|item| BraveResult {
                    title: item.title,
                    url: item.url,
                    description: item.content,
                    page_age: None,
                })
                .collect()
        }
        SearchProvider::DuckDuckGo => duckduckgo::parse(body)?,
    };
    project_results(
        items,
        body,
        validated,
        call_id,
        searched_at,
        config.provider,
        config.revision,
    )
}

fn project_results(
    items: Vec<BraveResult>,
    body: &[u8],
    validated: &ValidatedSearch,
    server_call_id: &str,
    searched_at: &str,
    provider: SearchProvider,
    revision: u64,
) -> Result<ToolRunOutput, AgentError> {
    let mut results = Vec::new();
    for item in items {
        if results.len() >= usize::from(validated.max_results()) {
            break;
        }
        let Some(url) = validated_public_result_url(&item.url) else {
            continue;
        };
        let title = bounded_text(&item.title, MAX_TITLE_CHARS);
        if title.is_empty()
            || results
                .iter()
                .any(|known: &WebSearchResult| known.url == url)
        {
            continue;
        }
        results.push(WebSearchResult {
            title,
            url,
            snippet: bounded_text(&item.description, MAX_SNIPPET_CHARS),
            published_at: item
                .page_age
                .as_deref()
                .map(|value| bounded_text(value, 128))
                .filter(|value| !value.is_empty()),
        });
    }
    let result = serde_json::json!({
        "schema_version": 1,
        "configuration_revision": revision,
        "web_search_call_id": server_call_id,
        "untrusted_external_content": true,
        "connector": {
            "connector_id": provider.connector_id(),
            "display_name": provider.display_name(),
            "requires_api_key": provider.requires_api_key(),
            "experimental": false,
        },
        "query_sha256": format!("{:x}", Sha256::digest(validated.query().as_bytes())),
        "searched_at": searched_at,
        "response_sha256": format!("{:x}", Sha256::digest(body)),
        "response_bytes": body.len(),
        "result_count": results.len(),
        "results": results,
    });
    Ok(ToolRunOutput {
        content: serde_json::to_string(&result)
            .map_err(|_| internal("failed to encode Web Search result"))?,
        image_data_url: None,
    })
}

fn validated_public_result_url(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.len() > MAX_URL_BYTES {
        return None;
    }
    let parsed = url::Url::parse(raw).ok()?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return None;
    }
    desk_utils::ssrf::check_transport_for_url(raw, false, true, true).ok()?;
    Some(parsed.to_string())
}

fn bounded_text(source: &str, max_chars: usize) -> String {
    let mut plain = String::with_capacity(source.len().min(max_chars));
    let mut in_tag = false;
    for ch in source.chars() {
        match ch {
            '<' => {
                in_tag = true;
                plain.push(' ');
            }
            '>' => {
                in_tag = false;
                plain.push(' ');
            }
            _ if !in_tag => plain.push(ch),
            _ => {}
        }
    }
    plain
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

fn error(kind: AgentErrorKind, message: impl Into<String>, retryable: bool) -> AgentError {
    AgentError {
        kind,
        message: message.into(),
        retryable,
        safe_for_model: true,
        error_code: None,
    }
}

fn invalid(message: impl Into<String>) -> AgentError {
    error(AgentErrorKind::InvalidInput, message, false)
}

fn unavailable(message: impl Into<String>) -> AgentError {
    error(AgentErrorKind::UnsupportedCapability, message, false)
}

fn transport(message: impl Into<String>) -> AgentError {
    error(AgentErrorKind::TransportError, message, true)
}

fn internal(message: impl Into<String>) -> AgentError {
    error(AgentErrorKind::Internal, message, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_diagnose_core::{chat::ToolCall, web_research::validate_search_call};

    #[tokio::test]
    async fn http_failures_are_bounded_and_never_expose_provider_bodies() {
        let call = call(serde_json::json!({"query":"Rust language","max_results":2}));
        let input = validate_search_call(&call, "Rust language").unwrap();
        let config = SearchConfig::default();
        for (status, retryable) in [
            (301, false),
            (401, false),
            (403, false),
            (429, false),
            (432, false),
            (433, false),
            (500, true),
        ] {
            let response = http::Response::builder()
                .status(status)
                .body("provider-secret-error-body")
                .unwrap()
                .into();
            let error = project_http_response(&config, response, &input, &call.id)
                .await
                .unwrap_err();
            assert_eq!(error.retryable, retryable, "HTTP {status}");
            assert!(!error.message.contains("provider-secret"));
        }
        for (content_type, length, body) in [
            ("application/json", None, b"{}".to_vec()),
            ("text/html", Some(MAX_RESPONSE_BYTES + 1), b"short".to_vec()),
            ("text/html", None, vec![b'x'; MAX_RESPONSE_BYTES + 1]),
        ] {
            let mut builder = http::Response::builder().header("content-type", content_type);
            if let Some(length) = length {
                builder = builder.header("content-length", length);
            }
            let response = builder.body(body).unwrap().into();
            assert!(
                project_http_response(&config, response, &input, &call.id)
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    #[ignore = "Explicit real-network check using a fixed public query; no credentials"]
    async fn duckduckgo_live_connection() {
        let result = test_connection(&SearchConfig::default()).await.unwrap();
        println!(
            "DuckDuckGo live search: {} result(s), {} ms",
            result.result_count, result.latency_ms
        );
    }

    fn call(arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "search-1".into(),
            name: desk_diagnose_core::web_research::WEB_SEARCH_TOOL_NAME.into(),
            arguments_json: arguments.to_string(),
        }
    }

    #[test]
    fn provider_requests_use_fixed_endpoints_and_isolated_credentials() {
        let call = call(serde_json::json!({"query":"Rust language","max_results":2}));
        let input = validate_search_call(&call, "Rust language").unwrap();
        let initial = SearchConfig::default();
        for provider in SearchProvider::ALL {
            let config = initial
                .candidate(&SearchConfigUpdate {
                    expected_revision: 0,
                    provider,
                    api_key: provider
                        .requires_api_key()
                        .then(|| "sentinel-secret".into()),
                })
                .unwrap();
            let request = configured_request(&reqwest::Client::new(), &config, &input)
                .unwrap()
                .build()
                .unwrap();
            assert!(!request.url().as_str().contains("sentinel-secret"));
            match provider {
                SearchProvider::DuckDuckGo => {
                    assert_eq!(request.url().host_str(), Some("html.duckduckgo.com"));
                    assert_eq!(request.method(), reqwest::Method::GET);
                    assert!(!request.headers().contains_key(header::AUTHORIZATION));
                    assert!(!request.headers().contains_key("X-Subscription-Token"));
                }
                SearchProvider::Brave => {
                    assert_eq!(request.url().host_str(), Some("api.search.brave.com"));
                    assert!(request.headers()["X-Subscription-Token"].is_sensitive());
                    assert!(!request.headers().contains_key(header::AUTHORIZATION));
                }
                SearchProvider::Tavily => {
                    assert_eq!(request.url().as_str(), "https://api.tavily.com/search");
                    assert_eq!(request.method(), reqwest::Method::POST);
                    assert!(request.headers()[header::AUTHORIZATION].is_sensitive());
                    let body: serde_json::Value =
                        serde_json::from_slice(request.body().unwrap().as_bytes().unwrap())
                            .unwrap();
                    assert_eq!(body["query"], "Rust language");
                    assert_eq!(body["max_results"], 2);
                    assert_eq!(body["include_answer"], false);
                    assert_eq!(body["auto_parameters"], false);
                    assert!(!body.to_string().contains("sentinel-secret"));
                }
            }
        }
    }

    #[test]
    fn all_provider_envelopes_identify_source_and_revision() {
        let call = call(serde_json::json!({"query":"Rust language","max_results":2}));
        let input = validate_search_call(&call, "Rust language").unwrap();
        for (provider, body) in [
            (
                SearchProvider::Brave,
                r#"{"web":{"results":[{"title":"Rust","url":"https://example.com","description":"language"}]}}"#,
            ),
            (
                SearchProvider::Tavily,
                r#"{"results":[{"title":"Rust","url":"https://example.com","content":"language"}],"answer":"not forwarded"}"#,
            ),
            (
                SearchProvider::DuckDuckGo,
                r#"<div class="result"><a class="result__a" href="https://example.com">Rust</a><div class="result__snippet">language</div></div>"#,
            ),
        ] {
            let config = SearchConfig::default()
                .candidate(&SearchConfigUpdate {
                    expected_revision: 0,
                    provider,
                    api_key: provider.requires_api_key().then(|| "secret".into()),
                })
                .unwrap();
            let output =
                project_configured_response(&config, body.as_bytes(), &input, &call.id, "now")
                    .unwrap();
            let value: serde_json::Value = serde_json::from_str(&output.content).unwrap();
            assert_eq!(value["connector"]["connector_id"], provider.connector_id());
            assert_eq!(
                value["connector"]["requires_api_key"],
                provider.requires_api_key()
            );
            assert_eq!(value["configuration_revision"], 1);
            assert_eq!(value["result_count"], 1);
            let registry =
                desk_diagnose_core::device_assistant::device_assistant_provider_registry()
                    .with_web_search_binding(config.binding());
            desk_diagnose_core::provider_preflight::read::limits::validate_output(
                &registry,
                &call,
                &output,
                &desk_agent_protocol::capability_grant::CapabilityGrantLimits {
                    max_bytes_per_call: 32768,
                    max_items_per_call: 8,
                    max_calls: 1,
                },
            )
            .unwrap();
            assert!(!output.content.contains("not forwarded"));
            assert!(!output.content.contains("secret"));
            assert!(project_configured_response(&config, b"{}", &input, &call.id, "now").is_err());
        }
    }

    #[test]
    fn exact_owner_query_and_bounded_result_count_are_required() {
        let exact = call(serde_json::json!({"query":"Rust language","max_results":5}));
        assert!(validate_search_call(&exact, "请搜索 Rust language").is_ok());
        assert!(validate_search_call(&exact, "请搜索刚才的关键词").is_err());
        assert!(
            validate_search_call(
                &call(serde_json::json!({"query":"Rust language","max_results":9})),
                "Rust language"
            )
            .is_err()
        );
        assert!(
            validate_search_call(
                &call(serde_json::json!({"query":"bad\nquery"})),
                "bad\nquery"
            )
            .is_err()
        );
    }

    #[test]
    fn response_projection_drops_non_public_urls_and_bounds_untrusted_markup() {
        assert!(validated_public_result_url("https://example.com/a").is_some());
        assert!(validated_public_result_url("http://example.com/a").is_none());
        assert!(validated_public_result_url("https://127.0.0.1/a").is_none());
        assert_eq!(bounded_text("<b>A &amp; B</b>", 20), "A & B");
        assert_eq!(bounded_text("abcdef", 3), "abc");
    }

    #[test]
    fn recorded_brave_corpus_projects_a_bounded_provenance_envelope() {
        let call = call(serde_json::json!({"query":"Rust language","max_results":2}));
        let validated = validate_search_call(&call, "请搜索 Rust language").unwrap();
        let body = serde_json::to_vec(&serde_json::json!({
            "web": {"results": [
                {
                    "title": "<b>Rust</b> &amp; safety",
                    "url": "https://www.rust-lang.org/learn",
                    "description": "A <em>systems</em> language",
                    "page_age": "2026-08-20"
                },
                {
                    "title": "Private",
                    "url": "https://127.0.0.1/secret",
                    "description": "must be dropped"
                },
                {
                    "title": "Docs",
                    "url": "https://doc.rust-lang.org/book/",
                    "description": "The Book"
                },
                {
                    "title": "Over limit",
                    "url": "https://example.com/third",
                    "description": "must be truncated"
                }
            ]}
        }))
        .unwrap();
        let output = project_configured_response(
            &SearchConfig::default()
                .candidate(&SearchConfigUpdate {
                    expected_revision: 0,
                    provider: SearchProvider::Brave,
                    api_key: Some("test".into()),
                })
                .unwrap(),
            &body,
            &validated,
            "server-call-1",
            "2026-08-28T00:00:00Z",
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["web_search_call_id"], "server-call-1");
        assert_eq!(
            value["connector"]["connector_id"],
            SearchProvider::Brave.connector_id()
        );
        assert_eq!(value["connector"]["experimental"], false);
        assert_eq!(value["searched_at"], "2026-08-28T00:00:00Z");
        assert_eq!(value["result_count"], 2);
        assert_eq!(value["results"][0]["title"], "Rust & safety");
        assert_eq!(value["results"][0]["snippet"], "A systems language");
        assert_eq!(
            value["results"][1]["url"],
            "https://doc.rust-lang.org/book/"
        );
        assert!(!output.content.contains("127.0.0.1"));
        assert!(!output.content.contains("Over limit"));
    }
}
