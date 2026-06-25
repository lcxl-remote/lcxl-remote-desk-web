use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use actix_web::web;
use desk_signal::model::SharedConnectionMap;
use desk_turn::model::{Statistics, TurnSettings};

use tokio::runtime::Handle;
use webrtc::turn;

pub struct TurnAuthHandler {
    pub turn_settings: TurnSettings,
    pub connection_map: web::Data<SharedConnectionMap>,
    /// Shared TURN byte-accounting state. The handler records the
    /// `src_addr → connection_id` binding here on every successful auth so
    /// `TrackedUdpConn` can fold relayed bytes into the per-connection counters.
    pub statistics: Arc<RwLock<Statistics>>,
}

impl TurnAuthHandler {
    pub fn new(
        turn_settings: TurnSettings,
        connection_map: web::Data<SharedConnectionMap>,
        statistics: Arc<RwLock<Statistics>>,
    ) -> Self {
        Self {
            turn_settings,
            connection_map,
            statistics,
        }
    }

    /// Bind a TURN client's source address to its `connection_id` in the shared
    /// statistics. Called on each successful auth; lock poisoning is logged and
    /// ignored (accounting is best-effort and must never fail an auth).
    fn record_binding(&self, src_addr: SocketAddr, connection_id: &str) {
        match self.statistics.write() {
            Ok(mut stats) => stats.record_binding(src_addr, connection_id),
            Err(e) => log::warn!("TURN statistics lock poisoned, skipping binding: {}", e),
        }
    }
}

impl turn::auth::AuthHandler for TurnAuthHandler {
    fn auth_handle(
        &self,
        username: &str,
        realm: &str,
        src_addr: SocketAddr,
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
                // Local path: the username is the connection_id verbatim.
                self.record_binding(src_addr, username);
                return Ok(key);
            } else {
                log::warn!(
                    "auth_handle password failed for username={}, client_id is None",
                    username
                );
            }
        }

        // Check static auth secret: REST credential `{expiration}:{name}` with
        // server-side expiry enforcement, shared with the manager handler.
        if let Some(secret) = &self.turn_settings.static_auth_secret {
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(u64::MAX); // fail closed: clock error → treat as expired
            return match desk_turn::utils::validate_rest_credential(
                secret, username, realm, now_secs,
            ) {
                Some(key) => {
                    log::info!(
                        "auth_handle REST credential success for username={}",
                        username
                    );
                    // REST path: username is `{expiration}:{connection_id}`; bind
                    // the address to the name after the first colon.
                    if let Some(name) = connection_id_from_rest_username(username) {
                        self.record_binding(src_addr, name);
                    }
                    Ok(key)
                }
                None => {
                    log::warn!(
                        "auth_handle REST credential rejected (malformed/expired) for username={}",
                        username
                    );
                    Err(turn::Error::Other("Unauthorized".to_owned()))
                }
            };
        }

        log::info!("username not found, auth failed for {}", username);
        Err(turn::Error::Other("Unauthorized".to_owned()))
    }
}

/// Extract the `connection_id` from a TURN REST username of the form
/// `{expiration}:{connection_id}`. Returns `None` when there is no colon. The
/// `connection_id` (a UUID) never contains a colon, so splitting on the first
/// one recovers it even if the expiration were to.
fn connection_id_from_rest_username(username: &str) -> Option<&str> {
    username.split_once(':').map(|(_, name)| name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use webrtc::turn::auth::AuthHandler;

    fn handler(secret: Option<&str>) -> TurnAuthHandler {
        let turn_settings = TurnSettings {
            static_auth_secret: secret.map(str::to_owned),
            ..TurnSettings::default()
        };
        let connection_map = web::Data::new(SharedConnectionMap::from(BTreeMap::new()));
        let statistics = Arc::new(RwLock::new(Statistics::default()));
        TurnAuthHandler::new(turn_settings, connection_map, statistics)
    }

    fn addr() -> SocketAddr {
        "127.0.0.1:9000".parse().unwrap()
    }

    // `auth_handle` calls `Handle::try_current()` + `block_on(spawn_blocking)`,
    // so the test needs a Tokio runtime (multi-thread so the blocking lookup of
    // the empty connection map can complete while we block on it).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rest_credential_unexpired_is_accepted() {
        // Far-future expiration → time-independent; empty map falls to static branch.
        let result = handler(Some("secret")).auth_handle("9999999999:host-1", "localhost", addr());
        assert!(result.is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rest_credential_expired_is_rejected() {
        let result = handler(Some("secret")).auth_handle("1:host-1", "localhost", addr());
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn malformed_rest_username_is_rejected() {
        let result = handler(Some("secret")).auth_handle("no-expiration", "localhost", addr());
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rest_success_binds_name_after_colon() {
        let h = handler(Some("secret"));
        h.auth_handle("9999999999:host-1", "localhost", addr())
            .expect("rest auth should succeed");
        // The auth must have bound the address to the connection_id (the name
        // after the colon), not the raw `{exp}:{name}` username.
        let stats = h.statistics.read().unwrap();
        assert_eq!(stats.connection_of(&addr()), Some("host-1"));
    }

    #[test]
    fn rest_username_parse_extracts_name() {
        assert_eq!(
            connection_id_from_rest_username("12345:host-1"),
            Some("host-1")
        );
        assert_eq!(connection_id_from_rest_username("no-colon"), None);
        // Only the first colon splits; the remainder is the name.
        assert_eq!(connection_id_from_rest_username("12345:a:b"), Some("a:b"));
    }
}
