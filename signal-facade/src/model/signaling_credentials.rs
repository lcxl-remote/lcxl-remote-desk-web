//! Credential extraction must not depend on optional peer metadata decoding.
use std::borrow::Cow;

const INVALID: &str = "Invalid signaling credentials";

/// Decode a form component without silently replacing malformed UTF-8 or `%`
/// escapes. Most metadata keys and credentials can borrow the original slice.
fn component(raw: &str) -> Result<Cow<'_, str>, &'static str> {
    if !raw.as_bytes().contains(&b'%') && !raw.as_bytes().contains(&b'+') {
        return Ok(Cow::Borrowed(raw));
    }
    let raw = raw.as_bytes();
    let mut decoded = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        match raw[index] {
            b'%' => {
                let high = raw
                    .get(index + 1)
                    .and_then(|b| (*b as char).to_digit(16))
                    .ok_or(INVALID)?;
                let low = raw
                    .get(index + 2)
                    .and_then(|b| (*b as char).to_digit(16))
                    .ok_or(INVALID)?;
                decoded.push((high * 16 + low) as u8);
                index += 3;
            }
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded)
        .map(Cow::Owned)
        .map_err(|_| INVALID)
}

/// Extract exactly one nonempty node credential. An explicitly malformed
/// credential is never equivalent to an absent credential. No error includes
/// query data; unrelated metadata values are decoded only by their extractor.
pub fn node_token(query: &str) -> Result<Option<Cow<'_, str>>, &'static str> {
    let mut token = None;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "token" || (key.as_bytes().contains(&b'%') && component(key)? == "token") {
            let value = component(value)?;
            if token.is_some() || value.is_empty() || value.chars().any(char::is_control) {
                return Err(INVALID);
            }
            token = Some(value);
        }
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_independent_of_version_fields() {
        assert_eq!(
            node_token("api_version=invalid&token=a%2Bb").unwrap(),
            Some("a+b".into())
        );
        assert_eq!(node_token("remote_desk_type=invalid").unwrap(), None);
        assert_eq!(node_token("token=a+b").unwrap(), Some("a b".into()));
        assert_eq!(
            node_token("token=%E5%AF%86%E9%92%A5").unwrap(),
            Some("密钥".into())
        );
    }

    #[test]
    fn explicit_invalid_credentials_never_become_absent() {
        for query in [
            "token=",
            "token",
            "token=a&token=b",
            "token=a&to%6ben=a",
            "token=%",
            "token=%GG",
            "token=%FF",
            "token=%00",
            "token=a&to%6Ken=b",
        ] {
            assert!(node_token(query).is_err(), "{query}");
        }
    }
}
