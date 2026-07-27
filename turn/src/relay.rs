//! Relay socket allocation for one interface.
//!
//! The relayed address a client is handed is `relay_address` plus the port of a
//! socket this host binds, so the socket has to be in the same address family as
//! the address advertised with it. Binding a wildcard of the wrong family — an
//! IPv4 socket behind an advertised IPv6 relay — produces an allocation that
//! looks successful and that no peer can ever reach.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};

use async_trait::async_trait;
use turn::relay::RelayAddressGenerator;
use webrtc_util::Conn;
use webrtc_util::vnet::net::Net;

/// Allocates relay sockets in the address family of the address it advertises.
pub struct FamilyPinnedRelay {
    /// The IP handed to clients as the relayed address.
    relay_address: IpAddr,
    min_port: u16,
    max_port: u16,
    net: Arc<Net>,
    /// Rotating start offset, so consecutive allocations spread across the range
    /// instead of contending for its first port.
    cursor: AtomicU16,
}

impl FamilyPinnedRelay {
    pub fn new(relay_address: IpAddr, min_port: u16, max_port: u16) -> Self {
        Self {
            relay_address,
            min_port,
            max_port,
            net: Arc::new(Net::new(None)),
            cursor: AtomicU16::new(0),
        }
    }

    /// The wildcard address to bind relay sockets on.
    ///
    /// A client may state which family it wants (RFC 6156). The crate reports
    /// that as a single `use_ipv4` flag which is `true` both when IPv4 was asked
    /// for and when nothing was asked for at all, and those cannot be told
    /// apart — so an interface that advertises an IPv6 relay treats the flag as
    /// "no preference" and serves its own family. An IPv4 interface, having no
    /// IPv6 address to advertise, refuses an explicit IPv6 request instead of
    /// substituting a relay the client did not ask for.
    fn bind_ip(&self, use_ipv4: bool) -> Result<IpAddr, turn::Error> {
        match self.relay_address {
            IpAddr::V6(_) => Ok(IpAddr::V6(Ipv6Addr::UNSPECIFIED)),
            IpAddr::V4(_) if use_ipv4 => Ok(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            IpAddr::V4(_) => Err(turn::Error::ErrPeerAddressFamilyMismatch),
        }
    }

    /// Pair a bound socket with the address to advertise for it.
    fn advertise(
        &self,
        conn: Arc<dyn Conn + Send + Sync>,
    ) -> Result<(Arc<dyn Conn + Send + Sync>, SocketAddr), turn::Error> {
        let mut relay_addr = conn.local_addr()?;
        relay_addr.set_ip(self.relay_address);
        Ok((conn, relay_addr))
    }
}

#[async_trait]
impl RelayAddressGenerator for FamilyPinnedRelay {
    fn validate(&self) -> Result<(), turn::Error> {
        if self.min_port == 0 {
            Err(turn::Error::ErrMinPortNotZero)
        } else if self.max_port == 0 {
            Err(turn::Error::ErrMaxPortNotZero)
        } else if self.max_port < self.min_port {
            Err(turn::Error::ErrMaxPortLessThanMinPort)
        } else {
            Ok(())
        }
    }

    async fn allocate_conn(
        &self,
        use_ipv4: bool,
        requested_port: u16,
    ) -> Result<(Arc<dyn Conn + Send + Sync>, SocketAddr), turn::Error> {
        let bind_ip = self.bind_ip(use_ipv4)?;

        if requested_port != 0 {
            let conn = self
                .net
                .bind(SocketAddr::new(bind_ip, requested_port))
                .await?;
            return self.advertise(conn);
        }

        // Walk the whole range from a rotating start rather than sampling it:
        // a range is often only a few ports wide, and a handful of random draws
        // can miss the one port that is free.
        let width = self.max_port - self.min_port + 1;
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % width;
        for offset in 0..width {
            let port = self.min_port + (start + offset) % width;
            if let Ok(conn) = self.net.bind(SocketAddr::new(bind_ip, port)).await {
                return self.advertise(conn);
            }
        }
        Err(turn::Error::ErrMaxRetriesExceeded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay(ip: &str, min_port: u16, max_port: u16) -> FamilyPinnedRelay {
        FamilyPinnedRelay::new(ip.parse().unwrap(), min_port, max_port)
    }

    /// An IPv6 relay must bind an IPv6 socket. The default request carries no
    /// family preference and reaches us as `use_ipv4 = true`; honouring that
    /// literally would bind an IPv4 socket and advertise an IPv6 address for
    /// it — an allocation that succeeds and cannot carry a byte.
    #[tokio::test]
    async fn an_ipv6_relay_binds_an_ipv6_socket_even_without_a_stated_preference() {
        let relay = relay("::1", 0, 0);
        for use_ipv4 in [true, false] {
            let bind = relay.bind_ip(use_ipv4).expect("an IPv6 relay always binds");
            assert!(bind.is_ipv6(), "use_ipv4 = {use_ipv4}");
            assert!(bind.is_unspecified());
        }
    }

    /// An IPv4 interface has no IPv6 address to advertise, so an explicit IPv6
    /// request is refused rather than answered with an IPv4 relay.
    #[tokio::test]
    async fn an_ipv4_relay_refuses_an_explicit_ipv6_request() {
        let relay = relay("203.0.113.7", 0, 0);
        assert!(relay.bind_ip(true).unwrap().is_ipv4());
        assert!(matches!(
            relay.bind_ip(false),
            Err(turn::Error::ErrPeerAddressFamilyMismatch)
        ));
    }

    /// The advertised address is the configured one, carrying the port of the
    /// socket actually bound — that pairing is the whole point of the type.
    #[tokio::test]
    async fn the_advertised_address_pairs_the_configured_ip_with_the_bound_port() {
        let relay = relay("203.0.113.7", 49000, 49099);
        let (conn, advertised) = relay.allocate_conn(true, 0).await.expect("a free port");
        assert_eq!(advertised.ip(), "203.0.113.7".parse::<IpAddr>().unwrap());
        assert_eq!(advertised.port(), conn.local_addr().unwrap().port());
        assert!(
            (49000..=49099).contains(&advertised.port()),
            "the port must come from the configured range"
        );
        assert!(conn.local_addr().unwrap().ip().is_unspecified());
    }

    /// A one-port range must still allocate that port, and report exhaustion
    /// once it is taken rather than looping.
    #[tokio::test]
    async fn a_single_port_range_is_used_then_reported_exhausted() {
        let relay = relay("203.0.113.7", 49123, 49123);
        let (held, advertised) = relay.allocate_conn(true, 0).await.expect("the only port");
        assert_eq!(advertised.port(), 49123);
        assert!(matches!(
            relay.allocate_conn(true, 0).await,
            Err(turn::Error::ErrMaxRetriesExceeded)
        ));
        drop(held);
    }

    /// A misconfigured port range is refused up front, where the server start
    /// can report it, rather than at the first allocation.
    #[test]
    fn an_impossible_port_range_is_rejected_before_serving() {
        assert!(relay("203.0.113.7", 1, 65535).validate().is_ok());
        assert!(matches!(
            relay("203.0.113.7", 0, 100).validate(),
            Err(turn::Error::ErrMinPortNotZero)
        ));
        assert!(matches!(
            relay("203.0.113.7", 100, 0).validate(),
            Err(turn::Error::ErrMaxPortNotZero)
        ));
        assert!(matches!(
            relay("203.0.113.7", 200, 100).validate(),
            Err(turn::Error::ErrMaxPortLessThanMinPort)
        ));
    }
}
