//! Credential checks shared by candidate, canonical-value and evidence storage.
//! Rejected values never appear in errors, logs or a replacement memory body.

use crate::redaction::{Redactor, RegexRedactor};
use regex::Regex;
use std::sync::LazyLock;

static REDACTOR: LazyLock<RegexRedactor> = LazyLock::new(RegexRedactor::new);
static LOCALIZED_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r#"(?:密码|密碼|口令|验证码|驗證碼|一次性密码|一次性密碼|密钥|密鑰|私钥|私鑰|令牌)\s*(?:是|为|為|[:=：])\s*["']?[^\s;；,"'}]+"#
).expect("static memory credential labels")
});

pub fn validate(text: &str) -> Result<(), &'static str> {
    if text
        .chars()
        .any(|value| value.is_control() && !matches!(value, '\n' | '\r' | '\t'))
    {
        return Err("unsupported memory content");
    }
    let checked = REDACTOR
        .redact(text)
        .map_err(|_| "memory credential filter unavailable")?;
    if !checked.kinds.is_empty() || checked.text != text || LOCALIZED_ASSIGNMENT.is_match(text) {
        return Err("unsupported memory content");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_rejected_in_all_retained_text_without_revealing_the_value() {
        for value in [
            "password=synthetic-test-value",
            "验证码是 123456",
            "私鑰：synthetic-test-value",
            "令牌为 synthetic-test-value",
            "{\"password\":\"synthetic-test-value\"}",
            "API密钥=synthetic-test-value",
        ] {
            let error = validate(value).unwrap_err();
            assert_eq!(error, "unsupported memory content");
            assert!(!error.contains("synthetic-test-value"));
        }
    }

    #[test]
    fn normal_preferences_paths_and_credential_handling_rules_remain_valid() {
        for value in [
            "默认用中文回复",
            "Never store passwords",
            "请不要保存密码",
            "token_limit 使用 4096",
            "/Projects/Repo",
            "C:\\Projects\\Repo",
        ] {
            validate(value).unwrap();
        }
    }

    #[test]
    fn retained_text_rejects_native_control_bytes_but_preserves_normal_multiline_text() {
        for value in ["preference\0hidden", "context\u{1b}[31m", "quote\u{7}"] {
            assert_eq!(validate(value).unwrap_err(), "unsupported memory content");
        }
        validate("项目背景\n\t工作目录 /Projects/Repo\r\n默认中文").unwrap();
    }
}
