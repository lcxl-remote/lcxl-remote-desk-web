//! Shared bounded public HTTP/HTTPS fetcher for central Web Research.

use futures_util::StreamExt;
use sha2::{Digest, Sha256};

use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::{seam::ToolRunOutput, web_research::ValidatedFetch};

const MAX_REDIRECTS: usize = 3;
use desk_diagnose_core::web_research::MAX_FETCH_RESULT_BYTES as MAX_RESULT_BYTES;

/// Read at most `max_body_bytes` of the page. A longer page is truncated, not
/// rejected, and the result says so.
pub async fn fetch_public_web_page(
    validated: ValidatedFetch,
    max_body_bytes: usize,
) -> Result<ToolRunOutput, AgentError> {
    let requested_url = validated.initial_url().as_str().to_string();
    let mut current = validated.initial_url().clone();
    for redirect_count in 0..=MAX_REDIRECTS {
        let client = pinned_public_client(&current).await?;
        let response = client
            .get(current.as_str())
            .header(
                reqwest::header::ACCEPT,
                "text/html,application/xhtml+xml,text/plain;q=0.9",
            )
            .header(
                reqwest::header::USER_AGENT,
                "lcxl-remote-desk-web-research/1",
            )
            .send()
            .await
            .map_err(|_| transport("Web Research request failed"))?;
        if response.status().is_redirection() {
            if redirect_count == MAX_REDIRECTS {
                return Err(invalid("Web Research redirect limit exceeded"));
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| invalid("Web Research redirect has no valid Location"))?;
            current = validated_redirect(&validated, &current, location)?;
            continue;
        }
        if !response.status().is_success() {
            return Err(
                if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || response.status().is_server_error()
                {
                    transport("Web Research endpoint returned a retryable error")
                } else {
                    invalid("Web Research endpoint rejected the request")
                },
            );
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
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
        // The server's declared length is reported only; it decides nothing.
        let content_length = response.content_length();
        let mut body = BoundedBody::new(max_body_bytes);
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| transport("Web Research response body failed"))?;
            if !body.push(&chunk) {
                break;
            }
        }
        let final_url = current.as_str().to_owned();
        let fetched_at = chrono::Utc::now().to_rfc3339();
        let content = tokio::task::spawn_blocking(move || {
            page_result(PageInput {
                requested_url,
                final_url,
                fetched_at,
                content_type,
                content_length,
                body,
            })
        })
        .await
        .map_err(|_| internal("Web Research extraction failed"))??;
        return Ok(ToolRunOutput {
            format: desk_diagnose_core::seam::ToolOutputFormat::WebDocument,
            content,
            image_data_url: None,
            document_preview: None,
        });
    }
    unreachable!("bounded redirect loop returns or continues")
}

/// Response bytes read up to a limit; the remainder of the stream is dropped.
struct BoundedBody {
    bytes: Vec<u8>,
    limit: usize,
    truncated: bool,
}

impl BoundedBody {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            truncated: false,
        }
    }

    /// Append a chunk; false once the limit is reached and reading should stop.
    fn push(&mut self, chunk: &[u8]) -> bool {
        let room = self.limit.saturating_sub(self.bytes.len());
        if chunk.len() > room {
            self.bytes.extend_from_slice(&chunk[..room]);
            self.truncated = true;
            return false;
        }
        self.bytes.extend_from_slice(chunk);
        true
    }
}

/// UTF-8 text of the read bytes. Only a truncated body may end inside a
/// character; that incomplete tail is dropped. Invalid bytes are rejected.
fn decode_body(bytes: &[u8], truncated: bool) -> Result<&str, AgentError> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(text),
        Err(error) if truncated && error.error_len().is_none() => {
            Ok(std::str::from_utf8(&bytes[..error.valid_up_to()]).expect("valid prefix"))
        }
        Err(_) => Err(invalid("Web Research response is not UTF-8 text")),
    }
}

struct PageInput {
    requested_url: String,
    final_url: String,
    fetched_at: String,
    content_type: String,
    content_length: Option<u64>,
    body: BoundedBody,
}

/// Build the model-facing result, shortening the excerpt so the encoded
/// result stays within the per-call output limit.
fn page_result(input: PageInput) -> Result<String, AgentError> {
    let source = decode_body(&input.body.bytes, input.body.truncated)?;
    let (title, published_at, mut excerpt) = if input.content_type == "text/plain" {
        (None, None, collapse_text(source))
    } else {
        extract_html(source)
    };
    let mut truncated = input.body.truncated;
    loop {
        let mut result = serde_json::json!({
            "schema_version": 1,
            "untrusted_external_content": true,
            "requested_url": input.requested_url,
            "final_url": input.final_url,
            "title": title,
            "published_at": published_at,
            "fetched_at": input.fetched_at,
            "content_type": input.content_type,
            "body_bytes": input.body.bytes.len(),
            // Digest of the bytes actually read, which is a prefix when truncated.
            "sha256": format!("{:x}", Sha256::digest(&input.body.bytes)),
            "truncated": truncated,
            "excerpt": excerpt,
        });
        if let Some(length) = input.content_length {
            result["content_length"] = serde_json::json!(length);
        }
        let encoded = serde_json::to_string(&result)
            .map_err(|_| internal("failed to encode Web Research result"))?;
        let over = encoded.len().saturating_sub(MAX_RESULT_BYTES);
        if over == 0 {
            return Ok(encoded);
        }
        if excerpt.is_empty() {
            return Err(invalid("Web Research result exceeds the output limit"));
        }
        // Escaping can only make the encoded excerpt longer than its text, so
        // removing the excess from the text fits within one more encoding.
        let mut end = excerpt.len().saturating_sub(over);
        while !excerpt.is_char_boundary(end) {
            end -= 1;
        }
        excerpt.truncate(end);
        truncated = true;
    }
}

fn validated_redirect(
    validated: &ValidatedFetch,
    current: &url::Url,
    location: &str,
) -> Result<url::Url, AgentError> {
    let next = current
        .join(location)
        .map_err(|_| invalid("Web Research redirect URL is invalid"))?;
    desk_diagnose_core::web_research::validate_public_url(next.as_str(), true)?;
    if !validated.same_approved_host(&next) {
        return Err(invalid(
            "Web Research redirects may not leave the approved host",
        ));
    }
    Ok(next)
}

async fn pinned_public_client(url: &url::Url) -> Result<reqwest::Client, AgentError> {
    let host = url
        .host_str()
        .ok_or_else(|| invalid("Web Research URL has no host"))?;
    let port = url.port_or_known_default().unwrap_or(443);
    let addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| transport("Web Research DNS lookup failed"))?
        .filter(|address| {
            desk_utils::ssrf::check_transport(address.ip(), false, url.scheme() == "https", false)
                .is_ok()
        })
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err(invalid(
            "Web Research target did not resolve to a permitted public address",
        ));
    }
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(20))
        .resolve_to_addrs(host, &addresses)
        .build()
        .map_err(|_| internal("Web Research client is unavailable"))
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
    let start = lower.find(&format!("<{tag}"))?;
    let body_start = lower[start..].find('>')? + start + 1;
    let end = lower[body_start..].find(&format!("</{tag}>"))? + body_start;
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
        if (tag_lower.contains(&format!("\"{key}\"")) || tag_lower.contains(&format!("'{key}'")))
            && let Some(content) = html_attribute(tag, "content")
        {
            return Some(decode_entities(content).trim().to_string());
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
            if quote == b'"' || quote == b'\'' {
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

fn internal(message: impl Into<String>) -> AgentError {
    error(AgentErrorKind::Internal, message, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(chunks: &[&[u8]], limit: usize) -> BoundedBody {
        let mut body = BoundedBody::new(limit);
        for chunk in chunks {
            if !body.push(chunk) {
                break;
            }
        }
        body
    }

    fn page(content_type: &str, body: BoundedBody) -> serde_json::Value {
        let encoded = page_result(PageInput {
            requested_url: "https://example.com/".into(),
            final_url: "https://example.com/".into(),
            fetched_at: "2026-11-10T00:00:00Z".into(),
            content_type: content_type.into(),
            content_length: Some(9_999_999),
            body,
        })
        .unwrap();
        assert!(encoded.len() <= MAX_RESULT_BYTES);
        serde_json::from_str(&encoded).unwrap()
    }

    #[test]
    fn a_long_page_is_truncated_at_the_limit_instead_of_failing() {
        let whole = read(&[b"hello ", b"world"], 64);
        assert!(!whole.truncated);
        let value = page("text/plain", whole);
        assert_eq!(value["excerpt"], "hello world");
        assert_eq!(value["truncated"], false);
        assert_eq!(value["content_length"], 9_999_999);

        let cut = read(&[b"hello ", b"world", b"never read"], 8);
        assert!(cut.truncated);
        assert_eq!(cut.bytes, b"hello wo");
        let value = page("text/plain", cut);
        assert_eq!(value["excerpt"], "hello wo");
        assert_eq!(value["truncated"], true);
        assert_eq!(value["body_bytes"], 8);
    }

    #[test]
    fn truncation_drops_only_an_incomplete_trailing_character() {
        // "界" is three bytes; the limit splits it.
        let cut = read(&["ab界".as_bytes()], 4);
        assert_eq!(decode_body(&cut.bytes, cut.truncated).unwrap(), "ab");
        // A whole body must be valid UTF-8, and invalid bytes are never trimmed away.
        assert!(decode_body(&cut.bytes, false).is_err());
        assert!(decode_body(b"a\xffb\xe7", true).is_err());
    }

    #[test]
    fn an_oversized_excerpt_is_shortened_to_the_result_limit() {
        let text = "\"quoted\" ".repeat(60_000);
        let body = read(&[text.as_bytes()], 1024 * 1024);
        assert!(!body.truncated);
        let value = page("text/plain", body);
        assert_eq!(value["truncated"], true);
        assert!(value["excerpt"].as_str().unwrap().len() < text.len());
    }

    #[test]
    fn redirects_allow_same_host_protocols_and_ports_but_reject_other_authority() {
        let validated = desk_diagnose_core::web_research::validate_fetch_arguments(
            &desk_diagnose_core::chat::ToolCall {
                id: "fetch".into(),
                name: desk_diagnose_core::web_research::WEB_FETCH_TOOL_NAME.into(),
                arguments_json: r#"{"url":"https://example.com/docs"}"#.into(),
            },
        )
        .unwrap();
        for (location, allowed) in [
            ("/other?q=1", true),
            ("http://example.com:8080/news", true),
            ("https://EXAMPLE.com.:8443/news", true),
            ("https://sub.example.com/", false),
            ("https://example.com.attacker.example/", false),
            ("http://127.0.0.1/", false),
            ("ftp://example.com/", false),
            ("https://user:secret@example.com/", false),
            ("/docs#fragment", false),
        ] {
            assert_eq!(
                validated_redirect(&validated, validated.initial_url(), location).is_ok(),
                allowed,
                "{location}"
            );
        }
    }

    #[tokio::test]
    async fn public_http_and_https_clients_build_requests_and_private_targets_fail() {
        for raw in [
            "http://93.184.216.34/docs",
            "https://93.184.216.34:8443/docs",
        ] {
            let url = desk_diagnose_core::web_research::validate_public_url(raw, true).unwrap();
            let client = pinned_public_client(&url).await.unwrap();
            assert_eq!(client.get(raw).build().unwrap().url(), &url);
        }
        for raw in ["http://127.0.0.1/", "https://10.0.0.1:8443/"] {
            assert!(
                pinned_public_client(&url::Url::parse(raw).unwrap())
                    .await
                    .is_err()
            );
        }
    }

    #[test]
    fn html_projection_keeps_evidence_and_drops_active_blocks() {
        let html = r#"<title>Quarter &amp; Review</title><meta property="article:published_time" content="2026-08-20T10:00:00Z"><style>.secret{}</style><script>ignore()</script><h1>Revenue</h1>"#;
        let (title, published, text) = extract_html(html);
        assert_eq!(title.as_deref(), Some("Quarter & Review"));
        assert_eq!(published.as_deref(), Some("2026-08-20T10:00:00Z"));
        assert!(text.contains("Revenue"));
        assert!(!text.contains("ignore()"));
        assert!(!text.contains(".secret"));
    }
}
