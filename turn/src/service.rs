use std::{
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::Instant,
};

use async_trait::async_trait;
use tokio::net::UdpSocket;
use turn::{
    auth::AuthHandler,
    relay::relay_range::RelayAddressGeneratorRanges,
    server::{
        config::{ConnConfig, ServerConfig},
        Server,
    },
};
use webrtc_util::Conn;

use crate::{
    error::DeskTurnError,
    model::{Statistics, TurnApiState, TurnSettings, TurnTransport},
};

/// A custom `Conn` wrapper that counts incoming and outgoing bytes/packets.
struct TrackedUdpConn {
    inner: Arc<UdpSocket>,
    statistics: Arc<RwLock<Statistics>>,
}

#[async_trait]
impl Conn for TrackedUdpConn {
    async fn connect(&self, addr: SocketAddr) -> webrtc_util::Result<()> {
        self.inner.connect(addr).await?;
        Ok(())
    }

    async fn recv(&self, buf: &mut [u8]) -> webrtc_util::Result<usize> {
        let n = self.inner.recv(buf).await?;
        Ok(n)
    }

    async fn recv_from(&self, buf: &mut [u8]) -> webrtc_util::Result<(usize, SocketAddr)> {
        let (n, addr) = self.inner.recv_from(buf).await?;
        log::info!("TURN UDP recv_from: {} bytes from {}", n, addr);

        if let Ok(mut stats) = self.statistics.write() {
            stats.global.received_bytes += n;
            stats.global.received_pkts += 1;

            let session = stats.sessions.entry(addr).or_default();
            session.received_bytes += n;
            session.received_pkts += 1;
        }

        Ok((n, addr))
    }

    async fn send(&self, buf: &[u8]) -> webrtc_util::Result<usize> {
        let n = self.inner.send(buf).await?;
        Ok(n)
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> webrtc_util::Result<usize> {
        let n = self.inner.send_to(buf, target).await?;
        log::info!("TURN UDP send_to: {} bytes to {}", n, target);

        if let Ok(mut stats) = self.statistics.write() {
            stats.global.send_bytes += n;
            stats.global.send_pkts += 1;

            let session = stats.sessions.entry(target).or_default();
            session.send_bytes += n;
            session.send_pkts += 1;
        }

        Ok(n)
    }

    fn local_addr(&self) -> webrtc_util::Result<SocketAddr> {
        Ok(self.inner.local_addr()?)
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        self.inner.peer_addr().ok()
    }

    async fn close(&self) -> webrtc_util::Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync + 'static) {
        self
    }
}

/// Starts the TURN server with the provided config and auth handler.
pub async fn startup_turn_server<A>(
    settings: TurnSettings,
    auth_handler: Arc<A>,
) -> Result<Arc<TurnApiState>, DeskTurnError>
where
    A: AuthHandler + Send + Sync + 'static,
{
    log::info!("Starting turn server with realm {:?}", settings.realm);

    let mut conn_configs = vec![];
    let statistics = Arc::new(RwLock::new(Statistics::default()));

    for iface in &settings.interfaces {
        if iface.transport == TurnTransport::UDP {
            let bind_addr: SocketAddr = iface.listen.parse().map_err(|e| DeskTurnError::AnyhowError(anyhow::anyhow!("Invalid bind addr: {}", e)))?;
            let udp_socket = Arc::new(UdpSocket::bind(bind_addr)
                .await
                .map_err(|e| DeskTurnError::AnyhowError(anyhow::anyhow!("Bind failed: {}", e)))?);

            log::info!("TURN UDP bind: {}", bind_addr);

            let external_ip: std::net::IpAddr = iface
                .external
                .split(':')
                .next()
                .unwrap_or("0.0.0.0")
                .parse()
                .unwrap_or_else(|_| "0.0.0.0".parse().unwrap());

            let tracked_conn = Arc::new(TrackedUdpConn {
                inner: udp_socket,
                statistics: statistics.clone(),
            });

            let relay_generator = Box::new(RelayAddressGeneratorRanges {
                relay_address: external_ip,
                min_port: settings.relay_min_port,
                max_port: settings.relay_max_port,
                max_retries: 10,
                address: "0.0.0.0".to_owned(),
                net: Arc::new(webrtc_util::vnet::net::Net::new(None)),
            });

            conn_configs.push(ConnConfig {
                conn: tracked_conn,
                relay_addr_generator: relay_generator,
            });
        }
    }

    if conn_configs.is_empty() {
        log::warn!("No valid UDP interfaces configured for TURN server.");
    }

    let config = ServerConfig {
        realm: settings.realm.clone(),
        auth_handler,
        conn_configs,
        channel_bind_timeout: std::time::Duration::from_secs(600),
        alloc_close_notify: None,
    };

    let server = Server::new(config).await.map_err(|e| DeskTurnError::AnyhowError(anyhow::anyhow!("Server start failed: {}", e)))?;

    // We start it, it runs asynchronously in background inside Server.
    Box::leak(Box::new(server));

    let api_state = Arc::new(TurnApiState {
        uptime: Instant::now(),
        statistics,
        settings,
    });

    log::info!("Turn server started successfully.");
    Ok(api_state)
}
