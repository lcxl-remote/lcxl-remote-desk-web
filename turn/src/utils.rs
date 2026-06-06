use base64::{Engine as _, prelude::BASE64_STANDARD};
use hmac::Mac;

/// Generate TURN credentials for REST API authentication.
///
/// Returns `(username, password)` where `username = "{expiration}:{name}"`
/// (`expiration` is a Unix-seconds timestamp `ttl_secs` in the future) and
/// `password = base64(HMAC-SHA1(secret, username))`. The `{expiration}:` prefix
/// is what [`validate_rest_credential`] enforces and what keeps the username out
/// of any connection-id namespace.
pub fn generate_turn_credentials(secret: &str, username: &str, ttl_secs: u64) -> (String, String) {
    let expiration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + ttl_secs;
    let username = format!("{}:{}", expiration, username);
    let password = rest_password(secret, &username);
    (username, password)
}

/// `base64(HMAC-SHA1(secret, username))` — the password a REST TURN client must
/// present, and the value both `TurnAuthHandler`s recompute to build the key.
fn rest_password(secret: &str, username: &str) -> String {
    let mut mac = hmac::Hmac::<sha1::Sha1>::new_from_slice(secret.as_bytes())
        .expect("HMAC can take key of any size");
    mac.update(username.as_bytes());
    BASE64_STANDARD.encode(mac.finalize().into_bytes())
}

/// Validate a TURN REST credential and produce the long-term auth key.
///
/// `username` must be `"{expiration_unix_secs}:{name}"`. The credential is valid
/// **iff `now_secs < expiration`** (an exactly-equal timestamp is treated as
/// expired). On success the returned `Vec<u8>` is the key the TURN server
/// compares against the password the client sent; `None` means malformed or
/// expired (the caller maps this to an auth rejection).
///
/// This is the single source of truth for REST credential validation, shared by
/// both `web/server` and `manager` `TurnAuthHandler`s so the expiry semantics
/// live in exactly one place.
pub fn validate_rest_credential(
    secret: &str,
    username: &str,
    realm: &str,
    now_secs: u64,
) -> Option<Vec<u8>> {
    let expiration: u64 = username.split(':').next()?.parse().ok()?;
    if now_secs >= expiration {
        return None; // expired (boundary `==` counts as expired)
    }
    let password = rest_password(secret, username);
    Some(turn::auth::generate_auth_key(username, realm, &password))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "test-secret";
    const REALM: &str = "localhost";

    #[test]
    fn accepts_unexpired_credential() {
        // `name` part is preserved after the expiration prefix.
        let (username, password) = generate_turn_credentials(SECRET, "host-1", 100);
        assert!(username.ends_with(":host-1"));
        let exp: u64 = username.split(':').next().unwrap().parse().unwrap();
        // Key for an unexpired credential matches the manual recomputation.
        let key = validate_rest_credential(SECRET, &username, REALM, exp - 1).unwrap();
        assert_eq!(key, turn::auth::generate_auth_key(&username, REALM, &password));
    }

    #[test]
    fn expiration_boundary_is_exclusive() {
        let username = "1000:host-1";
        assert!(validate_rest_credential(SECRET, username, REALM, 999).is_some()); // now < exp
        assert!(validate_rest_credential(SECRET, username, REALM, 1000).is_none()); // now == exp
        assert!(validate_rest_credential(SECRET, username, REALM, 1001).is_none()); // now > exp
    }

    #[test]
    fn rejects_malformed_username() {
        assert!(validate_rest_credential(SECRET, "host-1", REALM, 0).is_none());
        assert!(validate_rest_credential(SECRET, "notanumber:host-1", REALM, 0).is_none());
        assert!(validate_rest_credential(SECRET, ":host-1", REALM, 0).is_none());
    }
}
