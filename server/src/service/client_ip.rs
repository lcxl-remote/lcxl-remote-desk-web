//! Trusted client-IP extraction and network-prefix rate-limit identities.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use actix_web::HttpRequest;
use ipnet::IpNet;

pub const TRUSTED_PROXIES_ENV: &str = "LRD_TRUSTED_PROXIES";
pub const IPV6_PREFIX_LEN_ENV: &str = "LRD_AUTH_IPV6_PREFIX_LEN";
pub const TRUST_ALL_TOKEN: &str = "*";
pub const DEFAULT_IPV6_PREFIX_LEN: u8 = 64;

#[derive(Debug, Clone)]
pub struct TrustedProxies {
    nets: Vec<IpNet>,
    trust_all: bool,
}

impl Default for TrustedProxies {
    fn default() -> Self {
        Self {
            nets: vec![
                "127.0.0.0/8".parse().expect("valid loopback network"),
                "::1/128".parse().expect("valid loopback network"),
            ],
            trust_all: false,
        }
    }
}

impl TrustedProxies {
    pub fn parse(spec: &str) -> Self {
        let mut parsed = Self::default();
        for token in spec.split([',', ' ', '\t', '\n']) {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            if token == TRUST_ALL_TOKEN {
                parsed.trust_all = true;
                continue;
            }
            if let Ok(net) = token.parse::<IpNet>() {
                parsed.nets.push(net);
            } else if let Ok(ip) = token.parse::<IpAddr>() {
                parsed.nets.push(IpNet::from(normalize_ip(ip)));
            } else {
                log::warn!("Ignoring invalid {TRUSTED_PROXIES_ENV} entry: {token}");
            }
        }
        parsed
    }

    pub fn from_env() -> Self {
        Self::parse(
            std::env::var(TRUSTED_PROXIES_ENV)
                .unwrap_or_default()
                .trim(),
        )
    }

    fn contains(&self, ip: IpAddr) -> bool {
        let ip = normalize_ip(ip);
        self.trust_all || self.nets.iter().any(|net| net.contains(&ip))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NetworkKey {
    V4(Ipv4Addr),
    V6 { prefix: Ipv6Addr, prefix_len: u8 },
}

impl fmt::Display for NetworkKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::V4(ip) => write!(f, "{ip}/32"),
            Self::V6 { prefix, prefix_len } => write!(f, "{prefix}/{prefix_len}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClientIpExtractor {
    trusted: TrustedProxies,
    ipv6_prefix_len: u8,
}

impl Default for ClientIpExtractor {
    fn default() -> Self {
        Self {
            trusted: TrustedProxies::default(),
            ipv6_prefix_len: DEFAULT_IPV6_PREFIX_LEN,
        }
    }
}

impl ClientIpExtractor {
    pub fn new(trusted: TrustedProxies, ipv6_prefix_len: u8) -> Result<Self, String> {
        if !(1..=128).contains(&ipv6_prefix_len) {
            return Err(format!("{IPV6_PREFIX_LEN_ENV} must be between 1 and 128"));
        }
        Ok(Self {
            trusted,
            ipv6_prefix_len,
        })
    }

    pub fn from_env() -> Result<Self, String> {
        let ipv6_prefix_len = match std::env::var(IPV6_PREFIX_LEN_ENV) {
            Ok(value) => value.trim().parse::<u8>().map_err(|_| {
                format!("{IPV6_PREFIX_LEN_ENV} must be an integer between 1 and 128")
            })?,
            Err(std::env::VarError::NotPresent) => DEFAULT_IPV6_PREFIX_LEN,
            Err(error) => return Err(format!("failed to read {IPV6_PREFIX_LEN_ENV}: {error}")),
        };
        Self::new(TrustedProxies::from_env(), ipv6_prefix_len)
    }

    pub fn extract(&self, req: &HttpRequest) -> IpAddr {
        let peer = req.peer_addr().map(|addr| normalize_ip(addr.ip()));
        let xff = req
            .headers()
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok());
        self.resolve(peer, xff)
    }

    pub fn network_key(&self, req: &HttpRequest) -> NetworkKey {
        self.key_for(self.extract(req))
    }

    pub fn key_for(&self, ip: IpAddr) -> NetworkKey {
        match normalize_ip(ip) {
            IpAddr::V4(ip) => NetworkKey::V4(ip),
            IpAddr::V6(ip) => {
                let prefix_len = self.ipv6_prefix_len;
                let mask = if prefix_len == 128 {
                    u128::MAX
                } else {
                    u128::MAX << (128 - prefix_len)
                };
                NetworkKey::V6 {
                    prefix: Ipv6Addr::from(u128::from(ip) & mask),
                    prefix_len,
                }
            }
        }
    }

    pub fn resolve(&self, peer: Option<IpAddr>, xff: Option<&str>) -> IpAddr {
        let peer = normalize_ip(peer.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        if self.trusted.trust_all {
            return xff
                .and_then(|value| value.split(',').find_map(parse_forwarded_hop))
                .unwrap_or(peer);
        }
        if !self.trusted.contains(peer) {
            return peer;
        }
        let Some(xff) = xff else {
            return peer;
        };
        for hop in xff.rsplit(',') {
            let Some(ip) = parse_forwarded_hop(hop) else {
                continue;
            };
            if !self.trusted.contains(ip) {
                return ip;
            }
        }
        peer
    }
}

fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ip)),
        ip => ip,
    }
}

fn parse_forwarded_hop(hop: &str) -> Option<IpAddr> {
    let hop = hop.trim();
    if let Ok(ip) = hop.parse::<IpAddr>() {
        return Some(normalize_ip(ip));
    }
    hop.parse::<SocketAddr>()
        .ok()
        .map(|addr| normalize_ip(addr.ip()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    #[test]
    fn default_trusts_loopback_and_normalizes_mapped_ipv6() {
        let extractor = ClientIpExtractor::default();
        assert_eq!(
            extractor.resolve(Some(ip("127.0.0.1")), Some("198.51.100.7")),
            ip("198.51.100.7")
        );
        assert_eq!(
            extractor.resolve(Some(ip("::ffff:127.0.0.1")), Some("198.51.100.8")),
            ip("198.51.100.8")
        );
        assert_eq!(
            extractor.key_for(ip("::ffff:127.0.0.1")),
            extractor.key_for(ip("127.0.0.1"))
        );
        assert_eq!(
            extractor.resolve(Some(ip("::1")), Some("198.51.100.9")),
            ip("198.51.100.9")
        );
        assert_eq!(
            extractor.resolve(Some(ip("::ffff:192.168.1.9")), Some("203.0.113.8")),
            ip("192.168.1.9")
        );
    }

    #[test]
    fn untrusted_peer_cannot_forge_xff() {
        let extractor = ClientIpExtractor::default();
        assert_eq!(
            extractor.resolve(Some(ip("192.168.1.9")), Some("203.0.113.4")),
            ip("192.168.1.9")
        );
        for peer in ["fd00::1", "fe80::1"] {
            assert_eq!(
                extractor.resolve(Some(ip(peer)), Some("2001:db8::7")),
                ip(peer)
            );
        }
    }

    #[test]
    fn trusted_chain_walks_from_right_to_left() {
        let extractor =
            ClientIpExtractor::new(TrustedProxies::parse("10.0.0.0/8"), DEFAULT_IPV6_PREFIX_LEN)
                .unwrap();
        assert_eq!(
            extractor.resolve(Some(ip("10.0.0.2")), Some("198.51.100.9, 10.0.0.1")),
            ip("198.51.100.9")
        );
        assert_eq!(
            extractor.resolve(Some(ip("10.0.0.2")), Some("198.51.100.10:4321")),
            ip("198.51.100.10")
        );
    }

    #[test]
    fn ipv6_addresses_share_the_configured_prefix() {
        let extractor = ClientIpExtractor::default();
        assert_eq!(
            extractor.key_for(ip("2001:db8:1:2::1")),
            extractor.key_for(ip("2001:db8:1:2::ffff"))
        );
        assert_ne!(
            extractor.key_for(ip("2001:db8:1:2::1")),
            extractor.key_for(ip("2001:db8:1:3::1"))
        );

        let prefix_56 = ClientIpExtractor::new(TrustedProxies::default(), 56).unwrap();
        assert_eq!(
            prefix_56.key_for(ip("2001:db8:1:200::1")),
            prefix_56.key_for(ip("2001:db8:1:2ff::1"))
        );
        assert_ne!(
            prefix_56.key_for(ip("2001:db8:1:200::1")),
            prefix_56.key_for(ip("2001:db8:1:300::1"))
        );
    }

    #[test]
    fn malformed_xff_and_missing_peer_fall_back_without_trusting_the_header() {
        let extractor = ClientIpExtractor::default();
        assert_eq!(
            extractor.resolve(Some(ip("127.0.0.1")), Some("not-an-ip")),
            ip("127.0.0.1")
        );
        assert_eq!(
            extractor.resolve(None, Some("198.51.100.7")),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
    }

    #[test]
    fn trust_all_requires_an_explicit_wildcard() {
        let peer = ip("192.168.50.9");
        assert_eq!(
            ClientIpExtractor::default().resolve(Some(peer), Some("198.51.100.7")),
            peer
        );
        let trust_all =
            ClientIpExtractor::new(TrustedProxies::parse("*"), DEFAULT_IPV6_PREFIX_LEN).unwrap();
        assert_eq!(
            trust_all.resolve(Some(peer), Some("198.51.100.7, 10.0.0.2")),
            ip("198.51.100.7")
        );
    }

    #[test]
    fn docker_source_paths_do_not_invent_a_client_address() {
        let default = ClientIpExtractor::default();
        assert_eq!(
            default.resolve(Some(ip("198.51.100.20")), None),
            ip("198.51.100.20")
        );

        let bridge_proxy =
            ClientIpExtractor::new(TrustedProxies::parse("172.17.0.1"), DEFAULT_IPV6_PREFIX_LEN)
                .unwrap();
        assert_eq!(
            bridge_proxy.resolve(Some(ip("172.17.0.1")), Some("198.51.100.21")),
            ip("198.51.100.21")
        );

        assert_eq!(
            default.resolve(Some(ip("172.17.0.1")), None),
            ip("172.17.0.1")
        );
    }

    #[test]
    fn invalid_prefix_is_rejected() {
        assert!(ClientIpExtractor::new(TrustedProxies::default(), 0).is_err());
    }
}
