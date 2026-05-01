use std::net::SocketAddr;

use actix_web::web;
use base64::prelude::*;
use desk_signal::model::SharedConnectionMap;
use desk_turn::model::TurnSettings;
use hmac::{Hmac, Mac};
use sha1::Sha1;

use tokio::runtime::Handle;
use webrtc::turn;

pub struct TurnAuthHandler {
    pub turn_settings: TurnSettings,
    pub connection_map: web::Data<SharedConnectionMap>,
}

impl TurnAuthHandler {
    pub fn new(
        turn_settings: TurnSettings,
        connection_map: web::Data<SharedConnectionMap>,
    ) -> Self {
        Self {
            turn_settings,
            connection_map,
        }
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
        // Check username/password(connection_id/client_id)
        let connection_id = username.to_string();

        let handle = match Handle::try_current() {
            Ok(handle) => handle,
            Err(e) => {
                log::error!("Failed to get tokio handle in auth_handle: {}", e);
                return Err(turn::Error::Other("Internal error".to_owned()));
            }
        };
        let connection_map = self.connection_map.clone();
        let connection_option = futures::executor::block_on(async move {
            handle
                .spawn_blocking(move || connection_map.blocking_read().get(&connection_id).cloned())
                .await
        })
        .map_err(|e| turn::Error::Other(e.to_string()))?;
        if let Some(connection) = connection_option {
            if let Some(client_id) = &connection.model.version_info.client_id {
                let key = turn::auth::generate_auth_key(username, realm, client_id);
                log::info!("auth_handle password success for username={}", username);
                return Ok(key);
            } else {
                log::warn!(
                    "auth_handle password failed for username={}, client_id is None",
                    username
                );
            }
        }

        // Check static auth secret
        if let Some(secret) = &self.turn_settings.static_auth_secret {
            // TURN REST API password generation
            let mut mac = Hmac::<Sha1>::new_from_slice(secret.as_bytes())
                .map_err(|e| turn::Error::Other(e.to_string()))?;
            mac.update(username.as_bytes());
            let result = mac.finalize();
            let password_bytes = result.into_bytes();
            let password = BASE64_STANDARD.encode(password_bytes);

            let key = turn::auth::generate_auth_key(username, realm, &password);
            log::info!(
                "auth_handle static_auth_secret success for username={}",
                username
            );
            return Ok(key);
        }

        log::info!("username not found, auth failed for {}", username);
        Err(turn::Error::Other("Unauthorized".to_owned()))
    }
}
