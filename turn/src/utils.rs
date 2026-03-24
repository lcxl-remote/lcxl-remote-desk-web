use base64::{Engine as _, prelude::BASE64_STANDARD};
use hmac::Mac;

/// Generate TURN credentials for REST API authentication.
pub fn generate_turn_credentials(secret: &str, username: &str, ttl_secs: u64) -> (String, String) {
    let expiration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + ttl_secs;
    let username = format!("{}:{}", expiration, username);
    let mut mac = hmac::Hmac::<sha1::Sha1>::new_from_slice(secret.as_bytes())
        .expect("HMAC can take key of any size");
    mac.update(username.as_bytes());
    let code_slice = mac.finalize().into_bytes();
    (username, BASE64_STANDARD.encode(code_slice))
}
