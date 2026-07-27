//! Which of the configured interfaces this host can actually relay on.
//!
//! An interface is a pair of strings an operator typed, so the two questions
//! "what do we bind" and "what do we advertise" have to be answered from the
//! same inspection — otherwise a host binds one set of sockets and hands peers
//! another, and every ICE candidate pointing at the difference is wasted.
//!
//! Rejections are reported rather than repaired. Substituting a default for an
//! address that failed to parse is how a host ends up advertising a wildcard
//! address that no peer can dial, while its logs say it started successfully.

use std::net::SocketAddr;

use serde::Serialize;
use utoipa::ToSchema;

use crate::model::{TurnInterface, TurnTransport};

/// Why a configured interface is not served.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TurnInterfaceFault {
    /// Only UDP relaying is implemented; a TCP entry is neither bound nor
    /// advertised.
    TransportNotServed,
    /// `listen` is not an `IP:port` pair.
    ListenNotAnAddress,
    /// `external` is not an `IP:port` pair. Host names are included here: they
    /// are never resolved, matching what the manager accepts.
    ExternalNotAnAddress,
    /// `external` parses but names something no peer can dial — a wildcard
    /// address, or port zero.
    ExternalNotDialable,
}

/// A configured interface that will not be served, and what to change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RejectedTurnInterface {
    /// Position in the configured interface list, so an operator can find the
    /// entry even when several look alike.
    pub index: usize,
    /// The entry as configured, echoed back unrepaired.
    pub interface: TurnInterface,
    pub fault: TurnInterfaceFault,
    /// What is wrong and what a working entry looks like.
    pub detail: String,
}

/// A configured interface this host can bind and advertise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServableTurnInterface {
    pub listen: SocketAddr,
    pub external: SocketAddr,
}

impl ServableTurnInterface {
    /// The entry in canonical form — the same addresses, printed the way they
    /// have to appear on the wire. IPv6 literals gain their brackets here, which
    /// is what makes an IPv6 `turn:` URL dialable.
    pub fn canonical(&self) -> TurnInterface {
        TurnInterface {
            transport: TurnTransport::UDP,
            listen: self.listen.to_string(),
            external: self.external.to_string(),
        }
    }

    /// `turn:{external}?transport=udp` for this interface.
    pub fn turn_url(&self) -> String {
        format!("turn:{}?transport=udp", self.external)
    }
}

/// The configured interfaces split into what will be served and what will not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnInterfacePlan {
    pub servable: Vec<ServableTurnInterface>,
    pub rejected: Vec<RejectedTurnInterface>,
}

impl TurnInterfacePlan {
    /// Name every rejected entry once.
    ///
    /// A misconfigured interface is silent otherwise: the host keeps serving
    /// whatever else parsed, so nothing fails and nobody looks. This is the only
    /// place the operator learns that an address they configured is not in use.
    pub fn report_rejections(&self) {
        for rejection in &self.rejected {
            log::error!(
                "TURN interface #{} is not served: {}",
                rejection.index,
                rejection.detail
            );
        }
    }
}

/// Split `interfaces` into the ones this host serves and the ones it refuses.
///
/// Pure and cheap, so the caller deciding whether to run a runtime and the
/// caller building one can each call it and be certain they agree.
pub fn plan_turn_interfaces(interfaces: &[TurnInterface]) -> TurnInterfacePlan {
    let mut plan = TurnInterfacePlan::default();
    for (index, interface) in interfaces.iter().enumerate() {
        match inspect(interface) {
            Ok(servable) => plan.servable.push(servable),
            Err((fault, detail)) => plan.rejected.push(RejectedTurnInterface {
                index,
                interface: interface.clone(),
                fault,
                detail,
            }),
        }
    }
    plan
}

/// Inspect one entry, returning either what to bind or why not to.
fn inspect(
    interface: &TurnInterface,
) -> Result<ServableTurnInterface, (TurnInterfaceFault, String)> {
    if interface.transport != TurnTransport::UDP {
        return Err((
            TurnInterfaceFault::TransportNotServed,
            format!(
                "transport {:?} is not relayed; only UDP is served, so remove this entry \
                 or set its transport to udp",
                interface.transport
            ),
        ));
    }

    let listen: SocketAddr = interface.listen.parse().map_err(|_| {
        (
            TurnInterfaceFault::ListenNotAnAddress,
            format!(
                "listen \"{}\" is not an IP:port pair; write 0.0.0.0:3478 to listen on every \
                 IPv4 address, or [::]:3478 for IPv6",
                interface.listen
            ),
        )
    })?;

    let external: SocketAddr = interface.external.parse().map_err(|_| {
        (
            TurnInterfaceFault::ExternalNotAnAddress,
            format!(
                "external \"{}\" is not an IP:port pair; write the address peers dial, as \
                 203.0.113.7:3478 or [2001:db8::1]:3478 — host names are not resolved",
                interface.external
            ),
        )
    })?;

    if external.ip().is_unspecified() {
        return Err((
            TurnInterfaceFault::ExternalNotDialable,
            format!(
                "external \"{}\" is a wildcard address; peers cannot dial it, so give the \
                 address this host is reachable at",
                interface.external
            ),
        ));
    }
    if external.port() == 0 {
        return Err((
            TurnInterfaceFault::ExternalNotDialable,
            format!(
                "external \"{}\" has no port; give the port peers dial, usually the same one \
                 as listen",
                interface.external
            ),
        ));
    }

    Ok(ServableTurnInterface { listen, external })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interface(transport: TurnTransport, listen: &str, external: &str) -> TurnInterface {
        TurnInterface {
            transport,
            listen: listen.into(),
            external: external.into(),
        }
    }

    fn udp(listen: &str, external: &str) -> TurnInterface {
        interface(TurnTransport::UDP, listen, external)
    }

    fn fault_of(interface: TurnInterface) -> TurnInterfaceFault {
        let plan = plan_turn_interfaces(&[interface]);
        assert!(plan.servable.is_empty(), "the entry must not be served");
        plan.rejected[0].fault
    }

    /// An IPv6 `external` is the case the previous parser could not express at
    /// all: it split on `:` and took the first piece, so `[2001:db8::1]:3478`
    /// yielded `[`, which then failed to parse and was substituted with a
    /// wildcard. The entry now survives intact, brackets and all.
    #[test]
    fn an_ipv6_interface_is_served_with_its_address_intact() {
        let plan = plan_turn_interfaces(&[udp("[::]:3478", "[2001:db8::1]:3478")]);
        assert!(plan.rejected.is_empty(), "{:?}", plan.rejected);
        let served = plan.servable[0];
        assert!(served.external.is_ipv6());
        assert_eq!(served.external.port(), 3478);
        assert_eq!(served.canonical().external, "[2001:db8::1]:3478");
        assert_eq!(served.turn_url(), "turn:[2001:db8::1]:3478?transport=udp");
    }

    /// The ordinary IPv4 entry keeps working, printed the way it was written.
    #[test]
    fn an_ipv4_interface_is_served_unchanged() {
        let plan = plan_turn_interfaces(&[udp("0.0.0.0:3478", "203.0.113.7:3478")]);
        assert!(plan.rejected.is_empty());
        assert_eq!(
            plan.servable[0].canonical(),
            udp("0.0.0.0:3478", "203.0.113.7:3478")
        );
    }

    /// Each way of getting an interface wrong is reported as its own fault, so
    /// the operator is told which field to fix rather than that "TURN failed".
    #[test]
    fn every_way_of_being_unservable_is_named() {
        assert_eq!(
            fault_of(interface(
                TurnTransport::TCP,
                "0.0.0.0:3478",
                "203.0.113.7:3478"
            )),
            TurnInterfaceFault::TransportNotServed
        );
        assert_eq!(
            fault_of(udp("not-an-address", "203.0.113.7:3478")),
            TurnInterfaceFault::ListenNotAnAddress
        );
        assert_eq!(
            fault_of(udp("0.0.0.0:3478", "relay.example.com:3478")),
            TurnInterfaceFault::ExternalNotAnAddress,
            "host names are not resolved, matching what the manager accepts"
        );
        assert_eq!(
            fault_of(udp("0.0.0.0:3478", "203.0.113.7")),
            TurnInterfaceFault::ExternalNotAnAddress,
            "an address without a port is not an endpoint"
        );
        assert_eq!(
            fault_of(udp("0.0.0.0:3478", "0.0.0.0:3478")),
            TurnInterfaceFault::ExternalNotDialable,
            "the wildcard the old parser silently substituted is itself invalid"
        );
        assert_eq!(
            fault_of(udp("0.0.0.0:3478", "[::]:3478")),
            TurnInterfaceFault::ExternalNotDialable
        );
        assert_eq!(
            fault_of(udp("0.0.0.0:3478", "203.0.113.7:0")),
            TurnInterfaceFault::ExternalNotDialable
        );
    }

    /// A listen port of zero is an ephemeral port, not a misconfiguration — the
    /// kernel picks one. Only `external` has to name something dialable.
    #[test]
    fn an_ephemeral_listen_port_is_allowed() {
        let plan = plan_turn_interfaces(&[udp("127.0.0.1:0", "203.0.113.7:3478")]);
        assert!(plan.rejected.is_empty(), "{:?}", plan.rejected);
    }

    /// One bad entry does not unserve the good ones, and the report points at
    /// the offending position rather than at the list.
    #[test]
    fn a_bad_entry_is_isolated_and_located() {
        let plan = plan_turn_interfaces(&[
            udp("0.0.0.0:3478", "203.0.113.7:3478"),
            interface(TurnTransport::TCP, "0.0.0.0:3478", "203.0.113.7:3478"),
            udp("[::]:3479", "[2001:db8::1]:3479"),
        ]);
        assert_eq!(plan.servable.len(), 2, "the two UDP entries stay served");
        assert_eq!(plan.rejected.len(), 1);
        assert_eq!(plan.rejected[0].index, 1, "the middle entry is the bad one");
        assert_eq!(
            plan.rejected[0].interface.transport,
            TurnTransport::TCP,
            "the entry is echoed back as configured"
        );
    }

    /// Every rejection has to say what to change, not just that something is
    /// wrong: this text is the whole migration diagnostic for a host whose
    /// interfaces were accepted by an earlier build that never validated them.
    #[test]
    fn every_rejection_explains_the_fix() {
        let plan = plan_turn_interfaces(&[
            interface(TurnTransport::TCP, "0.0.0.0:3478", "203.0.113.7:3478"),
            udp("nonsense", "203.0.113.7:3478"),
            udp("0.0.0.0:3478", "relay.example.com:3478"),
            udp("0.0.0.0:3478", "0.0.0.0:3478"),
        ]);
        assert_eq!(plan.rejected.len(), 4);
        for rejection in &plan.rejected {
            let detail = &rejection.detail;
            assert!(
                detail.contains(';'),
                "{detail:?} states the problem but not the fix"
            );
            let offending = match rejection.fault {
                TurnInterfaceFault::TransportNotServed => "udp",
                TurnInterfaceFault::ListenNotAnAddress => &rejection.interface.listen,
                TurnInterfaceFault::ExternalNotAnAddress
                | TurnInterfaceFault::ExternalNotDialable => &rejection.interface.external,
            };
            assert!(
                detail.contains(offending),
                "{detail:?} does not quote {offending:?}"
            );
        }
    }
}
