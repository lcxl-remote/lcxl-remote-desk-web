//! SSRF / transport judgment core for outbound dials (single source of truth
//! shared by the manager, the open-source signal orchestrator, and the desk
//! server's signaling / connection-verify path).
//!
//! Two related concerns live here as pure functions:
//!
//! - **Model-provider SSRF** ([`ProviderSsrfMode`], [`check_provider_url`],
//!   [`check_resolved_ip`]): users configure a provider `base_url` that the server
//!   then dials. In a multi-tenant deployment any registered (untrusted) user can
//!   point that URL at an internal service or a cloud-metadata endpoint and read
//!   the response back — a classic SSRF. The mode decides whether a URL (write
//!   time) or a resolved IP (connect time, authoritative against DNS rebinding) is
//!   allowed.
//! - **Transport policy** ([`AddressClass`], [`classify_address`],
//!   [`check_transport`]): the connect-time judgment that additionally refuses a
//!   *plaintext* dial to a *public* address (used by the signaling proxy and the
//!   connection-verify probe), while always blocking the cloud-metadata floor and
//!   always permitting private / LAN targets over plaintext.
//!
//! It is intentionally dependency-light: only `url` for parsing and `std::net`
//! for address classification. No HTTP client, no TLS, no DNS, no config reading —
//! the callers own those and feed this module already-parsed inputs (the actix
//! `Resolve` adapter that runs [`check_transport`] per candidate lives in the desk
//! server, not here). Keeping it pure keeps it trivially unit-testable without a
//! network.

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// How aggressively to guard provider outbound dials. Configured per deployment;
/// each binary supplies its own sensible default (manager defaults to `Strict`,
/// the single-tenant open-source signal defaults to `Relaxed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ProviderSsrfMode {
    /// Public multi-tenant posture. Scheme must be `https`; private, loopback,
    /// link-local, ULA, CGNAT and cloud-metadata ranges are all rejected.
    Strict,
    /// Single-tenant / self-host posture. `http` is allowed and private/loopback
    /// targets (local model gateways like ollama / vLLM) are permitted, but the
    /// cloud-metadata hard floor is still enforced.
    Relaxed,
}

impl FromStr for ProviderSsrfMode {
    type Err = ();

    /// Parse from a config string (case-insensitive). Used when reading the mode
    /// from a stored system parameter / settings value.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "strict" => Ok(ProviderSsrfMode::Strict),
            "relaxed" => Ok(ProviderSsrfMode::Relaxed),
            _ => Err(()),
        }
    }
}

impl ProviderSsrfMode {
    /// Stable lowercase token for this mode (inverse of [`FromStr`]).
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderSsrfMode::Strict => "strict",
            ProviderSsrfMode::Relaxed => "relaxed",
        }
    }
}

impl fmt::Display for ProviderSsrfMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a provider target was rejected. Deliberately coarse: the message handed
/// back to an untrusted caller must not leak which internal host was probed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SsrfError {
    /// The string did not parse as a URL.
    InvalidUrl,
    /// The URL had no host component.
    MissingHost,
    /// The scheme is not permitted under the active mode (e.g. `http` in Strict,
    /// or any non-http(s) scheme).
    SchemeNotAllowed,
    /// The target address falls in a denied range for the active mode.
    BlockedAddress,
    /// A plaintext (non-TLS) scheme targets a public address while public-TLS
    /// enforcement is on. Kept distinct from [`SsrfError::BlockedAddress`] so a
    /// caller can surface a "use TLS" hint rather than an opaque "blocked".
    InsecureTransport,
}

impl fmt::Display for SsrfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            SsrfError::InvalidUrl => "provider URL is not a valid URL",
            SsrfError::MissingHost => "provider URL has no host",
            SsrfError::SchemeNotAllowed => "provider URL scheme is not allowed",
            SsrfError::BlockedAddress => "provider URL resolves to a blocked address",
            SsrfError::InsecureTransport => {
                "plaintext transport to a public address is not allowed"
            }
        };
        f.write_str(msg)
    }
}

impl std::error::Error for SsrfError {}

/// Validate a provider URL at write time (when a user saves a provider config).
///
/// The two policies compose **orthogonally**, exactly as at connect time (see
/// [`check_transport`]): `mode` governs whether a *private* target is reachable
/// (`Relaxed` allows local gateways, `Strict` does not), while `enforce_public_tls`
/// governs whether a *public* target may be dialed over *plaintext*. `http` and
/// `https` are both structurally valid schemes; a plaintext scheme is not rejected
/// on its own — only a public plaintext target under enforcement is. Any other
/// scheme is rejected.
///
/// An IP-literal host is judged immediately; a registered name (domain) is deferred
/// to the authoritative connect-time check because DNS can rebind between save and
/// dial.
pub fn check_provider_url(
    raw: &str,
    mode: ProviderSsrfMode,
    enforce_public_tls: bool,
) -> Result<(), SsrfError> {
    let url = url::Url::parse(raw).map_err(|_| SsrfError::InvalidUrl)?;
    let scheme = url.scheme().to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(SsrfError::SchemeNotAllowed);
    }
    let host = url.host_str().ok_or(SsrfError::MissingHost)?;
    check_transport_for_host(
        host,
        mode == ProviderSsrfMode::Relaxed,
        scheme == "https",
        enforce_public_tls,
    )
}

/// Validate a single resolved IP at connect time against the SSRF address floor
/// (private reachability by `mode`), ignoring the TLS policy. Retained for callers
/// that judge an already-resolved address without a scheme in hand; the transport
/// (TLS) policy is applied separately via [`check_transport`].
pub fn check_resolved_ip(ip: IpAddr, mode: ProviderSsrfMode) -> Result<(), SsrfError> {
    if is_blocked(ip, mode) {
        Err(SsrfError::BlockedAddress)
    } else {
        Ok(())
    }
}

/// Core address judgment for the active mode, after IPv4-mapped normalization.
fn is_blocked(ip: IpAddr, mode: ProviderSsrfMode) -> bool {
    let ip = normalize(ip);
    match mode {
        ProviderSsrfMode::Relaxed => is_metadata_floor(ip),
        ProviderSsrfMode::Strict => is_metadata_floor(ip) || is_private_or_loopback(ip),
    }
}

/// Normalize IPv4-mapped IPv6 (`::ffff:a.b.c.d`) back to IPv4 so that, e.g.,
/// `[::ffff:169.254.169.254]` cannot slip past the IPv4 metadata range check.
fn normalize(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

/// The cloud-metadata hard floor: rejected under both `Strict` and `Relaxed`.
/// Covers the most dangerous SSRF target (credential-bearing instance metadata)
/// plus unspecified addresses, across IPv4 and IPv6.
///
/// Maintenance model: the common case needs no per-vendor list, because the bulk
/// is the `169.254.0.0/16` link-local range holding the de-facto metadata address
/// `169.254.169.254` (shared by AWS, GCP, Azure, Oracle, Tencent Cloud, and most
/// others). Only a vendor that puts metadata on a NON-link-local address which
/// also falls inside a private/CGNAT/ULA range must be carved out individually
/// (otherwise it would be misclassified as private and let through) — the AWS
/// IPv6 address `fd00:ec2::254` below is one such carve-out.
///
/// Known outlier NOT yet covered here: Alibaba Cloud metadata `100.100.100.200`,
/// which sits inside CGNAT `100.64.0.0/10` and is therefore currently classified
/// as private rather than blocked. Whether to add it to this floor or make the
/// floor configurable is an open follow-up; the addresses are intentionally
/// hardcoded because this is a security floor an operator must not weaken.
fn is_metadata_floor(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            // 0.0.0.0/8 ("this host", includes the unspecified address) and
            // 169.254.0.0/16 link-local (covers 169.254.169.254 — AWS IMDSv1/v2,
            // GCP, Azure metadata).
            o[0] == 0 || (o[0] == 169 && o[1] == 254)
        }
        IpAddr::V6(v6) => {
            // Unspecified (::), fe80::/10 link-local (covers metadata reachable
            // via link-local), and the AWS IPv6 IMDS address fd00:ec2::254 — the
            // latter sits inside ULA (fc00::/7) which Relaxed otherwise permits,
            // so it must be carved out explicitly.
            v6.is_unspecified()
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6 == Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x254)
        }
    }
}

/// Private / loopback / CGNAT / ULA ranges, rejected only under `Strict`.
fn is_private_or_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()                                   // 127.0.0.0/8
                || v4.is_private()                             // 10/8, 172.16/12, 192.168/16
                || (o[0] == 100 && (64..=127).contains(&o[1])) // 100.64.0.0/10 CGNAT
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()                                   // ::1
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 ULA
        }
    }
}

/// Transport-security classification of a resolved candidate address, layered on
/// top of the SSRF ranges. Where [`check_resolved_ip`] answers "may I reach this
/// address at all", this answers "what transport policy applies", so a plaintext
/// dial to a *public* address can be blocked while private / loopback targets
/// (local gateways, self-hosted signaling on a LAN) stay reachable over plaintext.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressClass {
    /// The cloud-metadata hard floor (see [`is_metadata_floor`]). Never reachable,
    /// under any mode or switch — this is a security floor, not a TLS policy.
    AlwaysBlock,
    /// A private / loopback / CGNAT / ULA address (see [`is_private_or_loopback`]).
    /// Reachable over plaintext without any TLS requirement: these are LAN /
    /// local-gateway targets where TLS is commonly absent and the traffic never
    /// crosses an untrusted network.
    PrivateExempt,
    /// Any other (public / internet-routable) address. Plaintext to such a target
    /// crosses untrusted networks, so a public-TLS switch may require TLS.
    TlsRequired,
}

/// Classify a resolved IP for transport policy. Metadata floor wins first (it must
/// never be reachable), then private/loopback, else public. The IP is IPv4-mapped
/// normalized first so `::ffff:169.254.169.254` cannot slip past the floor.
pub fn classify_address(ip: IpAddr) -> AddressClass {
    let ip = normalize(ip);
    if is_metadata_floor(ip) {
        AddressClass::AlwaysBlock
    } else if is_private_or_loopback(ip) {
        AddressClass::PrivateExempt
    } else {
        AddressClass::TlsRequired
    }
}

/// The authoritative connect-time transport judgment, run per resolved candidate
/// just before the socket connects. It composes the SSRF address floor with the
/// public-plaintext TLS policy in one place so callers cannot get the layering
/// wrong:
///
/// - [`AddressClass::AlwaysBlock`] → always rejected (`BlockedAddress`), ignoring
///   both `allow_private` and `enforce_public_tls`. The metadata floor is not a
///   TLS policy and no switch may weaken it.
/// - [`AddressClass::PrivateExempt`] → reachable iff `allow_private` (the SSRF
///   mode: `Relaxed`/signaling allow it, `Strict` does not). No TLS requirement is
///   ever imposed on private targets.
/// - [`AddressClass::TlsRequired`] → reachable over TLS always; over plaintext only
///   when `enforce_public_tls` is off (an operator escape hatch). Otherwise
///   rejected with [`SsrfError::InsecureTransport`].
///
/// `scheme_is_tls` is fixed per dial (the caller knows the scheme it is about to
/// use — `wss`/`https` vs `ws`/`http`) and baked into the resolver, so the decision
/// is made on the single authoritative resolution with no second lookup.
pub fn check_transport(
    ip: IpAddr,
    allow_private: bool,
    scheme_is_tls: bool,
    enforce_public_tls: bool,
) -> Result<(), SsrfError> {
    match classify_address(ip) {
        AddressClass::AlwaysBlock => Err(SsrfError::BlockedAddress),
        AddressClass::PrivateExempt => {
            if allow_private {
                Ok(())
            } else {
                Err(SsrfError::BlockedAddress)
            }
        }
        AddressClass::TlsRequired => {
            if scheme_is_tls || !enforce_public_tls {
                Ok(())
            } else {
                Err(SsrfError::InsecureTransport)
            }
        }
    }
}

/// Parse a URL host component as an IP literal, tolerating the `[..]` bracket form
/// used for an IPv6 authority. Returns `None` for a registered name (a domain),
/// which needs DNS and is judged at connect time by the resolver.
pub fn host_as_ip_literal(host: &str) -> Option<IpAddr> {
    let trimmed = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    trimmed.parse::<IpAddr>().ok()
}

/// Connect-time transport judgment for a dial target's *host string*, applied
/// **before** the dial.
///
/// This closes a gap in the actix-tls connect pipeline: its resolver
/// short-circuits an IP-literal host (constructing the `SocketAddr` directly) and
/// never invokes the custom [`Resolve`] adapter that runs [`check_transport`] per
/// candidate. A literal target would therefore bypass the address floor and the
/// public-plaintext policy entirely. Calling this before every dial restores the
/// guard for literals:
///
/// - **IP-literal host** → judged immediately with [`check_transport`]. This is
///   authoritative for a literal: there is no DNS step, so the address cannot
///   rebind and a one-shot static check is equivalent to the per-candidate
///   connect-time check. `AlwaysBlock` is rejected regardless of the switches.
/// - **Registered name (domain) host** → `Ok(())`: actix-tls *does* invoke the
///   custom adapter for a domain, so each resolved candidate is judged there
///   (authoritative against DNS rebinding).
///
/// Argument order mirrors [`check_transport`].
pub fn check_transport_for_host(
    host: &str,
    allow_private: bool,
    scheme_is_tls: bool,
    enforce_public_tls: bool,
) -> Result<(), SsrfError> {
    match host_as_ip_literal(host) {
        Some(ip) => check_transport(ip, allow_private, scheme_is_tls, enforce_public_tls),
        None => Ok(()),
    }
}

/// Convenience over [`check_transport_for_host`] that extracts the host from a full
/// URL string. A URL that does not parse, or has no host, is treated as `Ok(())`
/// (deferred): the dial that follows fails on its own if the URL is unusable.
/// `scheme_is_tls` is still supplied by the caller because the TLS-ness of a scheme
/// is caller-specific (`wss`/`https` vs `ws`/`http`).
pub fn check_transport_for_url(
    url: &str,
    allow_private: bool,
    scheme_is_tls: bool,
    enforce_public_tls: bool,
) -> Result<(), SsrfError> {
    match url::Url::parse(url.trim())
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
    {
        Some(host) => {
            check_transport_for_host(&host, allow_private, scheme_is_tls, enforce_public_tls)
        }
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn mode_roundtrips_via_str() {
        for m in [ProviderSsrfMode::Strict, ProviderSsrfMode::Relaxed] {
            assert_eq!(ProviderSsrfMode::from_str(m.as_str()), Ok(m));
        }
        assert_eq!(
            ProviderSsrfMode::from_str("STRICT"),
            Ok(ProviderSsrfMode::Strict)
        );
        assert_eq!(
            ProviderSsrfMode::from_str(" relaxed "),
            Ok(ProviderSsrfMode::Relaxed)
        );
        assert!(ProviderSsrfMode::from_str("nonsense").is_err());
        // `off` was removed (it was an escape hatch that bypassed the metadata
        // floor); it no longer parses to any mode.
        assert!(ProviderSsrfMode::from_str("off").is_err());
    }

    #[test]
    fn mode_serde_is_lowercase() {
        let json = serde_json::to_string(&ProviderSsrfMode::Strict).unwrap();
        assert_eq!(json, "\"strict\"");
        let parsed: ProviderSsrfMode = serde_json::from_str("\"relaxed\"").unwrap();
        assert_eq!(parsed, ProviderSsrfMode::Relaxed);
    }

    #[test]
    fn metadata_floor_blocked_in_both_strict_and_relaxed() {
        for mode in [ProviderSsrfMode::Strict, ProviderSsrfMode::Relaxed] {
            // IPv4 cloud metadata.
            assert!(check_resolved_ip(v4(169, 254, 169, 254), mode).is_err());
            // Anything in 169.254.0.0/16.
            assert!(check_resolved_ip(v4(169, 254, 1, 1), mode).is_err());
            // 0.0.0.0/8 and unspecified.
            assert!(check_resolved_ip(v4(0, 0, 0, 0), mode).is_err());
            // IPv6 unspecified, link-local, AWS IPv6 IMDS.
            assert!(check_resolved_ip(IpAddr::V6(Ipv6Addr::UNSPECIFIED), mode).is_err());
            assert!(
                check_resolved_ip(IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)), mode)
                    .is_err()
            );
            assert!(
                check_resolved_ip(
                    IpAddr::V6(Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x254)),
                    mode
                )
                .is_err()
            );
        }
    }

    #[test]
    fn ipv4_mapped_metadata_is_normalized_and_blocked() {
        // [::ffff:169.254.169.254] must be caught after normalization.
        let mapped = IpAddr::V6(Ipv4Addr::new(169, 254, 169, 254).to_ipv6_mapped());
        assert!(check_resolved_ip(mapped, ProviderSsrfMode::Relaxed).is_err());
        assert!(check_resolved_ip(mapped, ProviderSsrfMode::Strict).is_err());
    }

    #[test]
    fn private_ranges_blocked_only_in_strict() {
        let privates = [
            v4(127, 0, 0, 1),  // loopback
            v4(10, 1, 2, 3),   // 10/8
            v4(172, 16, 0, 1), // 172.16/12
            v4(172, 31, 255, 1),
            v4(192, 168, 1, 1), // 192.168/16
            v4(100, 64, 0, 1),  // CGNAT
        ];
        for ip in privates {
            assert!(check_resolved_ip(ip, ProviderSsrfMode::Strict).is_err());
            // Relaxed permits local gateways.
            assert!(check_resolved_ip(ip, ProviderSsrfMode::Relaxed).is_ok());
        }
        // IPv6 loopback + ULA.
        assert!(
            check_resolved_ip(IpAddr::V6(Ipv6Addr::LOCALHOST), ProviderSsrfMode::Strict).is_err()
        );
        assert!(
            check_resolved_ip(
                IpAddr::V6(Ipv6Addr::new(0xfd12, 0, 0, 0, 0, 0, 0, 1)),
                ProviderSsrfMode::Strict
            )
            .is_err()
        );
        // But ULA (other than the carved-out IMDS addr) is allowed under Relaxed.
        assert!(
            check_resolved_ip(
                IpAddr::V6(Ipv6Addr::new(0xfd12, 0, 0, 0, 0, 0, 0, 1)),
                ProviderSsrfMode::Relaxed
            )
            .is_ok()
        );
    }

    #[test]
    fn public_addresses_allowed_everywhere() {
        for mode in [ProviderSsrfMode::Strict, ProviderSsrfMode::Relaxed] {
            assert!(check_resolved_ip(v4(1, 1, 1, 1), mode).is_ok());
            assert!(check_resolved_ip(v4(8, 8, 8, 8), mode).is_ok());
            assert!(
                check_resolved_ip(
                    IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0, 0, 0, 0, 0, 0x1111)),
                    mode
                )
                .is_ok()
            );
        }
    }

    #[test]
    fn classify_address_three_states() {
        // Metadata floor → AlwaysBlock (both IP families).
        assert_eq!(
            classify_address(v4(169, 254, 169, 254)),
            AddressClass::AlwaysBlock
        );
        assert_eq!(classify_address(v4(0, 0, 0, 0)), AddressClass::AlwaysBlock);
        assert_eq!(
            classify_address(IpAddr::V6(Ipv6Addr::new(
                0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x254
            ))),
            AddressClass::AlwaysBlock
        );
        // IPv4-mapped metadata is normalized before classification.
        assert_eq!(
            classify_address(IpAddr::V6(
                Ipv4Addr::new(169, 254, 169, 254).to_ipv6_mapped()
            )),
            AddressClass::AlwaysBlock
        );
        // Private / loopback / CGNAT / ULA → PrivateExempt.
        for ip in [
            v4(127, 0, 0, 1),
            v4(10, 1, 2, 3),
            v4(192, 168, 1, 1),
            v4(100, 64, 0, 1),
        ] {
            assert_eq!(classify_address(ip), AddressClass::PrivateExempt);
        }
        assert_eq!(
            classify_address(IpAddr::V6(Ipv6Addr::LOCALHOST)),
            AddressClass::PrivateExempt
        );
        // Public → TlsRequired.
        assert_eq!(classify_address(v4(1, 1, 1, 1)), AddressClass::TlsRequired);
        assert_eq!(
            classify_address(IpAddr::V6(Ipv6Addr::new(
                0x2606, 0x4700, 0, 0, 0, 0, 0, 0x1111
            ))),
            AddressClass::TlsRequired
        );
    }

    #[test]
    fn check_transport_metadata_floor_never_reachable() {
        // AlwaysBlock ignores both `allow_private` and `enforce_public_tls`: every
        // combination rejects, and always as BlockedAddress (never InsecureTransport).
        for &allow_private in &[true, false] {
            for &scheme_is_tls in &[true, false] {
                for &enforce in &[true, false] {
                    assert_eq!(
                        check_transport(
                            v4(169, 254, 169, 254),
                            allow_private,
                            scheme_is_tls,
                            enforce
                        ),
                        Err(SsrfError::BlockedAddress)
                    );
                }
            }
        }
    }

    #[test]
    fn check_transport_private_gated_by_allow_private_only() {
        // Private targets: reachable over plaintext when allowed, never subject to
        // the TLS switch (a LAN gateway on http is fine).
        assert!(check_transport(v4(192, 168, 1, 10), true, false, true).is_ok());
        assert!(check_transport(v4(192, 168, 1, 10), true, false, false).is_ok());
        // When private is disallowed (Strict-style), blocked regardless of scheme.
        assert_eq!(
            check_transport(v4(192, 168, 1, 10), false, true, false),
            Err(SsrfError::BlockedAddress)
        );
    }

    #[test]
    fn check_transport_public_plaintext_gated_by_switch() {
        let public = v4(1, 1, 1, 1);
        // TLS scheme: always allowed, switch on or off.
        assert!(check_transport(public, true, true, true).is_ok());
        assert!(check_transport(public, true, true, false).is_ok());
        // Plaintext + switch on → InsecureTransport (distinct from BlockedAddress).
        assert_eq!(
            check_transport(public, true, false, true),
            Err(SsrfError::InsecureTransport)
        );
        // Plaintext + switch off → escape hatch allows it.
        assert!(check_transport(public, true, false, false).is_ok());
    }

    #[test]
    fn provider_url_scheme_structural_check() {
        // http and https are both structurally valid; any other scheme is rejected
        // regardless of mode / switch.
        for mode in [ProviderSsrfMode::Strict, ProviderSsrfMode::Relaxed] {
            assert_eq!(
                check_provider_url("file:///etc/passwd", mode, true),
                Err(SsrfError::SchemeNotAllowed)
            );
            assert_eq!(
                check_provider_url("gopher://127.0.0.1/", mode, true),
                Err(SsrfError::SchemeNotAllowed)
            );
        }
    }

    #[test]
    fn provider_url_public_plaintext_gated_by_switch_not_mode() {
        // A public http provider is saveable when enforcement is off, WITHOUT
        // switching to Relaxed (which would also open private targets) — the two
        // switches are orthogonal.
        assert_eq!(
            check_provider_url("http://203.0.113.5/v1", ProviderSsrfMode::Strict, true),
            Err(SsrfError::InsecureTransport)
        );
        assert!(
            check_provider_url("http://203.0.113.5/v1", ProviderSsrfMode::Strict, false).is_ok()
        );
        // https public is always fine under either mode.
        assert!(
            check_provider_url("https://203.0.113.5/v1", ProviderSsrfMode::Strict, true).is_ok()
        );
        // A public *domain* over http is deferred at write time (Ok); the
        // connect-time resolver enforces TLS once the address is known.
        assert!(
            check_provider_url("http://api.example.com/v1", ProviderSsrfMode::Strict, true).is_ok()
        );
    }

    #[test]
    fn provider_url_private_reachability_gated_by_mode_not_switch() {
        // Strict blocks a private literal, Relaxed allows it — independent of the
        // TLS switch, and no TLS is imposed on a private target.
        assert_eq!(
            check_provider_url("http://192.168.1.10/v1", ProviderSsrfMode::Strict, true),
            Err(SsrfError::BlockedAddress)
        );
        assert!(
            check_provider_url("http://192.168.1.10/v1", ProviderSsrfMode::Relaxed, true).is_ok()
        );
        assert!(
            check_provider_url("http://localhost:11434/v1", ProviderSsrfMode::Relaxed, true)
                .is_ok()
        );
    }

    #[test]
    fn provider_url_metadata_floor_always_blocked() {
        // The metadata / link-local floor is blocked under any mode, scheme, or switch.
        for mode in [ProviderSsrfMode::Strict, ProviderSsrfMode::Relaxed] {
            for enforce in [true, false] {
                assert_eq!(
                    check_provider_url("http://169.254.169.254/latest/meta-data/", mode, enforce),
                    Err(SsrfError::BlockedAddress)
                );
                assert_eq!(
                    check_provider_url("https://[fd00:ec2::254]/", mode, enforce),
                    Err(SsrfError::BlockedAddress)
                );
            }
        }
    }

    #[test]
    fn provider_url_domain_host_deferred_to_connect_time() {
        // A registered name passes write-time (scheme ok); the connect-time IP check
        // is where rebinding to an internal address is caught.
        assert!(
            check_provider_url("https://api.openai.com/v1", ProviderSsrfMode::Strict, true).is_ok()
        );
        assert!(
            check_provider_url(
                "http://my-gateway.local/v1",
                ProviderSsrfMode::Relaxed,
                true
            )
            .is_ok()
        );
    }

    #[test]
    fn invalid_url_rejected_when_validating() {
        assert_eq!(
            check_provider_url("definitely not a url", ProviderSsrfMode::Strict, true),
            Err(SsrfError::InvalidUrl)
        );
    }

    #[test]
    fn host_as_ip_literal_parses_literals_and_rejects_names() {
        assert_eq!(host_as_ip_literal("203.0.113.5"), Some(v4(203, 0, 113, 5)));
        assert_eq!(
            host_as_ip_literal("[::1]"),
            Some(IpAddr::V6(Ipv6Addr::LOCALHOST))
        );
        assert_eq!(
            host_as_ip_literal("::1"),
            Some(IpAddr::V6(Ipv6Addr::LOCALHOST))
        );
        assert_eq!(host_as_ip_literal("api.openai.com"), None);
        assert_eq!(host_as_ip_literal("localhost"), None);
    }

    #[test]
    fn check_transport_for_host_guards_ip_literals() {
        // Public IP literal over plaintext with enforcement on → refused (this is
        // the bypass the actix-tls short-circuit would otherwise open).
        assert_eq!(
            check_transport_for_host("203.0.113.5", true, false, true),
            Err(SsrfError::InsecureTransport)
        );
        // Same literal over TLS → allowed.
        assert!(check_transport_for_host("203.0.113.5", true, true, true).is_ok());
        // Same literal over plaintext with enforcement off → allowed (escape hatch).
        assert!(check_transport_for_host("203.0.113.5", true, false, false).is_ok());
        // Metadata floor literal → always blocked, ignoring both switches and scheme.
        for (tls, enforce) in [(true, true), (false, false), (true, false)] {
            assert_eq!(
                check_transport_for_host("169.254.169.254", true, tls, enforce),
                Err(SsrfError::BlockedAddress)
            );
        }
        // IPv4-mapped IPv6 metadata literal cannot slip past via the bracket form.
        assert_eq!(
            check_transport_for_host("[::ffff:169.254.169.254]", true, true, true),
            Err(SsrfError::BlockedAddress)
        );
        // Private literal over plaintext → allowed when allow_private; blocked when not.
        assert!(check_transport_for_host("10.0.0.5", true, false, true).is_ok());
        assert_eq!(
            check_transport_for_host("10.0.0.5", false, false, true),
            Err(SsrfError::BlockedAddress)
        );
    }

    #[test]
    fn check_transport_for_host_defers_domains() {
        // A domain host is never judged here (deferred to the connect-time resolver),
        // even a public plaintext one under enforcement.
        assert!(check_transport_for_host("api.openai.com", true, false, true).is_ok());
        assert!(check_transport_for_host("localhost", true, false, true).is_ok());
    }

    #[test]
    fn check_transport_for_url_extracts_host_and_guards() {
        // Public IP-literal URL over plaintext under enforcement → refused.
        assert_eq!(
            check_transport_for_url("http://203.0.113.5:8080/v1", true, false, true),
            Err(SsrfError::InsecureTransport)
        );
        // Metadata literal URL → always blocked (ws scheme, TLS on, enforcement off).
        assert_eq!(
            check_transport_for_url("wss://169.254.169.254/api", true, true, false),
            Err(SsrfError::BlockedAddress)
        );
        // Domain URL → deferred (Ok) regardless of plaintext + enforcement.
        assert!(check_transport_for_url("http://api.openai.com/v1", true, false, true).is_ok());
        // Unparseable / hostless → deferred (Ok), left to the dial to fail.
        assert!(check_transport_for_url("not a url", true, false, true).is_ok());
    }
}
