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
use desk_diagnose_core::{
    seam::ToolRunOutput,
    web_research::{BRAVE_WEB_SEARCH_CONNECTOR_ID, ValidatedSearch},
};

pub const BRAVE_WEB_SEARCH_DISPLAY_NAME: &str = "Brave Web Search";
pub const OSS_API_KEY_ENV: &str = "LRD_BRAVE_SEARCH_API_KEY";

const SEARCH_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_TITLE_CHARS: usize = 512;
const MAX_SNIPPET_CHARS: usize = 2_000;
const MAX_URL_BYTES: usize = 2_048;

#[derive(Clone)]
pub struct BraveSearchConfig {
    api_key: String,
}

impl std::fmt::Debug for BraveSearchConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BraveSearchConfig")
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

impl BraveSearchConfig {
    pub fn from_env(name: &str) -> Option<Self> {
        Self::from_secret(std::env::var(name).ok().as_deref())
    }

    pub fn from_secret(raw: Option<&str>) -> Option<Self> {
        let value = raw?.trim();
        if value.is_empty() || value.len() > 4_096 || value.chars().any(char::is_control) {
            return None;
        }
        Some(Self {
            api_key: value.to_string(),
        })
    }

    /// Stable within a process without revealing the credential. This binds a
    /// consumed Manager read grant to the exact central connector configuration.
    pub fn readiness_identity(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(BRAVE_WEB_SEARCH_CONNECTOR_ID.as_bytes());
        digest.update((self.api_key.len() as u64).to_le_bytes());
        digest.update(self.api_key.as_bytes());
        format!("brave:{:x}", digest.finalize())
    }
}

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
    #[serde(default)]
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

pub async fn search_public_web(
    config: &BraveSearchConfig,
    validated: ValidatedSearch,
    server_call_id: &str,
) -> Result<ToolRunOutput, AgentError> {
    if server_call_id.trim().is_empty() || server_call_id.len() > 512 {
        return Err(invalid("Web Search call identity is invalid"));
    }
    let mut token = header::HeaderValue::from_str(&config.api_key)
        .map_err(|_| unavailable("Web Search credential is invalid"))?;
    token.set_sensitive(true);
    let client = reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|_| unavailable("Web Search client is unavailable"))?;
    let count = validated.max_results().to_string();
    let response = client
        .get(SEARCH_ENDPOINT)
        .header(header::ACCEPT, "application/json")
        .header("X-Subscription-Token", token)
        .query(&[
            ("q", validated.query()),
            ("count", count.as_str()),
            ("safesearch", "strict"),
        ])
        .send()
        .await
        .map_err(|_| transport("Web Search request failed"))?;
    let status = response.status();
    if status.is_redirection() {
        return Err(transport(
            "Web Search endpoint returned an unexpected redirect",
        ));
    }
    if !status.is_success() {
        return Err(match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                unavailable("Web Search credential was rejected")
            }
            StatusCode::TOO_MANY_REQUESTS => transport("Web Search rate limit was reached"),
            status if status.is_server_error() => {
                transport("Web Search provider returned a server error")
            }
            _ => invalid("Web Search provider rejected the request"),
        });
    }
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if content_type != "application/json" {
        return Err(transport("Web Search response is not JSON"));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
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
    project_brave_response(
        &body,
        &validated,
        server_call_id,
        &chrono::Utc::now().to_rfc3339(),
    )
}

fn project_brave_response(
    body: &[u8],
    validated: &ValidatedSearch,
    server_call_id: &str,
    searched_at: &str,
) -> Result<ToolRunOutput, AgentError> {
    let response: BraveResponse = serde_json::from_slice(body)
        .map_err(|_| transport("Web Search response schema is invalid"))?;
    let mut results = Vec::new();
    for item in response.web.into_iter().flat_map(|web| web.results) {
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
        "web_search_call_id": server_call_id,
        "untrusted_external_content": true,
        "connector": {
            "connector_id": BRAVE_WEB_SEARCH_CONNECTOR_ID,
            "display_name": BRAVE_WEB_SEARCH_DISPLAY_NAME,
            "requires_api_key": true,
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

    fn call(arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "search-1".into(),
            name: desk_diagnose_core::web_research::WEB_SEARCH_TOOL_NAME.into(),
            arguments_json: arguments.to_string(),
        }
    }

    #[test]
    fn configuration_rejects_missing_blank_control_and_oversized_secrets() {
        assert!(BraveSearchConfig::from_secret(None).is_none());
        assert!(BraveSearchConfig::from_secret(Some("  ")).is_none());
        assert!(BraveSearchConfig::from_secret(Some("bad\nkey")).is_none());
        assert!(BraveSearchConfig::from_secret(Some(&"x".repeat(4_097))).is_none());
        let configured = BraveSearchConfig::from_secret(Some(" secret ")).unwrap();
        assert!(!format!("{configured:?}").contains("secret"));
        assert!(configured.readiness_identity().starts_with("brave:"));
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
        let output =
            project_brave_response(&body, &validated, "server-call-1", "2026-08-28T00:00:00Z")
                .unwrap();
        let value: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["web_search_call_id"], "server-call-1");
        assert_eq!(
            value["connector"]["connector_id"],
            BRAVE_WEB_SEARCH_CONNECTOR_ID
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
