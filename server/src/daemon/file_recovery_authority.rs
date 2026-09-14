//! Isolate backups by the authenticated upstream connection and credential.
use sha2::{Digest, Sha256};

pub(crate) fn bind(lane: &str, endpoint: &str, credential: &str) -> Option<String> {
    if !matches!(lane, "local-signal" | "remote-signal" | "manager") || credential.is_empty() {
        return None;
    }
    let mut endpoint = url::Url::parse(endpoint).ok()?;
    if !matches!(endpoint.scheme(), "ws" | "wss" | "http" | "https") {
        return None;
    }
    endpoint.set_query(None);
    endpoint.set_fragment(None);
    endpoint.set_username("").ok()?;
    endpoint.set_password(None).ok()?;
    // Loopback port changes do not transfer device ownership. Remote origins
    // and paths remain part of the authenticated namespace.
    let endpoint = if lane == "local-signal" {
        "local"
    } else {
        endpoint.as_str()
    };
    Some(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(lane, endpoint, credential)).ok()?)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stable_across_reconnects_but_not_authority_or_credential_changes() {
        let bound = bind("manager", "wss://central/desk?token=redacted", "secret").unwrap();
        assert_eq!(
            Some(bound.clone()),
            bind("manager", "wss://central/desk?version=2", "secret")
        );
        assert_ne!(
            Some(bound.clone()),
            bind("manager", "wss://other/desk", "secret")
        );
        assert_ne!(
            Some(bound.clone()),
            bind("manager", "wss://central/desk", "new-owner-secret")
        );
        assert_ne!(
            Some(bound.clone()),
            bind("remote-signal", "wss://central/desk", "secret")
        );
        assert!(!bound.contains("secret"));
        assert_eq!(
            bind("local-signal", "ws://localhost:1/desk", "secret"),
            bind("local-signal", "ws://localhost:2/desk", "secret")
        );
        assert!(bind("untrusted", "ws://host", "secret").is_none());
        assert!(bind("manager", "wss://host", "").is_none());
    }
}
