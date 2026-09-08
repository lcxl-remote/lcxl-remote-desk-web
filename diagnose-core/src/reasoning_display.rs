//! Bounded readable reasoning for reviewed UI presentation, independent of replay.
use serde_json::Value;
const MAX_BYTES: usize = 32 * 1024;

pub fn bounded(text: &str) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    let mut end = text.len().min(MAX_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Some(text[..end].to_string())
}

pub fn anthropic_blocks(blocks: &Value) -> Option<String> {
    let mut text = String::new();
    for block in blocks.as_array()? {
        if block["type"] != "thinking" {
            continue;
        }
        let Some(thinking) = block["thinking"].as_str() else {
            continue;
        };
        if !text.is_empty() {
            text.push('\n');
        }
        if let Some(fragment) = bounded(thinking) {
            text.push_str(&fragment);
        }
        if text.len() >= MAX_BYTES {
            break;
        }
    }
    bounded(&text)
}

pub fn review_text(answer: &str, reasoning: Option<&str>) -> String {
    match reasoning.and_then(bounded) {
        Some(reasoning) => format!("Model reasoning:\n{reasoning}\n\nAssistant answer:\n{answer}"),
        None => answer.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reasoning_display_excludes_signatures_redacted_blocks_and_empty_content() {
        assert_eq!(
            anthropic_blocks(&serde_json::json!([
                {"type":"thinking", "thinking":"visible", "signature":"private-signature"},
                {"type":"redacted_thinking", "data":"private-redacted"},
                {"type":"text", "text":"answer"}
            ]))
            .as_deref(),
            Some("visible")
        );
        assert!(bounded(" \n").is_none());
        assert!(bounded(&"中".repeat(MAX_BYTES)).unwrap().len() <= MAX_BYTES);
        assert!(review_text("answer", Some("visible")).contains("visible"));
        assert_eq!(review_text("answer", None), "answer");
    }
}
