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

        let cursor = self.cursor.fetch_add(1, Ordering::Relaxed);
        for port in walk_ports(self.min_port, self.max_port, cursor) {
            if let Ok(conn) = self.net.bind(SocketAddr::new(bind_ip, port)).await {
                return self.advertise(conn);
            }
        }
        Err(turn::Error::ErrMaxRetriesExceeded)
    }
}

/// The ports one allocation walks, in order: the whole configured range, rotated
/// so consecutive allocations do not all start at its first port. A range is
/// often only a few ports wide, and sampling it at random can miss the one port
/// that is free.
///
/// The arithmetic runs in `u32` rather than in the domain the ports live in: a
/// range may be wider than half of `u16`, and `start + offset` then leaves that
/// domain before the modulo brings it back. Each port that comes out is
/// `min_port + (… % width)`, which is `max_port` at most, so narrowing it again
/// is exact.
fn walk_ports(min_port: u16, max_port: u16, cursor: u16) -> impl Iterator<Item = u16> {
    let min = u32::from(min_port);
    // A range whose ends are inverted is refused by `validate` before anything
    // serves; saturating here keeps a caller that skipped it walking one port
    // rather than panicking.
    let width = u32::from(max_port).saturating_sub(min) + 1;
    let start = u32::from(cursor) % width;
    (0..width).map(move |offset| (min + (start + offset) % width) as u16)
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

    /// The widest range `validate` accepts, walked from a start near its end —
    /// where `start + offset` runs past what a port can hold. Doing that in the
    /// port's own width overflows: the walk panics in a debug build and, in a
    /// release one, wraps and revisits ports it has already tried while never
    /// reaching others.
    #[test]
    fn the_widest_range_is_walked_from_any_start() {
        let ports: Vec<u16> = walk_ports(1, 65535, 65534).collect();

        assert_eq!(ports.len(), 65535, "every port in the range is visited");
        assert_eq!(ports[0], 65535, "the walk starts where the cursor points");
        assert_eq!(ports[1], 1, "and wraps to the bottom of the range");
    }

    /// Exhaustion has to mean exhaustion: a walk that visited a port twice would
    /// report the range full while something in it was still free.
    #[test]
    fn a_walk_visits_every_port_exactly_once() {
        for (min, max, cursor) in [
            (1u16, 65535u16, 40_000u16),
            (49_000, 49_099, 7),
            (80, 80, 0),
        ] {
            let ports: Vec<u16> = walk_ports(min, max, cursor).collect();
            let mut unique = ports.clone();
            unique.sort_unstable();
            unique.dedup();

            assert_eq!(unique.len(), ports.len(), "{min}..={max} revisited a port");
            assert_eq!(*unique.first().unwrap(), min);
            assert_eq!(*unique.last().unwrap(), max);
        }
    }

    /// The rotation is the point of the cursor: two consecutive allocations must
    /// not both begin by contending for the same port.
    #[test]
    fn consecutive_walks_begin_at_different_ports() {
        let first = walk_ports(49_000, 49_099, 0).next().unwrap();
        let second = walk_ports(49_000, 49_099, 1).next().unwrap();

        assert_ne!(first, second);
    }
}
