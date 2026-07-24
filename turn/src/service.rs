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
        Server,
        config::{ConnConfig, ServerConfig},
    },
};
use webrtc_util::Conn;

use crate::{
    error::DeskTurnError,
    model::{Statistics, TurnApiState, TurnSettings, TurnTrafficClass, TurnTransport},
};

/// STUN magic cookie (RFC 5389 §6), located at bytes[4..8] of every STUN message.
const STUN_MAGIC_COOKIE: u32 = 0x2112_A442;
/// STUN message header size in bytes (RFC 5389 §6).
const STUN_HEADER_SIZE: usize = 20;
/// TURN Send indication method (RFC 5766) — client → server relayed data.
const METHOD_SEND: u16 = 0x006;
/// TURN Data indication method (RFC 5766) — server → client relayed data.
const METHOD_DATA: u16 = 0x007;

/// Classify a single client-facing TURN datagram (`data == &buf[..n]`).
///
/// Relay = a validated ChannelData message, or a well-formed STUN Send/Data
/// indication. Everything else — control STUN, malformed, or too short — is
/// classified as Control (fail-open: bytes that cannot be confirmed as relayed
/// are never billed).
fn classify(data: &[u8]) -> TurnTrafficClass {
    // Validated ChannelData: checks header length, declared length, and channel
    // number range 0x4000..=0x7FFF (stricter than a bare leading-byte check).
    if turn::proto::chandata::ChannelData::is_channel_data(data) {
        return TurnTrafficClass::Relay;
    }
    // STUN message: require a well-formed header BEFORE trusting the method, so a
    // malformed ChannelData / non-STUN datagram cannot slip through as a Send/Data
    // relay indication. A valid STUN header (RFC 5389 §6) has:
    //   - the top two bits of the message type set to zero (`data[0] & 0xC0 == 0`),
    //   - the magic cookie 0x2112A442 at bytes[4..8], and
    //   - a message-length field (bytes[2..4]) that exactly accounts for the body,
    //     i.e. `len == 20 + declared_length`.
    // Anything failing these stays Control (fail-open: never billed).
    if data.len() >= STUN_HEADER_SIZE
        && data[0] & 0xc0 == 0
        && u32::from_be_bytes([data[4], data[5], data[6], data[7]]) == STUN_MAGIC_COOKIE
        && data.len() == STUN_HEADER_SIZE + u16::from_be_bytes([data[2], data[3]]) as usize
    {
        let typ = u16::from_be_bytes([data[0], data[1]]);
        // Reassemble the 12-bit STUN method from its interleaved bit groups.
        let method = (typ & 0x000f) | ((typ & 0x00e0) >> 1) | ((typ & 0x3e00) >> 2);
        if method == METHOD_SEND || method == METHOD_DATA {
            return TurnTrafficClass::Relay;
        }
    }
    TurnTrafficClass::Control
}

/// A custom `Conn` wrapper that counts incoming and outgoing bytes/packets,
/// split into relay (billable) and control (observability) traffic classes.
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
        log::trace!("TURN UDP recv_from: {} bytes from {}", n, addr);

        // Classify only the valid payload slice, not the whole caller buffer.
        let class = classify(&buf[..n]);
        if let Ok(mut stats) = self.statistics.write() {
            stats.global.add_recv(n, class);
            stats.sessions.entry(addr).or_default().add_recv(n, class);
            stats.record_recv(addr, n, class);
        }

        Ok((n, addr))
    }

    async fn send(&self, buf: &[u8]) -> webrtc_util::Result<usize> {
        let n = self.inner.send(buf).await?;
        Ok(n)
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> webrtc_util::Result<usize> {
        let n = self.inner.send_to(buf, target).await?;
        log::trace!("TURN UDP send_to: {} bytes to {}", n, target);

        // Classify only the bytes actually sent.
        let class = classify(&buf[..n]);
        if let Ok(mut stats) = self.statistics.write() {
            stats.global.add_send(n, class);
            stats.sessions.entry(target).or_default().add_send(n, class);
            stats.record_send(target, n, class);
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
    statistics: Arc<RwLock<Statistics>>,
) -> Result<Arc<TurnApiState>, DeskTurnError>
where
    A: AuthHandler + Send + Sync + 'static,
{
    log::info!("Starting turn server with realm {:?}", settings.realm);

    let mut conn_configs = vec![];

    for iface in &settings.interfaces {
        if iface.transport == TurnTransport::UDP {
            let bind_addr: SocketAddr = iface.listen.parse().map_err(|e| {
                DeskTurnError::AnyhowError(anyhow::anyhow!("Invalid bind addr: {}", e))
            })?;
            let udp_socket =
                Arc::new(UdpSocket::bind(bind_addr).await.map_err(|e| {
                    DeskTurnError::AnyhowError(anyhow::anyhow!("Bind failed: {}", e))
                })?);

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

    let server = Server::new(config)
        .await
        .map_err(|e| DeskTurnError::AnyhowError(anyhow::anyhow!("Server start failed: {}", e)))?;

    let api_state = Arc::new(TurnApiState {
        uptime: Instant::now(),
        statistics,
        settings,
        server,
    });

    log::info!("Turn server started successfully.");
    Ok(api_state)
}

#[cfg(test)]
mod classify_tests {
    use super::*;

    /// A 20-byte STUN message with the given on-wire message type, a valid magic
    /// cookie, zero length and a zero transaction id.
    fn stun_msg(typ: u16) -> Vec<u8> {
        let mut m = vec![0u8; STUN_HEADER_SIZE];
        m[0..2].copy_from_slice(&typ.to_be_bytes());
        m[4..8].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
        m
    }

    /// A well-formed ChannelData message on `channel` carrying `data`.
    fn channel_data(channel: u16, data: &[u8]) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(&channel.to_be_bytes());
        m.extend_from_slice(&(data.len() as u16).to_be_bytes());
        m.extend_from_slice(data);
        m
    }

    fn is_relay(data: &[u8]) -> bool {
        matches!(classify(data), TurnTrafficClass::Relay)
    }

    #[test]
    fn send_and_data_indications_are_relay() {
        assert!(is_relay(&stun_msg(0x0016)), "Send indication");
        assert!(is_relay(&stun_msg(0x0017)), "Data indication");
    }

    #[test]
    fn control_methods_and_responses_are_control() {
        for typ in [
            0x0001u16, // Binding request
            0x0101,    // Binding success
            0x0003,    // Allocate request
            0x0103,    // Allocate success
            0x0113,    // Allocate error
            0x0004,    // Refresh request
            0x0008,    // CreatePermission request
            0x0009,    // ChannelBind request
        ] {
            assert!(
                !is_relay(&stun_msg(typ)),
                "type {:#06x} must be control",
                typ
            );
        }
    }

    #[test]
    fn valid_channel_data_is_relay() {
        assert!(is_relay(&channel_data(0x4000, &[1, 2, 3, 4])));
        assert!(is_relay(&channel_data(0x7fff, &[9; 16])));
    }

    #[test]
    fn malformed_channeldata_does_not_leak_as_relay() {
        // Too short to be valid ChannelData; its first two
        // bytes decode to STUN method 0x006 (Send), but the STUN header check
        // (length + magic cookie) rejects it, so it stays Control.
        assert!(!is_relay(&[0x40, 0x06]));
        // Channel number out of range 0x4000..=0x7FFF.
        assert!(!is_relay(&channel_data(0x8000, &[1, 2, 3, 4])));
        // Declared length exceeds the actual payload.
        assert!(!is_relay(&[0x40, 0x00, 0xff, 0xff, 1, 2]));
    }

    #[test]
    fn stun_without_magic_cookie_is_control() {
        // A Send-indication type but a corrupted magic cookie must not bill.
        let mut m = stun_msg(0x0016);
        m[4] ^= 0xff;
        assert!(!is_relay(&m));
    }

    #[test]
    fn malformed_with_magic_cookie_does_not_leak_as_relay() {
        // Non-STUN first bytes (top two bits 0b01 → not a STUN type) that also fail
        // ChannelData validation (oversized declared length), but carry a magic
        // cookie at bytes[4..8]. The `data[0] & 0xC0` guard keeps this Control;
        // without it the method bits of 0x4006 would decode to Send (0x006).
        let mut m = vec![0u8; STUN_HEADER_SIZE];
        m[0] = 0x40;
        m[1] = 0x06;
        m[2] = 0xff; // declared length far exceeds the payload → not ChannelData
        m[3] = 0xff;
        m[4..8].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
        assert!(!is_relay(&m));

        // A genuine Send type (0x0016) with a valid magic cookie but a STUN length
        // field inconsistent with the datagram size is not a well-formed message.
        let mut m2 = stun_msg(0x0016);
        m2[2] = 0xff;
        m2[3] = 0xff;
        assert!(!is_relay(&m2));
    }

    #[test]
    fn short_or_empty_datagrams_are_control() {
        assert!(!is_relay(&[]));
        assert!(!is_relay(&[0x00]));
        // Send-indication first bytes but shorter than a STUN header.
        assert!(!is_relay(&[0x00, 0x16, 0x00, 0x00, 0x21, 0x12, 0xa4, 0x42]));
    }
}
