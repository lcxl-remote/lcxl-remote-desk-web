use std::{
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::Instant,
};

use async_trait::async_trait;
use tokio::net::UdpSocket;
use turn::{
    auth::AuthHandler,
    server::{
        Server,
        config::{ConnConfig, ServerConfig},
    },
};
use webrtc_util::Conn;

use crate::{
    error::DeskTurnError,
    interface::plan_turn_interfaces,
    model::{Statistics, TurnApiState, TurnSettings, TurnTrafficClass},
    relay::FamilyPinnedRelay,
};

/// `turn 0.17.1` allocates this private `server::INBOUND_MTU` receive buffer.
/// Keep the version in the name and lock its behavior with an integration test
/// whenever the dependency is upgraded.
pub const PINNED_TURN_0_17_INBOUND_MTU_BYTES: usize = 1500;
#[cfg(test)]
const STUN_HEADER_SIZE: usize = 20;
#[cfg(test)]
const STUN_MAGIC_COOKIE: u32 = 0x2112_A442;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayDirection {
    ClientToServer,
    ServerToClient,
}

#[async_trait]
pub trait RelayTrafficGate: Send + Sync {
    async fn allow_relay(&self, peer: SocketAddr, direction: RelayDirection, bytes: usize) -> bool;
}

#[derive(Debug, Default)]
pub struct AllowAllRelayTrafficGate;

#[async_trait]
impl RelayTrafficGate for AllowAllRelayTrafficGate {
    async fn allow_relay(
        &self,
        _peer: SocketAddr,
        _direction: RelayDirection,
        _bytes: usize,
    ) -> bool {
        true
    }
}

/// Classify a single client-facing TURN datagram (`data == &buf[..n]`).
///
/// Relay = a validated ChannelData message, or a well-formed STUN Send/Data
/// indication. Everything else — control STUN, malformed, or too short — is
/// classified as Control (fail-open: bytes that cannot be confirmed as relayed
/// are never billed).
pub fn classify(data: &[u8]) -> TurnTrafficClass {
    // Validated ChannelData: checks header length, declared length, and channel
    // number range 0x4000..=0x7FFF (stricter than a bare leading-byte check).
    if turn::proto::chandata::ChannelData::is_channel_data(data) {
        return TurnTrafficClass::Relay;
    }
    // Use the same STUN crate/version as the TURN server instead of maintaining
    // an independent header parser whose trailing-byte semantics can drift.
    let mut message = stun::message::Message::new();
    message.raw.clear();
    message.raw.extend_from_slice(data);
    if message.decode().is_ok()
        && message.typ.class == stun::message::CLASS_INDICATION
        && matches!(
            message.typ.method,
            stun::message::METHOD_SEND | stun::message::METHOD_DATA
        )
    {
        return TurnTrafficClass::Relay;
    }
    TurnTrafficClass::Control
}

/// A custom `Conn` wrapper that counts incoming and outgoing bytes/packets,
/// split into relay (billable) and control (observability) traffic classes.
struct TrackedUdpConn {
    inner: Arc<UdpSocket>,
    statistics: Arc<RwLock<Statistics>>,
    gate: Arc<dyn RelayTrafficGate>,
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
        loop {
            let (n, addr) = self.inner.recv_from(buf).await?;
            log::trace!("TURN UDP recv_from: {} bytes from {}", n, addr);
            let class = classify(&buf[..n]);
            if class == TurnTrafficClass::Relay
                && !self
                    .gate
                    .allow_relay(addr, RelayDirection::ClientToServer, n)
                    .await
            {
                continue;
            }
            if let Ok(mut stats) = self.statistics.write() {
                stats.global.add_recv(n, class);
                stats.sessions.entry(addr).or_default().add_recv(n, class);
                stats.record_recv(addr, n, class);
            }
            return Ok((n, addr));
        }
    }

    async fn send(&self, buf: &[u8]) -> webrtc_util::Result<usize> {
        let n = self.inner.send(buf).await?;
        Ok(n)
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> webrtc_util::Result<usize> {
        let class = classify(buf);
        if class == TurnTrafficClass::Relay
            && !self
                .gate
                .allow_relay(target, RelayDirection::ServerToClient, buf.len())
                .await
        {
            // Model UDP network loss without surfacing a socket failure to the
            // TURN allocation state machine.
            return Ok(buf.len());
        }
        let n = self.inner.send_to(buf, target).await?;
        log::trace!("TURN UDP send_to: {} bytes to {}", n, target);
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
    traffic_gate: Arc<dyn RelayTrafficGate>,
) -> Result<Arc<TurnApiState>, DeskTurnError>
where
    A: AuthHandler + Send + Sync + 'static,
{
    log::info!("Starting turn server with realm {:?}", settings.realm);

    let plan = plan_turn_interfaces(&settings.interfaces);
    plan.report_rejections();
    if plan.servable.is_empty() {
        return Err(DeskTurnError::AnyhowError(anyhow::anyhow!(
            "no servable TURN interface: {} configured, none usable",
            settings.interfaces.len()
        )));
    }

    let mut conn_configs = vec![];
    for servable in &plan.servable {
        let udp_socket = Arc::new(
            UdpSocket::bind(servable.listen)
                .await
                .map_err(|e| DeskTurnError::AnyhowError(anyhow::anyhow!("Bind failed: {}", e)))?,
        );

        log::info!(
            "TURN UDP bind: {} advertising {}",
            servable.listen,
            servable.external
        );

        let tracked_conn = Arc::new(TrackedUdpConn {
            inner: udp_socket,
            statistics: statistics.clone(),
            gate: traffic_gate.clone(),
        });

        conn_configs.push(ConnConfig {
            conn: tracked_conn,
            relay_addr_generator: Box::new(FamilyPinnedRelay::new(
                servable.external.ip(),
                settings.relay_min_port,
                settings.relay_max_port,
            )),
        });
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

    // Keep only what is being served: this state is what advertises the relay
    // and what `/info` reports, and either one naming an interface the server
    // never bound sends peers at an address nothing answers on.
    let settings = TurnSettings {
        interfaces: plan.servable.iter().map(|s| s.canonical()).collect(),
        ..settings
    };

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
mod startup_tests {
    use super::*;
    use crate::model::{TurnInterface, TurnTransport};

    struct AllowAll;
    impl AuthHandler for AllowAll {
        fn auth_handle(
            &self,
            _username: &str,
            _realm: &str,
            _src_addr: SocketAddr,
        ) -> Result<Vec<u8>, turn::Error> {
            Ok(vec![0u8; 16])
        }
    }

    fn udp(listen: String) -> TurnInterface {
        TurnInterface {
            transport: TurnTransport::UDP,
            listen,
            external: "127.0.0.1:3478".to_string(),
        }
    }

    /// Every configured UDP interface has to be bound, not just the first.
    ///
    /// Proven by conflict rather than by counting: the second interface names a
    /// port this test already holds, so the start can only fail if that bind was
    /// really attempted. A loop that stopped after the first interface would
    /// report success here.
    #[tokio::test]
    async fn every_udp_interface_is_bound() {
        let occupied = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let taken = occupied.local_addr().unwrap();

        let settings = TurnSettings {
            interfaces: vec![udp("127.0.0.1:0".to_string()), udp(taken.to_string())],
            ..TurnSettings::default()
        };
        let result = startup_turn_server(
            settings,
            Arc::new(AllowAll),
            Arc::new(RwLock::new(Statistics::default())),
            Arc::new(AllowAllRelayTrafficGate),
        )
        .await;

        assert!(
            result.is_err(),
            "the second interface must be bound too, and its port is taken"
        );
    }

    /// The first interface is bound for real as well, so a start that succeeds
    /// has actually taken a socket rather than skipping the loop entirely.
    #[tokio::test]
    async fn a_single_interface_starts_and_closes() {
        let settings = TurnSettings {
            interfaces: vec![udp("127.0.0.1:0".to_string())],
            ..TurnSettings::default()
        };
        let state = startup_turn_server(
            settings,
            Arc::new(AllowAll),
            Arc::new(RwLock::new(Statistics::default())),
            Arc::new(AllowAllRelayTrafficGate),
        )
        .await
        .expect("an ephemeral port must be bindable");
        state.server.close().await.expect("close");
    }

    /// A configuration whose every entry is unservable fails the start with a
    /// reason, instead of substituting a wildcard address and reporting success.
    ///
    /// That substitution was the old behaviour, and it is worse than a failure:
    /// the host advertised `turn:0.0.0.0:3478` to every peer while its logs said
    /// the server had started.
    #[tokio::test]
    async fn a_configuration_with_nothing_servable_refuses_to_start() {
        let settings = TurnSettings {
            interfaces: vec![TurnInterface {
                transport: TurnTransport::UDP,
                listen: "127.0.0.1:0".to_string(),
                external: "relay.example.com:3478".to_string(),
            }],
            ..TurnSettings::default()
        };
        let error = match startup_turn_server(
            settings,
            Arc::new(AllowAll),
            Arc::new(RwLock::new(Statistics::default())),
            Arc::new(AllowAllRelayTrafficGate),
        )
        .await
        {
            Ok(_) => panic!("an unresolvable external address is not a relay"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("no servable TURN interface"), "{error}");
    }

    /// What the runtime keeps is what it serves: an entry it refused to bind
    /// must not survive into the state that issues ICE candidates, or peers
    /// spend their ICE budget dialling a socket that was never opened.
    #[tokio::test]
    async fn the_runtime_only_carries_what_it_bound() {
        let settings = TurnSettings {
            static_auth_secret: Some("s3cret".into()),
            interfaces: vec![
                udp("127.0.0.1:0".to_string()),
                TurnInterface {
                    transport: TurnTransport::TCP,
                    listen: "127.0.0.1:0".to_string(),
                    external: "203.0.113.7:3478".to_string(),
                },
            ],
            ..TurnSettings::default()
        };
        let state = startup_turn_server(
            settings,
            Arc::new(AllowAll),
            Arc::new(RwLock::new(Statistics::default())),
            Arc::new(AllowAllRelayTrafficGate),
        )
        .await
        .expect("the UDP entry is servable");

        assert_eq!(
            state.settings.interfaces.len(),
            1,
            "the TCP entry was never bound and must not be kept"
        );
        assert_eq!(
            state
                .settings
                .get_rest_ice_servers("peer", 600)
                .expect("a running relay with a secret advertises itself")
                .urls,
            vec!["turn:127.0.0.1:3478?transport=udp".to_string()],
            "only the bound interface is advertised"
        );
        state.server.close().await.expect("close");
    }
}

/// A client allocating a relay over IPv6, end to end.
///
/// Parsing an IPv6 `external` is not the same as relaying over it: the relay
/// socket is bound separately, and an IPv4 socket paired with an advertised IPv6
/// address yields an allocation that succeeds and can carry nothing. Only moving
/// bytes through the relay tells the two apart.
#[cfg(test)]
mod ipv6_relay_tests {
    use super::*;
    use crate::model::{TurnInterface, TurnTransport};
    use std::net::{IpAddr, Ipv6Addr};
    use std::time::Duration;
    use turn::client::{Client, ClientConfig};

    const USER: &str = "relay-user";
    const PASS: &str = "relay-pass";
    const REALM: &str = "localhost";

    struct StaticCredential;
    impl AuthHandler for StaticCredential {
        fn auth_handle(
            &self,
            username: &str,
            realm: &str,
            _src_addr: SocketAddr,
        ) -> Result<Vec<u8>, turn::Error> {
            if username == USER {
                Ok(turn::auth::generate_auth_key(username, realm, PASS))
            } else {
                Err(turn::Error::ErrFakeErr)
            }
        }
    }

    /// Whether this machine can carry IPv6 loopback traffic at all.
    async fn ipv6_loopback_available() -> bool {
        UdpSocket::bind("[::1]:0").await.is_ok()
    }

    #[tokio::test]
    async fn a_client_relays_over_an_ipv6_allocation() {
        if !ipv6_loopback_available().await {
            eprintln!("skipping: no IPv6 loopback on this machine");
            return;
        }

        // The advertised address must name the port peers dial, and the server
        // binds it, so the port is chosen before the server starts. Taking it
        // from an ephemeral bind that is then released keeps the test from
        // colliding with whatever else the machine is running.
        let port = {
            let probe = UdpSocket::bind("[::1]:0").await.unwrap();
            probe.local_addr().unwrap().port()
        };
        let server_addr = format!("[::1]:{port}");

        let settings = TurnSettings {
            realm: REALM.to_string(),
            interfaces: vec![TurnInterface {
                transport: TurnTransport::UDP,
                listen: server_addr.clone(),
                external: server_addr.clone(),
            }],
            relay_min_port: 51000,
            relay_max_port: 51099,
            ..TurnSettings::default()
        };
        let state = startup_turn_server(
            settings,
            Arc::new(StaticCredential),
            Arc::new(RwLock::new(Statistics::default())),
            Arc::new(AllowAllRelayTrafficGate),
        )
        .await
        .expect("an IPv6 TURN server must start");

        let client = Client::new(ClientConfig {
            stun_serv_addr: server_addr.clone(),
            turn_serv_addr: server_addr.clone(),
            username: USER.to_string(),
            password: PASS.to_string(),
            realm: REALM.to_string(),
            software: String::new(),
            rto_in_ms: 0,
            conn: Arc::new(UdpSocket::bind("[::1]:0").await.unwrap()),
            vnet: None,
        })
        .await
        .expect("client");
        client.listen().await.expect("client listen");

        let relayed = client.allocate().await.expect("an IPv6 allocation");
        let relayed_addr = relayed.local_addr().expect("the relayed address");
        assert!(
            relayed_addr.is_ipv6(),
            "an IPv6 interface must hand out an IPv6 relay, got {relayed_addr}"
        );
        assert_eq!(relayed_addr.ip(), IpAddr::V6(Ipv6Addr::LOCALHOST));

        // The relay has to move bytes both ways. Sending is what fails when the
        // relay socket is IPv4 behind an advertised IPv6 address: the peer is an
        // IPv6 address the socket cannot reach.
        let peer = UdpSocket::bind("[::1]:0").await.unwrap();
        let peer_addr = peer.local_addr().unwrap();
        relayed
            .send_to(b"ping", peer_addr)
            .await
            .expect("the relay must reach an IPv6 peer");

        let mut buf = [0u8; 64];
        let (n, from) = tokio::time::timeout(Duration::from_secs(5), peer.recv_from(&mut buf))
            .await
            .expect("the peer must receive the relayed datagram")
            .unwrap();
        assert_eq!(&buf[..n], b"ping");
        assert_eq!(
            from, relayed_addr,
            "the peer sees the relay, not the client"
        );

        // And back: a permission for this peer now exists, so the return path is
        // relayed too.
        peer.send_to(b"pong", relayed_addr).await.unwrap();
        let (n, _) = tokio::time::timeout(Duration::from_secs(5), relayed.recv_from(&mut buf))
            .await
            .expect("the client must receive the peer's reply")
            .unwrap();
        assert_eq!(&buf[..n], b"pong");

        client.close().await.expect("client close");
        state.server.close().await.expect("server close");
    }
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
