//! Bounded central Web Research URL fetcher.
//!
//! This first vertical intentionally fetches only an exact HTTPS URL copied
//! from the owner's current message. It is not a search engine and has no input
//! field capable of uploading local content. Every connect candidate is judged
//! under the strict public-only SSRF policy, including redirects.

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::chat::ToolCall;
use desk_diagnose_core::seam::ToolRunOutput;

use crate::model_dial::SsrfResolver;

pub const WEB_FETCH_TOOL_NAME: &str = "fetch_public_web_page";
pub const WEB_SEARCH_TOOL_NAME: &str = "search_public_web";
const MAX_URL_BYTES: usize = 2_048;
const MAX_BODY_BYTES: usize = 128 * 1024;
const MAX_EXCERPT_CHARS: usize = 24_000;
const MAX_REDIRECTS: usize = 3;
const CONNECT_TIMEOUT_SECS: u64 = 10;
const REQUEST_TIMEOUT_SECS: u64 = 20;
const MAX_QUERY_BYTES: usize = 256;
const MAX_SEARCH_RESULTS: u8 = 8;
const DEFAULT_SEARCH_RESULTS: u8 = 5;
const MAX_SEARCH_BODY_BYTES: usize = 256 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchArgs {
    url: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    #[serde(default = "default_search_results")]
    max_results: u8,
}

const fn default_search_results() -> u8 {
    DEFAULT_SEARCH_RESULTS
}

#[derive(Debug, Clone)]
pub(crate) struct ValidatedSearch {
    query: String,
    max_results: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WebSearchResult {
    title: String,
    url: String,
    snippet: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WebSearchConnectorDescriptor {
    pub connector_id: &'static str,
    pub display_name: &'static str,
    pub requires_api_key: bool,
    pub experimental: bool,
}

#[async_trait(?Send)]
pub(crate) trait WebSearchConnector: Send + Sync {
    fn descriptor(&self) -> WebSearchConnectorDescriptor;

    async fn search(
        &self,
        query: &str,
        max_results: u8,
    ) -> Result<Vec<WebSearchResult>, AgentError>;
}

struct DuckDuckGoHtmlConnector;

impl DuckDuckGoHtmlConnector {
    const SEARCH_ORIGIN: &'static str = "https://html.duckduckgo.com";
}

#[async_trait(?Send)]
impl WebSearchConnector for DuckDuckGoHtmlConnector {
    fn descriptor(&self) -> WebSearchConnectorDescriptor {
        WebSearchConnectorDescriptor {
            connector_id: desk_diagnose_core::device_assistant::DUCKDUCKGO_HTML_CONNECTOR_ID,
            display_name: "DuckDuckGo HTML",
            requires_api_key: false,
            experimental: true,
        }
    }

    async fn search(
        &self,
        query: &str,
        max_results: u8,
    ) -> Result<Vec<WebSearchResult>, AgentError> {
        let mut url = Url::parse(&format!("{}/html/", Self::SEARCH_ORIGIN))
            .expect("compiled DuckDuckGo search origin is valid");
        url.query_pairs_mut().append_pair("q", query);
        let client = tls_client();
        let mut response = client
            .get(url.as_str())
            .insert_header(("Accept", "text/html,application/xhtml+xml"))
            .insert_header(("Accept-Language", "en-US,en;q=0.7"))
            .insert_header(("User-Agent", "lcxl-remote-desk-web-search/1"))
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .send()
            .await
            .map_err(|_| transport("DuckDuckGo HTML search request failed"))?;
        if !response.status().is_success() {
            return Err(transport(format!(
                "DuckDuckGo HTML search returned HTTP {}",
                response.status().as_u16()
            )));
        }
        if response
            .headers()
            .get(awc::http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|length| length > MAX_SEARCH_BODY_BYTES)
        {
            return Err(invalid("Web Search response exceeds the byte limit"));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.next().await {
            let chunk = chunk.map_err(|_| transport("Web Search response body failed"))?;
            if body.len().saturating_add(chunk.len()) > MAX_SEARCH_BODY_BYTES {
                return Err(invalid("Web Search response exceeds the byte limit"));
            }
            body.extend_from_slice(&chunk);
        }
        let html = std::str::from_utf8(&body)
            .map_err(|_| invalid("Web Search response is not UTF-8 HTML"))?;
        let results = parse_duckduckgo_html(html, usize::from(max_results));
        if results.is_empty() {
            return Err(transport(
                "DuckDuckGo HTML search returned no parseable public results",
            ));
        }
        Ok(results)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ValidatedFetch {
    initial_url: Url,
    approved_host: String,
    approved_port: u16,
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

fn transport(message: impl Into<String>) -> AgentError {
    error(AgentErrorKind::TransportError, message, true)
}

/// Validate before grant matching so malformed or non-user-nominated URLs never
/// consume a grant use or create a dispatch intent.
pub(crate) fn validate_fetch_call(
    call: &ToolCall,
    current_user_message: &str,
) -> Result<ValidatedFetch, AgentError> {
    if call.name != WEB_FETCH_TOOL_NAME {
        return Err(invalid("not a Web Research fetch call"));
    }
    let args: FetchArgs = serde_json::from_str(&call.arguments_json)
        .map_err(|decode_error| invalid(format!("invalid Web Research input: {decode_error}")))?;
    if args.url.is_empty()
        || args.url.len() > MAX_URL_BYTES
        || args.url.trim() != args.url
        || !current_user_message.contains(&args.url)
    {
        return Err(invalid(
            "Web Research URL must appear verbatim in the owner's current message",
        ));
    }
    let url = validate_url(&args.url)?;
    let approved_host = url
        .host_str()
        .expect("validated URL has a host")
        .to_ascii_lowercase();
    let approved_port = url
        .port_or_known_default()
        .expect("https has a known default port");
    Ok(ValidatedFetch {
        initial_url: url,
        approved_host,
        approved_port,
    })
}

/// Validate before grant matching so an invented or oversized query never
/// consumes an ExportData grant use or creates a dispatch intent.
pub(crate) fn validate_search_call(
    call: &ToolCall,
    current_user_message: &str,
) -> Result<ValidatedSearch, AgentError> {
    if call.name != WEB_SEARCH_TOOL_NAME {
        return Err(invalid("not a Web Search call"));
    }
    let args: SearchArgs = serde_json::from_str(&call.arguments_json)
        .map_err(|decode_error| invalid(format!("invalid Web Search input: {decode_error}")))?;
    if args.query.is_empty()
        || args.query.len() > MAX_QUERY_BYTES
        || args.query.trim() != args.query
        || !current_user_message.contains(&args.query)
    {
        return Err(invalid(
            "Web Search query must appear verbatim in the owner's current message",
        ));
    }
    if !(1..=MAX_SEARCH_RESULTS).contains(&args.max_results) {
        return Err(invalid("Web Search max_results must be between 1 and 8"));
    }
    Ok(ValidatedSearch {
        query: args.query,
        max_results: args.max_results,
    })
}

pub(crate) async fn search_public_web(
    validated: ValidatedSearch,
) -> Result<ToolRunOutput, AgentError> {
    let connector = DuckDuckGoHtmlConnector;
    search_with_connector(&connector, validated).await
}

async fn search_with_connector(
    connector: &dyn WebSearchConnector,
    validated: ValidatedSearch,
) -> Result<ToolRunOutput, AgentError> {
    let descriptor = connector.descriptor();
    let results = connector
        .search(&validated.query, validated.max_results)
        .await?;
    let result = serde_json::json!({
        "schema_version": 1,
        "untrusted_external_content": true,
        "connector": {
            "connector_id": descriptor.connector_id,
            "display_name": descriptor.display_name,
            "requires_api_key": descriptor.requires_api_key,
            "experimental": descriptor.experimental,
        },
        "query_sha256": format!("{:x}", Sha256::digest(validated.query.as_bytes())),
        "searched_at": chrono::Utc::now().to_rfc3339(),
        "result_count": results.len(),
        "results": results,
    });
    Ok(ToolRunOutput {
        content: serde_json::to_string(&result).map_err(|encode_error| {
            error(
                AgentErrorKind::Internal,
                format!("failed to encode Web Search result: {encode_error}"),
                false,
            )
        })?,
        image_data_url: None,
    })
}

fn validate_url(raw: &str) -> Result<Url, AgentError> {
    let url = Url::parse(raw).map_err(|_| invalid("Web Research URL is invalid"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !matches!(url.port(), None | Some(443))
    {
        return Err(invalid(
            "Web Research accepts only public HTTPS URLs without credentials, fragments, or non-443 ports",
        ));
    }
    // IP literals bypass actix-tls's custom resolver, so judge them here. DNS
    // names are authoritatively judged per candidate by SsrfResolver at dial.
    desk_utils::ssrf::check_transport_for_url(raw, false, true, true)
        .map_err(|_| invalid("Web Research target is not a permitted public HTTPS address"))?;
    Ok(url)
}

fn same_approved_origin(url: &Url, validated: &ValidatedFetch) -> bool {
    url.host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case(&validated.approved_host))
        && url.port_or_known_default() == Some(validated.approved_port)
}

fn tls_client() -> awc::Client {
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        let _ = roots.add(cert);
    }
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring supports default TLS versions")
    .with_root_certificates(std::sync::Arc::new(roots))
    .with_no_client_auth();
    let tcp = actix_tls::connect::Connector::new(actix_tls::connect::Resolver::custom(
        SsrfResolver::strict_public_https(),
    ))
    .service();
    awc::Client::builder()
        .connector(
            awc::Connector::new()
                .connector(tcp)
                .timeout(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
                .rustls_0_23(std::sync::Arc::new(tls)),
        )
        .finish()
}

pub(crate) async fn fetch_public_web_page(
    validated: ValidatedFetch,
) -> Result<ToolRunOutput, AgentError> {
    let client = tls_client();
    let requested_url = validated.initial_url.as_str().to_string();
    let mut current = validated.initial_url.clone();

    for redirect_count in 0..=MAX_REDIRECTS {
        let mut response = client
            .get(current.as_str())
            .insert_header(("Accept", "text/html,application/xhtml+xml,text/plain;q=0.9"))
            .insert_header(("User-Agent", "lcxl-remote-desk-web-research/1"))
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .send()
            .await
            .map_err(|_| transport("Web Research HTTPS request failed"))?;

        if response.status().is_redirection() {
            if redirect_count == MAX_REDIRECTS {
                return Err(invalid("Web Research redirect limit exceeded"));
            }
            let location = response
                .headers()
                .get(awc::http::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| invalid("Web Research redirect has no valid Location"))?;
            let next = current
                .join(location)
                .map_err(|_| invalid("Web Research redirect URL is invalid"))?;
            validate_url(next.as_str())?;
            if !same_approved_origin(&next, &validated) {
                return Err(invalid(
                    "Web Research redirects may not leave the approved host",
                ));
            }
            current = next;
            continue;
        }

        if !response.status().is_success() {
            return Err(transport(format!(
                "Web Research endpoint returned HTTP {}",
                response.status().as_u16()
            )));
        }
        let content_type = response
            .headers()
            .get(awc::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if !matches!(
            content_type.as_str(),
            "text/html" | "text/plain" | "application/xhtml+xml"
        ) {
            return Err(invalid(
                "Web Research response is not supported textual content",
            ));
        }
        if response
            .headers()
            .get(awc::http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|length| length > MAX_BODY_BYTES)
        {
            return Err(invalid("Web Research response exceeds the byte limit"));
        }

        let mut body = Vec::new();
        while let Some(chunk) = response.next().await {
            let chunk = chunk.map_err(|_| transport("Web Research response body failed"))?;
            if body.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
                return Err(invalid("Web Research response exceeds the byte limit"));
            }
            body.extend_from_slice(&chunk);
        }
        let source = std::str::from_utf8(&body)
            .map_err(|_| invalid("Web Research response is not UTF-8 text"))?;
        let (title, published_at, excerpt) = if content_type == "text/plain" {
            (None, None, collapse_text(source))
        } else {
            extract_html(source)
        };
        let fetched_at = chrono::Utc::now().to_rfc3339();
        let result = serde_json::json!({
            "schema_version": 1,
            "untrusted_external_content": true,
            "requested_url": requested_url,
            "final_url": current.as_str(),
            "title": title,
            "published_at": published_at,
            "fetched_at": fetched_at,
            "content_type": content_type,
            "body_bytes": body.len(),
            "sha256": format!("{:x}", Sha256::digest(&body)),
            "excerpt": excerpt.chars().take(MAX_EXCERPT_CHARS).collect::<String>(),
        });
        return Ok(ToolRunOutput {
            content: serde_json::to_string(&result).map_err(|encode_error| {
                error(
                    AgentErrorKind::Internal,
                    format!("failed to encode Web Research result: {encode_error}"),
                    false,
                )
            })?,
            image_data_url: None,
        });
    }
    unreachable!("bounded redirect loop returns or continues")
}

fn extract_html(source: &str) -> (Option<String>, Option<String>, String) {
    let title = extract_element_text(source, "title").filter(|value| !value.is_empty());
    let published = extract_meta_content(source, "article:published_time")
        .or_else(|| extract_meta_content(source, "datePublished"))
        .or_else(|| extract_meta_content(source, "date"));
    let without_script = remove_element_blocks(source, "script");
    let without_style = remove_element_blocks(&without_script, "style");
    (title, published, collapse_text(&strip_tags(&without_style)))
}

fn extract_element_text(source: &str, tag: &str) -> Option<String> {
    let lower = source.to_ascii_lowercase();
    let open = format!("<{tag}");
    let start = lower.find(&open)?;
    let body_start = lower[start..].find('>')? + start + 1;
    let close = format!("</{tag}>");
    let end = lower[body_start..].find(&close)? + body_start;
    Some(collapse_text(&decode_entities(&strip_tags(
        &source[body_start..end],
    ))))
}

fn extract_meta_content(source: &str, key: &str) -> Option<String> {
    let lower = source.to_ascii_lowercase();
    let key = key.to_ascii_lowercase();
    let mut offset = 0;
    while let Some(relative) = lower[offset..].find("<meta") {
        let start = offset + relative;
        let end = lower[start..].find('>')? + start + 1;
        let tag = &source[start..end];
        let tag_lower = &lower[start..end];
        if tag_lower.contains(&format!("\"{key}\"")) || tag_lower.contains(&format!("'{key}'")) {
            if let Some(content) = html_attribute(tag, "content") {
                return Some(decode_entities(content).trim().to_string());
            }
        }
        offset = end;
    }
    None
}

fn html_attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let lower = tag.to_ascii_lowercase();
    let mut offset = 0;
    while let Some(relative) = lower[offset..].find(name) {
        let start = offset + relative;
        let before_ok = start == 0 || !lower.as_bytes()[start - 1].is_ascii_alphanumeric();
        let mut cursor = start + name.len();
        while lower
            .as_bytes()
            .get(cursor)
            .is_some_and(u8::is_ascii_whitespace)
        {
            cursor += 1;
        }
        if before_ok && lower.as_bytes().get(cursor) == Some(&b'=') {
            cursor += 1;
            while lower
                .as_bytes()
                .get(cursor)
                .is_some_and(u8::is_ascii_whitespace)
            {
                cursor += 1;
            }
            let quote = *tag.as_bytes().get(cursor)?;
            if quote == b'\"' || quote == b'\'' {
                let value_start = cursor + 1;
                let value_end = tag.as_bytes()[value_start..]
                    .iter()
                    .position(|byte| *byte == quote)?
                    + value_start;
                return Some(&tag[value_start..value_end]);
            }
        }
        offset = cursor.max(start + 1);
    }
    None
}

fn remove_element_blocks(source: &str, tag: &str) -> String {
    let mut output = source.to_string();
    loop {
        let lower = output.to_ascii_lowercase();
        let Some(start) = lower.find(&format!("<{tag}")) else {
            return output;
        };
        let Some(relative_end) = lower[start..].find(&format!("</{tag}>")) else {
            output.truncate(start);
            return output;
        };
        let end = start + relative_end + tag.len() + 3;
        output.replace_range(start..end, " ");
    }
}

fn strip_tags(source: &str) -> String {
    let mut output = String::with_capacity(source.len());
    let mut in_tag = false;
    for ch in source.chars() {
        match ch {
            '<' => {
                in_tag = true;
                output.push(' ');
            }
            '>' => {
                in_tag = false;
                output.push(' ');
            }
            _ if !in_tag => output.push(ch),
            _ => {}
        }
    }
    decode_entities(&output)
}

fn decode_entities(source: &str) -> String {
    source
        .replace("&nbsp;", " ")
        .replace("&#160;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn collapse_text(source: &str) -> String {
    source.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_duckduckgo_html(source: &str, max_results: usize) -> Vec<WebSearchResult> {
    let lower = source.to_ascii_lowercase();
    let mut cursor = 0;
    let mut results = Vec::new();
    while results.len() < max_results {
        let Some(relative) = lower[cursor..].find("result__a") else {
            break;
        };
        let class_position = cursor + relative;
        let Some(tag_start) = lower[..class_position].rfind("<a") else {
            cursor = class_position + "result__a".len();
            continue;
        };
        let Some(relative_tag_end) = lower[class_position..].find('>') else {
            break;
        };
        let tag_end = class_position + relative_tag_end + 1;
        let tag = &source[tag_start..tag_end];
        let Some(href) = html_attribute(tag, "href") else {
            cursor = tag_end;
            continue;
        };
        let Some(relative_close) = lower[tag_end..].find("</a>") else {
            break;
        };
        let close = tag_end + relative_close;
        let title = collapse_text(&strip_tags(&source[tag_end..close]));
        let next_result = lower[close..]
            .find("result__a")
            .map_or(source.len(), |relative| close + relative);
        let snippet = lower[close..next_result]
            .find("result__snippet")
            .and_then(|relative| {
                let class_position = close + relative;
                let body_start = lower[class_position..].find('>')? + class_position + 1;
                let body_end = lower[body_start..next_result]
                    .find("</a>")
                    .or_else(|| lower[body_start..next_result].find("</div>"))?
                    + body_start;
                Some(collapse_text(&strip_tags(&source[body_start..body_end])))
            })
            .unwrap_or_default();
        if let Some(url) = duckduckgo_result_url(href) {
            if !title.is_empty() && !results.iter().any(|item: &WebSearchResult| item.url == url) {
                results.push(WebSearchResult {
                    title,
                    url,
                    snippet: snippet.chars().take(1_000).collect(),
                });
            }
        }
        cursor = close + 4;
    }
    results
}

fn duckduckgo_result_url(href: &str) -> Option<String> {
    let decoded = decode_entities(href);
    let candidate = if decoded.starts_with("//") {
        format!("https:{decoded}")
    } else {
        decoded
    };
    let parsed = Url::parse(&candidate).ok()?;
    let target = if parsed
        .host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case("duckduckgo.com"))
    {
        parsed
            .query_pairs()
            .find_map(|(key, value)| (key == "uddg").then(|| value.into_owned()))?
    } else {
        candidate
    };
    validate_url(&target).ok()?;
    Some(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(url: &str) -> ToolCall {
        ToolCall {
            id: "web-1".into(),
            name: WEB_FETCH_TOOL_NAME.into(),
            arguments_json: serde_json::json!({"url": url}).to_string(),
        }
    }

    #[test]
    fn exact_owner_url_and_public_https_shape_are_required() {
        let good = "https://example.com/report?q=1";
        assert!(validate_fetch_call(&call(good), &format!("请读取 {good}")).is_ok());
        assert!(validate_fetch_call(&call(good), "读取刚才的网址").is_err());
        assert!(validate_fetch_call(&call("http://example.com"), "http://example.com").is_err());
        assert!(validate_fetch_call(&call("https://127.0.0.1"), "https://127.0.0.1").is_err());
        assert!(
            validate_fetch_call(&call("https://example.com/#x"), "https://example.com/#x").is_err()
        );
    }

    #[test]
    fn html_projection_keeps_evidence_text_and_drops_active_blocks() {
        let html = r#"<html><head><title>Quarter &amp; Review</title><meta property="article:published_time" content="2026-08-20T10:00:00Z"><style>.secret{}</style><script>ignore()</script></head><body><h1>Revenue</h1><p>North 120</p></body></html>"#;
        let (title, published, text) = extract_html(html);
        assert_eq!(title.as_deref(), Some("Quarter & Review"));
        assert_eq!(published.as_deref(), Some("2026-08-20T10:00:00Z"));
        assert!(text.contains("Revenue"));
        assert!(text.contains("North 120"));
        assert!(!text.contains("ignore()"));
        assert!(!text.contains(".secret"));
    }

    #[test]
    fn exact_owner_query_and_bounded_result_count_are_required() {
        let call = ToolCall {
            id: "search-1".into(),
            name: WEB_SEARCH_TOOL_NAME.into(),
            arguments_json: serde_json::json!({"query": "Rust language", "max_results": 5})
                .to_string(),
        };
        assert!(validate_search_call(&call, "请搜索 Rust language").is_ok());
        assert!(validate_search_call(&call, "请搜索刚才的关键词").is_err());
        let too_many = ToolCall {
            arguments_json: serde_json::json!({"query": "Rust language", "max_results": 9})
                .to_string(),
            ..call
        };
        assert!(validate_search_call(&too_many, "Rust language").is_err());
    }

    #[test]
    fn duckduckgo_html_projection_unwraps_results_and_keeps_snippets() {
        let html = r#"
        <div class="result">
          <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F&amp;rut=x">Rust Language</a>
          <a class="result__snippet">A language empowering everyone.</a>
        </div>
        <div class="result">
          <a class="result__a" href="https://example.com/docs">Example Docs</a>
          <div class="result__snippet">Bounded &amp; public.</div>
        </div>"#;
        let results = parse_duckduckgo_html(html, 5);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert_eq!(results[0].snippet, "A language empowering everyone.");
        assert_eq!(results[1].url, "https://example.com/docs");
        assert_eq!(results[1].snippet, "Bounded & public.");
    }
}
