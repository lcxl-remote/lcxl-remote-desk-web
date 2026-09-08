//! Token accounting for normalized, trusted terminal provider usage.
use crate::chat::TokenUsage;

/// Conservative reservation for a text-only, fully rendered provider request.
/// Include schemas, JSON escaping and per-frame provider formatting rather than
/// assuming four characters per token. Provider receipts settle the reservation;
/// an unexpected overrun remains charged and prevents further task dispatch.
pub fn text_request_reservation(body: &serde_json::Value, output_limit: u64) -> Option<u64> {
    if output_limit == 0 {
        return None;
    }
    let frames = ["messages", "tools"]
        .into_iter()
        .try_fold(1u64, |sum, key| {
            sum.checked_add(
                u64::try_from(
                    body.get(key)
                        .and_then(serde_json::Value::as_array)
                        .map_or(0, Vec::len),
                )
                .ok()?,
            )
        })?;
    u64::try_from(serde_json::to_vec(body).ok()?.len())
        .ok()?
        .checked_add(frames.checked_mul(1024)?)?
        .checked_add(output_limit)
}

/// Scheduling reservation policy, not a claim about a provider's vision tokenizer.
/// Encoded pixels are not text tokens. Reserve a fixed allowance per validated
/// image; retain all other rendered bytes and framing in the text reservation.
/// Actual terminal usage settles this allowance, and overruns stop further calls.
pub fn image_request_reservation<'a>(
    body: &serde_json::Value,
    output_limit: u64,
    urls: impl IntoIterator<Item = &'a str>,
) -> Option<u64> {
    const IMAGE_TOKEN_ALLOWANCE: u64 = 32_768;
    let urls: Vec<_> = urls.into_iter().collect();
    let images = crate::image_input::validate_image_request(urls.iter().copied()).ok()?;
    let rendered = serde_json::to_string(body).ok()?;
    let encoded_bytes = urls.iter().try_fold(0u64, |total, url| {
        let (_, payload) = url.split_once(',')?;
        // Only subtract actual rendered pixel payloads. Provider formatting and
        // unknown image representations remain fully charged as rendered bytes.
        if !rendered.contains(payload) {
            return None;
        }
        total.checked_add(u64::try_from(payload.len()).ok()?)
    })?;
    text_request_reservation(body, output_limit)?
        .checked_sub(encoded_bytes)?
        .checked_add(
            u64::try_from(images.len())
                .ok()?
                .checked_mul(IMAGE_TOKEN_ALLOWANCE)?,
        )
}

/// Missing primary usage, negative counters or an unrepresentable ledger total
/// leave the original reservation charged. Cached counters are disjoint from
/// input_tokens in the shared adapter contract; omitted cache classes count zero.
pub fn terminal_token_units(usage: &TokenUsage) -> Option<u64> {
    [
        usage.input_tokens?,
        usage.output_tokens?,
        usage.cache_read_tokens.unwrap_or(0),
        usage.cache_write_tokens.unwrap_or(0),
    ]
    .into_iter()
    .try_fold(0i64, |sum, value| {
        if value < 0 {
            None
        } else {
            sum.checked_add(value)
        }
    })
    .and_then(|sum| u64::try_from(sum).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn text_reservation_counts_rendered_request_and_rejects_overflow() {
        let body =
            serde_json::json!({"messages":[{"content":"中\\\"文"}], "tools":[{"name":"read"}]});
        let bytes = serde_json::to_vec(&body).unwrap().len() as u64;
        assert_eq!(
            text_request_reservation(&body, 200),
            Some(bytes + 3072 + 200)
        );
        assert_eq!(text_request_reservation(&body, 0), None);
        assert_eq!(text_request_reservation(&body, u64::MAX), None);
    }
    #[test]
    fn terminal_usage_counts_disjoint_classes_and_keeps_unknown_reserved() {
        let usage = TokenUsage {
            input_tokens: Some(70),
            output_tokens: Some(20),
            cache_read_tokens: Some(30),
            cache_write_tokens: None,
        };
        assert_eq!(terminal_token_units(&usage), Some(120));
        assert_eq!(
            terminal_token_units(&TokenUsage {
                input_tokens: Some(25),
                output_tokens: Some(7),
                cache_read_tokens: Some(40),
                cache_write_tokens: Some(12)
            }),
            Some(84)
        );
        assert_eq!(
            terminal_token_units(&TokenUsage {
                input_tokens: Some(0),
                output_tokens: Some(0),
                ..Default::default()
            }),
            Some(0)
        );
        for invalid in [
            TokenUsage::default(),
            TokenUsage {
                input_tokens: None,
                ..usage
            },
            TokenUsage {
                output_tokens: None,
                ..usage
            },
            TokenUsage {
                cache_read_tokens: Some(-1),
                ..usage
            },
            TokenUsage {
                input_tokens: Some(i64::MAX),
                ..usage
            },
        ] {
            assert_eq!(terminal_token_units(&invalid), None);
        }
    }
}
