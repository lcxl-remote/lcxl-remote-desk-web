use actix_web::web;
use base64::prelude::*;
use hmac::{Hmac, Mac};
use serde_json::json;
use sha1::Sha1;
use turn_server::{
    statistics::Statistics,
    turn::{Observer, SessionAddr},
};

use crate::model::settings::SharedSettings;

#[derive(Clone)]
pub struct TurnObserver {
    pub settings: web::Data<SharedSettings>,
    pub statistics: Statistics,
}

impl TurnObserver {
    pub fn new(settings: web::Data<SharedSettings>, statistics: Statistics) -> Self {
        Self {
            settings,
            statistics,
        }
    }
}

impl Observer for TurnObserver {
    fn get_password(&self, username: &str) -> Option<String> {
        // Match the static authentication information first.
        log::info!("get_password: username={}", username);

        let handle = tokio::runtime::Handle::current();
        let settings_data = self.settings.clone();
        let secret = futures::executor::block_on(async move {
            handle
                .spawn_blocking(move || {
                    let settings = settings_data.blocking_read();
                    settings.turn.static_auth_secret.clone()
                })
                .await
                .unwrap_or_else(|error| {
                    log::error!(
                        "Failed to spawn_blocking for get_password, error: {}",
                        error
                    );
                    None
                })
        });

        if let Some(secret) = secret {
            let mut mac = Hmac::<Sha1>::new_from_slice(secret.as_bytes()).ok()?;
            mac.update(username.as_bytes());
            let result = mac.finalize();
            let code = result.into_bytes();
            return Some(BASE64_STANDARD.encode(code));
        }

        log::warn!(
            "static_auth_secret not found, authentication failed for username={}",
            username
        );
        None
    }

    #[allow(clippy::let_underscore_future)]
    fn allocated(&self, addr: &SessionAddr, name: &str, port: u16) {
        log::info!(
            "allocate: address={:?}, interface={:?}, username={:?}, port={}",
            addr.address,
            addr.interface,
            name,
            port
        );

        {
            self.statistics.register(*addr);

            turn_server::api::events::send_with_stream("allocated", || {
                json!({
                    "session": {
                        "address": addr.address,
                        "interface": addr.interface,
                    },
                    "username": name,
                    "port": port,
                })
            });
        }
    }

    #[allow(clippy::let_underscore_future)]
    fn channel_bind(&self, addr: &SessionAddr, name: &str, channel: u16) {
        log::info!(
            "channel bind: address={:?}, interface={:?}, username={:?}, channel={}",
            addr.address,
            addr.interface,
            name,
            channel
        );

        {
            turn_server::api::events::send_with_stream("channel_bind", || {
                json!({
                    "session": {
                        "address": addr.address,
                        "interface": addr.interface,
                    },
                    "username": name,
                    "channel": channel,
                })
            });
        }
    }

    #[allow(clippy::let_underscore_future)]
    fn create_permission(&self, addr: &SessionAddr, name: &str, ports: &[u16]) {
        log::info!(
            "create permission: address={:?}, interface={:?}, username={:?}, ports={:?}",
            addr.address,
            addr.interface,
            name,
            ports
        );

        {
            turn_server::api::events::send_with_stream("create_permission", || {
                json!({
                    "session": {
                        "address": addr.address,
                        "interface": addr.interface,
                    },
                    "username": name,
                    "ports": ports,
                })
            });
        }
    }

    #[allow(clippy::let_underscore_future)]
    fn refresh(&self, addr: &SessionAddr, name: &str, lifetime: u32) {
        log::info!(
            "refresh: address={:?}, interface={:?}, username={:?}, lifetime={}",
            addr.address,
            addr.interface,
            name,
            lifetime
        );

        {
            turn_server::api::events::send_with_stream("refresh", || {
                json!({
                    "session": {
                        "address": addr.address,
                        "interface": addr.interface,
                    },
                    "username": name,
                    "lifetime": lifetime,
                })
            });
        }
    }

    #[allow(clippy::let_underscore_future)]
    fn closed(&self, addr: &SessionAddr, name: &str) {
        log::info!(
            "closed: address={:?}, interface={:?}, username={:?}",
            addr.address,
            addr.interface,
            name
        );

        {
            self.statistics.unregister(&addr);

            turn_server::api::events::send_with_stream("closed", || {
                json!({
                    "session": {
                        "address": addr.address,
                        "interface": addr.interface,
                    },
                    "username": name,
                })
            });
        }
    }
}
