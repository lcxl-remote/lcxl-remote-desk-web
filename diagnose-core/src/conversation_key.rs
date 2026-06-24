//! Server-side derivation of the storage key that identifies an agentic
//! conversation.
//!
//! The control end supplies a *client* conversation id (`DiagnoseRequestData::
//! conversation_id`) purely as continuation intent. The server NEVER uses that
//! value as a storage key directly: a client could otherwise pre-create or
//! collide with another subject's conversation by guessing the id. Instead the
//! server derives a **subject-namespaced** key by hashing the trusted subject
//! (`actor` / `device`, both server-injected) together with the validated client
//! id. Two different subjects supplying the same client id land on different
//! keys, so cross-subject preemption is impossible at the source (the per-session
//! `check_subject` guard remains as defence in depth).
//!
//! An absent / empty / malformed client id falls back to the per-request id, so
//! such a request transparently starts a fresh single-question conversation.

use sha2::{Digest, Sha256};

/// Upper bound on the trimmed client conversation id length. Anything longer is
/// rejected (and falls back to the request id) so a client cannot bloat storage
/// keys with an unbounded string.
pub const MAX_CONVERSATION_ID_LEN: usize = 128;

/// Whether a (already trimmed) client conversation id is well-formed: non-empty,
/// within [`MAX_CONVERSATION_ID_LEN`], and limited to `[A-Za-z0-9_-]`.
pub fn is_valid_client_conversation_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_CONVERSATION_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Append one length-prefixed field (u32 little-endian length + raw bytes) to the
/// hash. The length prefix makes the concatenation unambiguous: `("a","bc")` and
/// `("ab","c")` hash differently, so distinct subjects can never alias.
fn absorb_field(hasher: &mut Sha256, field: &[u8]) {
    hasher.update((field.len() as u32).to_le_bytes());
    hasher.update(field);
}

/// Derive the subject-namespaced storage key for a conversation.
///
/// `client_conversation_id` is the non-authoritative value from the request; it
/// is trimmed and validated, and on failure the `fallback_request_id` is used in
/// its place (yielding a fresh single-question conversation). The subject fields
/// come from the server's trusted context. The result is a lowercase hex
/// SHA-256 digest.
pub fn derive_conversation_key(
    actor_id: &str,
    device_id: &str,
    client_conversation_id: Option<&str>,
    fallback_request_id: &str,
) -> String {
    let client = client_conversation_id
        .map(str::trim)
        .filter(|s| is_valid_client_conversation_id(s))
        .unwrap_or(fallback_request_id);

    let mut hasher = Sha256::new();
    absorb_field(&mut hasher, actor_id.as_bytes());
    absorb_field(&mut hasher, device_id.as_bytes());
    absorb_field(&mut hasher, client.as_bytes());

    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(actor: &str, device: &str, client: Option<&str>) -> String {
        derive_conversation_key(actor, device, client, "req-fallback")
    }

    #[test]
    fn validation_bounds_charset_and_length() {
        assert!(is_valid_client_conversation_id("cv-1_AB"));
        assert!(!is_valid_client_conversation_id(""));
        assert!(!is_valid_client_conversation_id("has space"));
        assert!(!is_valid_client_conversation_id("dots.not.allowed"));
        assert!(!is_valid_client_conversation_id("slash/x"));
        assert!(is_valid_client_conversation_id(
            &"a".repeat(MAX_CONVERSATION_ID_LEN)
        ));
        assert!(!is_valid_client_conversation_id(
            &"a".repeat(MAX_CONVERSATION_ID_LEN + 1)
        ));
    }

    #[test]
    fn same_inputs_are_stable() {
        assert_eq!(key("a", "d", Some("cv-1")), key("a", "d", Some("cv-1")));
        // Hex SHA-256 is 64 chars.
        assert_eq!(key("a", "d", Some("cv-1")).len(), 64);
    }

    #[test]
    fn distinct_subjects_with_same_client_id_do_not_collide() {
        let base = key("actorA", "dev1", Some("shared"));
        assert_ne!(base, key("actorB", "dev1", Some("shared")));
        assert_ne!(base, key("actorA", "dev2", Some("shared")));
    }

    #[test]
    fn length_prefix_prevents_field_boundary_aliasing() {
        // Without length prefixing, ("ab","c") and ("a","bc") could collide.
        assert_ne!(key("ab", "c", Some("cv")), key("a", "bc", Some("cv")));
    }

    #[test]
    fn invalid_or_empty_client_id_falls_back_to_request_id() {
        // Empty, malformed, and absent all collapse onto the fallback request id,
        // which is what a fresh single-question conversation keys on.
        let fallback = key("a", "d", None);
        assert_eq!(key("a", "d", Some("")), fallback);
        assert_eq!(key("a", "d", Some("  ")), fallback);
        assert_eq!(key("a", "d", Some("bad id!")), fallback);
        assert_eq!(
            key("a", "d", Some(&"x".repeat(MAX_CONVERSATION_ID_LEN + 1))),
            fallback,
        );
        // A valid client id does NOT collapse onto the fallback.
        assert_ne!(key("a", "d", Some("cv-1")), fallback);
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_before_use() {
        assert_eq!(key("a", "d", Some("  cv-1  ")), key("a", "d", Some("cv-1")));
    }
}
