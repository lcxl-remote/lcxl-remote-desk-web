//! Classify provider HTTP failures before a long-running goal chooses whether
//! to wait and retry. A rejected request cannot be treated as an outage.

use desk_agent_protocol::{AgentError, AgentErrorKind};

pub fn from_status(status: u16, message: impl Into<String>) -> AgentError {
    let retryable =
        status == 408 || status == 425 || status == 429 || (500..=599).contains(&status);
    let kind = if retryable {
        AgentErrorKind::ModelUnavailable
    } else {
        AgentErrorKind::ModelRejected
    };
    AgentError {
        kind,
        message: message.into(),
        retryable,
        safe_for_model: true,
        error_code: None,
    }
}

/// HTTP Retry-After is either decimal seconds or an HTTP date. The caller
/// bounds this absolute hint by the goal deadline; provider body text is never
/// accepted as a scheduling hint.
pub fn retry_after_unix_ms(raw: &str, now_unix_ms: u64) -> Option<u64> {
    let raw = raw.trim();
    if !raw.is_empty() && raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return raw
            .parse::<u64>()
            .ok()
            .map(|seconds| now_unix_ms.saturating_add(seconds.saturating_mul(1_000)));
    }
    let deadline = chrono::DateTime::parse_from_rfc2822(raw).ok()?;
    u64::try_from(deadline.timestamp_millis()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_transient_provider_statuses_are_retryable() {
        for status in [408, 425, 429, 500, 502, 503, 504] {
            let error = from_status(status, "provider unavailable");
            assert_eq!(error.kind, AgentErrorKind::ModelUnavailable);
            assert!(error.retryable);
        }
        for status in [400, 404, 413, 422] {
            let error = from_status(status, "invalid request");
            assert_eq!(error.kind, AgentErrorKind::ModelRejected);
            assert!(!error.retryable);
        }
        for status in [401, 403] {
            let error = from_status(status, "authorization rejected");
            assert_eq!(error.kind, AgentErrorKind::ModelRejected);
            assert!(!error.retryable);
        }
    }

    #[test]
    fn retry_after_parses_header_not_provider_text() {
        assert_eq!(retry_after_unix_ms("120", 1_000), Some(121_000));
        assert_eq!(
            retry_after_unix_ms("Wed, 21 Oct 2015 07:28:00 GMT", 0),
            Some(1_445_412_480_000)
        );
        assert_eq!(retry_after_unix_ms("a long error body", 1_000), None);
    }
}
