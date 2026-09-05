//! Bounded parsing of the official non-JavaScript search page.

use super::{AgentError, AgentErrorKind, BraveResult, error};
use kuchikiki::traits::TendrilSink;

fn malformed() -> AgentError {
    error(
        AgentErrorKind::TransportError,
        "DuckDuckGo response structure is not recognized",
        false,
    )
}

pub(super) fn parse(body: &[u8]) -> Result<Vec<BraveResult>, AgentError> {
    let source = std::str::from_utf8(body).map_err(|_| malformed())?;
    let document = kuchikiki::parse_html().one(source).document_node;
    if document
        .select_first("#challenge-form, #anomaly-form, .anomaly-modal, form[action*='anomaly.js']")
        .is_ok()
    {
        return Err(error(
            AgentErrorKind::TransportError,
            "DuckDuckGo requires human verification",
            false,
        ));
    }
    let mut results = Vec::new();
    for result in document.select(".result").map_err(|_| malformed())? {
        // Advertisements are not organic search results.
        if result
            .attributes
            .borrow()
            .get("class")
            .is_some_and(|class| class.split_whitespace().any(|value| value == "result--ad"))
        {
            continue;
        }
        let Ok(link) = result.as_node().select_first("a.result__a") else {
            continue;
        };
        let raw = link
            .attributes
            .borrow()
            .get("href")
            .map(str::to_owned)
            .ok_or_else(malformed)?;
        let url = result_url(&raw).unwrap_or_default();
        let description = result
            .as_node()
            .select_first(".result__snippet")
            .map(|snippet| snippet.text_contents())
            .unwrap_or_default();
        results.push(BraveResult {
            title: link.text_contents(),
            url,
            description,
            page_age: None,
        });
    }
    if results.is_empty()
        && document
            .select_first(".no-results, .no-results__message")
            .is_err()
    {
        return Err(malformed());
    }
    Ok(results)
}

fn result_url(raw: &str) -> Option<String> {
    let base = url::Url::parse("https://html.duckduckgo.com/").ok()?;
    let url = base.join(raw).ok()?;
    if matches!(
        url.host_str(),
        Some("duckduckgo.com" | "html.duckduckgo.com")
    ) {
        if url.path() != "/l/" {
            return None;
        }
        return url
            .query_pairs()
            .find(|(key, _)| key == "uddg")
            .map(|(_, value)| value.into_owned());
    }
    Some(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_entities_wrapped_links_and_skips_ads() {
        let body = br#"<html><div class="result result--ad"><a class="result__a" href="https://example.com/ad">ad</a></div><div class="result"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa&amp;x=1">A &amp; B</a><a class="result__snippet">&#20320;&#22909;</a></div></html>"#;
        let results = parse(body).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://example.com/a");
        assert_eq!(results[0].title, "A & B");
        assert_eq!(results[0].description, "你好");
    }

    #[test]
    fn distinguishes_empty_results_from_challenge_and_changed_markup() {
        assert!(
            parse(br#"<div class="no-results">No results</div>"#)
                .unwrap()
                .is_empty()
        );
        assert!(parse(br#"<div class="unknown-results">changed</div>"#).is_err());
        let challenge =
            parse(br#"<form id="challenge-form"></form><div class="no-results"></div>"#)
                .unwrap_err();
        assert!(!challenge.retryable);
        assert!(challenge.message.contains("verification"));
        assert!(parse(&[0xff]).is_err());
    }
}
