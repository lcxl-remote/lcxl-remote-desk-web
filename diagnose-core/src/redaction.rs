//! Model-agnostic secret redaction shared by both runtimes (security model §9).
//!
//! Before any free text reaches an external model — collected evidence on the
//! edge, or browser-supplied terminal context on the manager — it is scrubbed
//! for credentials and other secrets. Redaction is **server-authoritative** and
//! lives here so the thin edge and the central orchestrator scrub with the exact
//! same pattern set and can never drift: a secret caught on one runtime is caught
//! on the other.
//!
//! Redaction is **fail-closed**: the [`Redactor`] trait returns a `Result`, so a
//! caller that cannot run redaction refuses to send to the model rather than
//! falling back to raw text. Evidence-snapshot walking (which depends on the
//! edge's concrete output types) stays in `server` and consumes this trait.

use std::fmt;

use regex::Regex;

/// The result of redacting one string: the scrubbed text plus one kind tag per
/// redaction occurrence (so the tag count equals the number of secrets removed,
/// matching the `redactions` lists carried by the protocol outputs).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Redacted {
    pub text: String,
    pub kinds: Vec<String>,
}

/// A redactor failed to run. Carries a short, content-free reason for the
/// `ai.redaction.failed` audit event — never the unredacted data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionError {
    pub reason: String,
}

impl fmt::Display for RedactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "redaction failed: {}", self.reason)
    }
}

impl std::error::Error for RedactionError {}

/// Scrubs secrets from free-text strings. Object-safe so callers hold
/// `Arc<dyn Redactor>` and tests can substitute a failing mock to exercise the
/// fail-closed path.
pub trait Redactor: Send + Sync {
    /// Redact a single string. Returns `Err` to force fail-closed.
    fn redact(&self, input: &str) -> Result<Redacted, RedactionError>;
}

/// How a pattern's match is rewritten.
enum Rewrite {
    /// Replace the whole match with `<redacted:kind>`.
    Whole,
    /// Keep capture group 1 (a non-secret prefix) and replace the rest with
    /// `<redacted:kind>`.
    KeepPrefix,
    /// Keep capture groups 1 and 3 around a quoted JSON value while replacing
    /// group 2. This preserves valid JSON instead of dropping the closing quote.
    KeepJsonEdges,
}

struct Pattern {
    kind: &'static str,
    re: Regex,
    rewrite: Rewrite,
}

impl Pattern {
    fn replacement(&self) -> String {
        match self.rewrite {
            Rewrite::Whole => format!("<redacted:{}>", self.kind),
            Rewrite::KeepPrefix => format!("${{1}}<redacted:{}>", self.kind),
            Rewrite::KeepJsonEdges => {
                format!("${{1}}<redacted:{}>${{3}}", self.kind)
            }
        }
    }
}

/// Regex-based redactor implementing the security model §9 first-version list.
///
/// Patterns are applied in a fixed order over the accumulating text; each match
/// is rewritten and counted. The order puts multi-line / highly-specific
/// patterns (PEM blocks, cloud key formats) before the generic
/// `key = value` shapes so a specific secret is tagged with its precise kind.
pub struct RegexRedactor {
    patterns: Vec<Pattern>,
}

impl Default for RegexRedactor {
    fn default() -> Self {
        Self::new()
    }
}

impl RegexRedactor {
    /// Build the redactor with the §9 pattern set. The patterns are static and
    /// known-valid; a construction failure is a programming error (covered by a
    /// unit test), so this panics rather than returning a `Result`.
    pub fn new() -> Self {
        let specs: Vec<(&'static str, &'static str, Rewrite)> = vec![
            // PEM private key block (multi-line). Must run first so its body is
            // not partially matched by the generic value patterns.
            (
                "private_key",
                r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
                Rewrite::Whole,
            ),
            // Cloud provider key formats.
            ("aws_access_key", r"\bAKIA[0-9A-Z]{16}\b", Rewrite::Whole),
            ("gcp_api_key", r"\bAIza[0-9A-Za-z_\-]{35}\b", Rewrite::Whole),
            // HTTP credentials.
            (
                "bearer",
                r"(?i)(bearer\s+)[A-Za-z0-9\-._~+/=]{8,}",
                Rewrite::KeepPrefix,
            ),
            (
                "cookie",
                r"(?i)((?:set-)?cookie:\s*)[^\r\n]+",
                Rewrite::KeepPrefix,
            ),
            // Valid JSON fields need their closing quote preserved. These run
            // before the generic key/value patterns and also handle spaces and
            // escaped characters inside a credential value.
            (
                "api_key",
                r#"(?i)("api[-_]?key"\s*:\s*")((?:\\.|[^"\\])*)(")"#,
                Rewrite::KeepJsonEdges,
            ),
            (
                "token",
                r#"(?i)("(?:access[-_]?|refresh[-_]?)?token"\s*:\s*")((?:\\.|[^"\\])*)(")"#,
                Rewrite::KeepJsonEdges,
            ),
            (
                "password",
                r#"(?i)("(?:password|passwd|pwd)"\s*:\s*")((?:\\.|[^"\\])*)(")"#,
                Rewrite::KeepJsonEdges,
            ),
            (
                "authority_token",
                r#"(?i)("(?:session|lease|approval|capability)[-_]?(?:token|id)"\s*:\s*")((?:\\.|[^"\\])*)(")"#,
                Rewrite::KeepJsonEdges,
            ),
            // Generic `key = value` / `key: value` shapes. `[^\s;"']+` stops at
            // a `;` so connection strings (`...;Password=secret;...`) redact
            // only the value.
            (
                "api_key",
                r#"(?i)(api[-_]?key\s*[=:]\s*["']?)[^\s;"']+"#,
                Rewrite::KeepPrefix,
            ),
            (
                "token",
                r#"(?i)((?:access[-_]?|refresh[-_]?)?token\s*[=:]\s*["']?)[^\s;"']+"#,
                Rewrite::KeepPrefix,
            ),
            (
                "password",
                r#"(?i)((?:password|passwd|pwd)\s*[=:]\s*["']?)[^\s;"']+"#,
                Rewrite::KeepPrefix,
            ),
            (
                "authority_token",
                r#"(?i)((?:session|lease|approval|capability)[-_]?(?:token|id)\s*[=:]\s*["']?)[^\s;"']+"#,
                Rewrite::KeepPrefix,
            ),
            // Windows user directory: keep `C:\Users\`, redact the account name
            // segment (stops at the next path separator).
            (
                "windows_user_dir",
                r#"(?i)([A-Z]:\\Users\\)[^\\/:*?"<>|\r\n]+"#,
                Rewrite::KeepPrefix,
            ),
            // SSH private key paths (`~/.ssh/id_rsa`, `C:\Users\x\.ssh\id_ed25519`).
            (
                "ssh_key_path",
                r"(?i)[\w./\\~-]*[\\/]\.ssh[\\/]id_[a-z0-9_]+",
                Rewrite::Whole,
            ),
        ];

        let patterns = specs
            .into_iter()
            .map(|(kind, src, rewrite)| Pattern {
                kind,
                re: Regex::new(src)
                    .unwrap_or_else(|e| panic!("redaction pattern `{kind}` is invalid: {e}")),
                rewrite,
            })
            .collect();

        Self { patterns }
    }
}

impl Redactor for RegexRedactor {
    fn redact(&self, input: &str) -> Result<Redacted, RedactionError> {
        let mut text = input.to_string();
        let mut kinds = Vec::new();
        for pattern in &self.patterns {
            // Non-overlapping left-to-right matches == replace_all's behaviour,
            // so the count and the rewrite agree.
            let n = pattern.re.find_iter(&text).count();
            if n == 0 {
                continue;
            }
            text = pattern
                .re
                .replace_all(&text, pattern.replacement())
                .into_owned();
            for _ in 0..n {
                kinds.push(pattern.kind.to_string());
            }
        }
        Ok(Redacted { text, kinds })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gcp_key() -> String {
        // AIza + exactly 35 trailing chars.
        format!("AIza{}", "a".repeat(35))
    }

    /// Every §9 secret kind in the corpus is caught (zero false-negatives): the
    /// secret value is gone and the kind tag is recorded.
    #[test]
    fn corpus_redacts_every_secret_kind() {
        let r = RegexRedactor::new();
        let cases: Vec<(&str, &str, &str)> = vec![
            (
                "bearer",
                "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig",
                "eyJhbGciOiJIUzI1NiJ9.payload.sig",
            ),
            (
                "api_key",
                "api_key=sk-1234567890abcdef",
                "sk-1234567890abcdef",
            ),
            (
                "token",
                "access_token: ghp_ABCDEFG1234567890abcdef",
                "ghp_ABCDEFG1234567890abcdef",
            ),
            (
                "password",
                "Server=db;Password=SuperSecret123;Db=x",
                "SuperSecret123",
            ),
            (
                "aws_access_key",
                "key AKIAIOSFODNN7EXAMPLE here",
                "AKIAIOSFODNN7EXAMPLE",
            ),
            (
                "cookie",
                "Cookie: session=abc123; theme=dark",
                "session=abc123",
            ),
            (
                "windows_user_dir",
                r"path C:\Users\alice\AppData\Local",
                r"\alice\",
            ),
            (
                "ssh_key_path",
                "key at /home/bob/.ssh/id_rsa now",
                "/home/bob/.ssh/id_rsa",
            ),
        ];
        for (kind, input, secret) in cases {
            let out = r.redact(input).expect("redact must not fail");
            assert!(
                !out.text.contains(secret),
                "kind `{kind}` left the secret in: {}",
                out.text
            );
            assert!(
                out.kinds.iter().any(|k| k == kind),
                "kind `{kind}` was not tagged; tags = {:?}",
                out.kinds
            );
        }

        // GCP key and PEM block (built separately).
        let gcp = format!("token {} end", gcp_key());
        let out = r.redact(&gcp).expect("redact");
        assert!(!out.text.contains(&gcp_key()));
        assert!(out.kinds.iter().any(|k| k == "gcp_api_key"));

        let pem = "head\n-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKC\nAAA=\n-----END RSA PRIVATE KEY-----\ntail";
        let out = r.redact(pem).expect("redact");
        assert!(!out.text.contains("MIIEpAIBAAKC"));
        assert!(out.text.contains("head"));
        assert!(out.text.contains("tail"));
        assert!(out.kinds.iter().any(|k| k == "private_key"));
    }

    /// A clean string is returned untouched with no redaction tags.
    #[test]
    fn clean_text_is_unchanged() {
        let r = RegexRedactor::new();
        let input = "CPU at 98% on process ffmpeg.exe, 8 logical cores.";
        let out = r.redact(input).expect("redact");
        assert_eq!(out.text, input);
        assert!(out.kinds.is_empty());
    }

    /// Multiple secrets in one string are all redacted and counted (the tag
    /// count equals the number of secrets, not the number of distinct kinds).
    #[test]
    fn multiple_secrets_are_all_counted() {
        let r = RegexRedactor::new();
        let input = "api_key=AAA password=BBB token=CCC";
        let out = r.redact(input).expect("redact");
        assert!(!out.text.contains("AAA"));
        assert!(!out.text.contains("BBB"));
        assert!(!out.text.contains("CCC"));
        assert_eq!(out.kinds.len(), 3);
    }

    #[test]
    fn quoted_json_credentials_are_fully_redacted_and_remain_valid_json() {
        let input = r#"{"api_key":"alpha beta","password":"hunter\"two","approval_id":"approval-secret","safe":"keep"}"#;
        let output = RegexRedactor::new().redact(input).unwrap();
        assert!(!output.text.contains("alpha beta"));
        assert!(!output.text.contains("hunter"));
        assert!(!output.text.contains("approval-secret"));
        assert!(output.text.contains("keep"));
        let parsed: serde_json::Value = serde_json::from_str(&output.text).unwrap();
        assert_eq!(parsed["safe"], "keep");
        assert_eq!(parsed["api_key"], "<redacted:api_key>");
        assert_eq!(parsed["password"], "<redacted:password>");
        assert_eq!(parsed["approval_id"], "<redacted:authority_token>");
    }

    /// The construction of the static pattern set never panics.
    #[test]
    fn redactor_constructs() {
        let _ = RegexRedactor::new();
    }
}
