use std::net::SocketAddr;

use actix_web::web;
use base64::prelude::*;
use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::model::settings::SharedSettings;

use webrtc::turn;

pub struct TurnAuthHandler {
    pub secret: Option<String>,
}

impl TurnAuthHandler {
    pub fn new(settings: web::Data<SharedSettings>) -> Self {
        // Clone the secret during initialization to avoid blocking async contexts later
        let secret = {
            let s = futures::executor::block_on(settings.read());
            s.turn.static_auth_secret.clone()
        };
        Self { secret }
    }
}

impl turn::auth::AuthHandler for TurnAuthHandler {
    fn auth_handle(
        &self,
        username: &str,
        realm: &str,
        _src_addr: SocketAddr,
    ) -> Result<Vec<u8>, turn::Error> {
        log::debug!("auth_handle: username={}, realm={}", username, realm);

        if let Some(secret) = &self.secret {
            // TURN REST API password generation
            let mut mac = Hmac::<Sha1>::new_from_slice(secret.as_bytes())
                .map_err(|e| turn::Error::Other(e.to_string()))?;
            mac.update(username.as_bytes());
            let result = mac.finalize();
            let password_bytes = result.into_bytes();
            let password = BASE64_STANDARD.encode(&password_bytes);

            let key = turn::auth::generate_auth_key(username, realm, &password);
            log::debug!("auth_handle success for username={}", username);
            return Ok(key);
        }

        log::warn!("static_auth_secret not found, auth failed for {}", username);
        Err(turn::Error::Other("Unauthorized".to_owned()))
    }
}
