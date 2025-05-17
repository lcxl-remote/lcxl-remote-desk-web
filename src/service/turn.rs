use std::{sync::Arc, time::Instant};

use actix_web::web;
use serde_json::json;
use tokio::runtime::Handle;
use turn_server::{
    statistics::Statistics,
    turn::{Observer, Service, SessionAddr},
};

use crate::{
    desk_error::DeskError,
    model::{
        settings::{Settings, SharedSettings},
        turn::TurnApiState,
    },
};

#[rustfmt::skip]
pub static SOFTWARE: &str = concat!(
    "lcxl-web-remote-desk-turn-rs.",
    env!("CARGO_PKG_VERSION")
);

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
        let handle = Handle::current();
        // Spawn a local task to get user settings from shared settings.
        log::info!("get_password: username={}", username);

        let user_settings = handle.block_on(async move {
            let settings = self.settings.lock().await;
            settings.user.clone()
        });

        if user_settings.login_user_name == username {
            log::info!("found user by username={}", username);
            return Some(user_settings.login_password.clone());
        }
        log::info!("not found user by username={}", username);
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

/// Starts the TURN server with the provided settings.
pub async fn startup_turn_server(
    settings: web::Data<SharedSettings>,
) -> Result<TurnApiState, DeskError> {
    let config = {
        let settings = settings.lock().await;
        Arc::new(settings.to_turn_server_config()?)
    };
    log::info!("Starting turn server with config {:?}", config);

    let statistics = Statistics::default();
    let service = Service::new(
        SOFTWARE.to_string(),
        config.turn.realm.clone(),
        config.turn.get_externals(),
        TurnObserver::new(settings, statistics.clone()),
    );

    turn_server::server::start(&config, &statistics, &service).await?;
    let api_state = TurnApiState {
        config: config.clone(),
        uptime: Instant::now(),
        service,
        statistics,
    };

    log::info!("Turn server starteds successfully.");
    Ok(api_state)
}
