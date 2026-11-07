//! Server-side derivation of conversation storage keys. A client continuation
//! intent is never used as a storage key directly. The key includes the trusted
//! subject, and storage still verifies that subject before accessing the row.

use sha2::{Digest, Sha256};

/// Upper bound on the trimmed client conversation id length. Anything longer is
/// rejected (and falls back to the request id) so a client cannot bloat storage
/// keys with an unbounded string.
pub const MAX_CONVERSATION_ID_LEN: usize = 128;

/// The access context in which the authenticated actor owns the conversation.
/// The host resolves this before admission. Device access is checked separately
/// for each request and action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssistantConversationScope {
    Personal,
    Organization(i32),
}

impl AssistantConversationScope {
    pub fn object_context_key(
        self,
        actor_id: &str,
        device_id: &str,
        update: &desk_agent_protocol::ai_assistant::AiAssistantObjectContextUpdate,
    ) -> Result<String, &'static str> {
        if !is_valid_client_conversation_id(&update.conversation_id) {
            return Err("invalid AI Assistant conversation id");
        }
        if update.conversation_target.is_selectable() {
            Ok(derive_assistant_conversation_key(
                actor_id,
                self,
                &update.conversation_id,
            ))
        } else {
            Ok(derive_conversation_key(
                actor_id,
                device_id,
                Some(&update.conversation_id),
                "",
            ))
        }
    }
}

/// Derive an assistant conversation key from the authenticated actor, access
/// context and validated client continuation intent. Device selection is per request.
pub fn derive_assistant_conversation_key(
    actor_id: &str,
    scope: AssistantConversationScope,
    conversation_id: &str,
) -> String {
    let mut hasher = Sha256::new();
    absorb_field(&mut hasher, b"assistant-conversation");
    absorb_field(&mut hasher, actor_id.as_bytes());
    match scope {
        AssistantConversationScope::Personal => absorb_field(&mut hasher, b"personal"),
        AssistantConversationScope::Organization(org_id) => {
            absorb_field(&mut hasher, b"organization");
            absorb_field(&mut hasher, &org_id.to_le_bytes());
        }
    }
    absorb_field(&mut hasher, conversation_id.as_bytes());
    finish_key(hasher)
}

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

/// Derive a storage key for a device-bound conversation.
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
    finish_key(hasher)
}

fn finish_key(hasher: Sha256) -> String {
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

    #[test]
    fn assistant_keys_isolate_actors_and_access_contexts_from_device_conversations() {
        use AssistantConversationScope::{Organization, Personal};

        let personal = derive_assistant_conversation_key("7", Personal, "thread");
        let organization = derive_assistant_conversation_key("7", Organization(1), "thread");
        assert_eq!(
            personal,
            derive_assistant_conversation_key("7", Personal, "thread")
        );
        assert_ne!(
            personal,
            derive_assistant_conversation_key("7", Personal, "other")
        );
        assert_ne!(
            personal,
            derive_assistant_conversation_key("8", Personal, "thread")
        );
        assert_ne!(personal, organization);
        assert_ne!(
            organization,
            derive_assistant_conversation_key("7", Organization(2), "thread")
        );
        assert_ne!(personal, key("7", "device", Some("thread")));
    }
}
