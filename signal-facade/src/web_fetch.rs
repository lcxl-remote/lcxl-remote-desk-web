//! Shared bounded public-HTTPS fetcher for central Web Research.

use futures_util::StreamExt;
use sha2::{Digest, Sha256};

use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::{seam::ToolRunOutput, web_research::ValidatedFetch};

const MAX_BODY_BYTES: usize = 128 * 1024;
const MAX_EXCERPT_CHARS: usize = 24_000;
const MAX_REDIRECTS: usize = 3;

pub async fn fetch_public_web_page(validated: ValidatedFetch) -> Result<ToolRunOutput, AgentError> {
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
            .map_err(|_| transport("Web Research HTTPS request failed"))?;
        if response.status().is_redirection() {
            if redirect_count == MAX_REDIRECTS {
                return Err(invalid("Web Research redirect limit exceeded"));
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| invalid("Web Research redirect has no valid Location"))?;
            let next = current
                .join(location)
                .map_err(|_| invalid("Web Research redirect URL is invalid"))?;
            desk_diagnose_core::web_research::validate_public_url(next.as_str(), true)?;
            if !validated.same_approved_origin(&next) {
                return Err(invalid(
                    "Web Research redirects may not leave the approved host",
                ));
            }
            current = next;
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
        if response
            .content_length()
            .is_some_and(|length| length > MAX_BODY_BYTES as u64)
        {
            return Err(invalid("Web Research response exceeds the byte limit"));
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
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
        let result = serde_json::json!({
            "schema_version": 1,
            "untrusted_external_content": true,
            "requested_url": requested_url,
            "final_url": current.as_str(),
            "title": title,
            "published_at": published_at,
            "fetched_at": chrono::Utc::now().to_rfc3339(),
            "content_type": content_type,
            "body_bytes": body.len(),
            "sha256": format!("{:x}", Sha256::digest(&body)),
            "excerpt": excerpt.chars().take(MAX_EXCERPT_CHARS).collect::<String>(),
        });
        return Ok(ToolRunOutput {
            content: serde_json::to_string(&result)
                .map_err(|_| internal("failed to encode Web Research result"))?,
            image_data_url: None,
        });
    }
    unreachable!("bounded redirect loop returns or continues")
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
            desk_utils::ssrf::check_transport(address.ip(), false, true, true).is_ok()
        })
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err(invalid(
            "Web Research target did not resolve to a permitted public address",
        ));
    }
    reqwest::Client::builder()
        .https_only(true)
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
